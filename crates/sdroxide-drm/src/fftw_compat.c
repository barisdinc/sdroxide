/* The FFTW entry points declared in `include/fftw3.h`.
 *
 * A radix-2 Cooley-Tukey transform covers the power-of-two lengths, and
 * Bluestein's chirp-z covers everything else by turning a length-n transform
 * into a cyclic convolution of the next power of two at or above 2n-1. That is
 * a little more work than a mixed-radix transform would be for Dream's 1152-
 * and 704-point symbols, and far less code to get wrong. At one FFT per OFDM
 * symbol — 21 ms of audio in mode B — the difference does not show up.
 *
 * Both directions are unnormalised, as in FFTW: a forward followed by a
 * backward transform multiplies by n. */

#include "fftw3.h"

#include <math.h>
#include <stdlib.h>
#include <string.h>

#ifndef M_PI
# define M_PI 3.14159265358979323846
#endif

typedef enum { PLAN_DFT, PLAN_R2HC, PLAN_HC2R } plan_kind;

struct sdrx_fftw_plan_s {
    plan_kind kind;
    int       n;        /* the transform length the caller asked for */
    int       inverse;  /* FFTW_BACKWARD, or the HC2R direction */
    void*     in;
    void*     out;

    int       m;        /* power of two the radix-2 engine runs at */
    double*   tw;       /* m/2 forward twiddles, interleaved */
    double*   work;     /* 2*m */

    /* Only when n is not itself a power of two. */
    double*   chirp;    /* 2*n */
    double*   bfft;     /* 2*m, the transformed convolution kernel */
    double*   bwork;    /* 2*m */
};

static int next_pow2(int v)
{
    int p = 1;
    while (p < v)
        p <<= 1;
    return p;
}

static int is_pow2(int v) { return v > 0 && (v & (v - 1)) == 0; }

/* In-place, on interleaved re/im. `tw` holds exp(-2*pi*i*j/m) for j < m/2. */
static void fft_pow2(double* a, int m, const double* tw, int inverse)
{
    for (int i = 1, j = 0; i < m; i++) {
        int bit = m >> 1;
        for (; j & bit; bit >>= 1)
            j ^= bit;
        j ^= bit;
        if (i < j) {
            double tr = a[2 * i], ti = a[2 * i + 1];
            a[2 * i] = a[2 * j];
            a[2 * i + 1] = a[2 * j + 1];
            a[2 * j] = tr;
            a[2 * j + 1] = ti;
        }
    }
    for (int len = 2; len <= m; len <<= 1) {
        const int half = len >> 1;
        const int step = m / len;
        for (int i = 0; i < m; i += len) {
            for (int j = 0; j < half; j++) {
                const double wr = tw[2 * (j * step)];
                const double wi = inverse ? -tw[2 * (j * step) + 1]
                                          : tw[2 * (j * step) + 1];
                double* p = a + 2 * (i + j);
                double* q = a + 2 * (i + j + half);
                const double xr = q[0] * wr - q[1] * wi;
                const double xi = q[0] * wi + q[1] * wr;
                q[0] = p[0] - xr;
                q[1] = p[1] - xi;
                p[0] += xr;
                p[1] += xi;
            }
        }
    }
}

/* Transform `plan->work` in place as a length-n DFT with the given direction. */
static void dft_work(const fftw_plan plan, int inverse)
{
    const int n = plan->n;

    if (plan->chirp == NULL) {
        fft_pow2(plan->work, plan->m, plan->tw, inverse);
        return;
    }

    /* X[k] = c[k] * sum_j (x[j] * c[j]) * conj(c[k-j]),  c[j] = e^(s*i*pi*j^2/n).
       The sum is a linear convolution, so it is evaluated as a cyclic one of
       length m >= 2n-1. `bfft` already holds the transform of conj(c),
       mirrored, for the forward direction; the backward direction is its
       conjugate, which is applied while multiplying. */
    const int m = plan->m;
    double* a = plan->bwork;
    memset(a, 0, sizeof(double) * 2 * (size_t)m);
    for (int j = 0; j < n; j++) {
        const double xr = plan->work[2 * j], xi = plan->work[2 * j + 1];
        const double cr = plan->chirp[2 * j];
        const double ci = inverse ? plan->chirp[2 * j + 1] : -plan->chirp[2 * j + 1];
        a[2 * j] = xr * cr - xi * ci;
        a[2 * j + 1] = xr * ci + xi * cr;
    }

    fft_pow2(a, m, plan->tw, 0);
    for (int k = 0; k < m; k++) {
        const double ar = a[2 * k], ai = a[2 * k + 1];
        const double br = plan->bfft[2 * k];
        const double bi = inverse ? plan->bfft[2 * k + 1] : -plan->bfft[2 * k + 1];
        a[2 * k] = ar * br - ai * bi;
        a[2 * k + 1] = ar * bi + ai * br;
    }
    fft_pow2(a, m, plan->tw, 1);

    const double scale = 1.0 / m;
    for (int k = 0; k < n; k++) {
        const double cr = plan->chirp[2 * k];
        const double ci = inverse ? plan->chirp[2 * k + 1] : -plan->chirp[2 * k + 1];
        const double ar = a[2 * k] * scale, ai = a[2 * k + 1] * scale;
        plan->work[2 * k] = ar * cr - ai * ci;
        plan->work[2 * k + 1] = ar * ci + ai * cr;
    }
}

static void plan_free(fftw_plan p)
{
    if (p == NULL)
        return;
    free(p->tw);
    free(p->work);
    free(p->chirp);
    free(p->bfft);
    free(p->bwork);
    free(p);
}

/* Everything but the buffer binding and the transform kind. */
static fftw_plan plan_alloc(int n)
{
    if (n < 1)
        return NULL;

    fftw_plan p = (fftw_plan)calloc(1, sizeof(*p));
    if (p == NULL)
        return NULL;
    p->n = n;
    p->m = is_pow2(n) ? n : next_pow2(2 * n - 1);

    /* m/2 complex twiddles, and never fewer than one. */
    p->tw = (double*)malloc(sizeof(double) * (p->m > 1 ? (size_t)p->m : 2u));
    p->work = (double*)malloc(sizeof(double) * 2 * (size_t)n);
    if (p->tw == NULL || p->work == NULL) {
        plan_free(p);
        return NULL;
    }
    for (int j = 0; j < p->m / 2; j++) {
        const double ang = -2.0 * M_PI * j / p->m;
        p->tw[2 * j] = cos(ang);
        p->tw[2 * j + 1] = sin(ang);
    }
    /* A length-1 transform has no twiddles and no butterflies. */
    if (p->m == 1)
        p->tw[0] = 1.0, p->tw[1] = 0.0;

    if (!is_pow2(n)) {
        p->chirp = (double*)malloc(sizeof(double) * 2 * (size_t)n);
        p->bfft = (double*)malloc(sizeof(double) * 2 * (size_t)p->m);
        p->bwork = (double*)malloc(sizeof(double) * 2 * (size_t)p->m);
        if (p->chirp == NULL || p->bfft == NULL || p->bwork == NULL) {
            plan_free(p);
            return NULL;
        }
        /* j*j grows past what a double holds exactly for large n; reducing it
           modulo 2n first keeps the angle small and the phase exact. */
        for (int j = 0; j < n; j++) {
            const long long jj = ((long long)j * (long long)j) % (2LL * n);
            const double ang = M_PI * (double)jj / (double)n;
            p->chirp[2 * j] = cos(ang);
            p->chirp[2 * j + 1] = sin(ang);
        }
        memset(p->bfft, 0, sizeof(double) * 2 * (size_t)p->m);
        p->bfft[0] = p->chirp[0];
        p->bfft[1] = -p->chirp[1];
        for (int j = 1; j < n; j++) {
            p->bfft[2 * j] = p->chirp[2 * j];
            p->bfft[2 * j + 1] = -p->chirp[2 * j + 1];
            p->bfft[2 * (p->m - j)] = p->chirp[2 * j];
            p->bfft[2 * (p->m - j) + 1] = -p->chirp[2 * j + 1];
        }
        fft_pow2(p->bfft, p->m, p->tw, 0);
    }
    return p;
}

fftw_plan fftw_plan_dft_1d(int n, fftw_complex* in, fftw_complex* out,
                           int sign, unsigned flags)
{
    (void)flags;
    fftw_plan p = plan_alloc(n);
    if (p == NULL)
        return NULL;
    p->kind = PLAN_DFT;
    p->inverse = sign == FFTW_BACKWARD;
    p->in = in;
    p->out = out;
    return p;
}

fftw_plan fftw_plan_r2r_1d(int n, double* in, double* out,
                           fftw_r2r_kind kind, unsigned flags)
{
    (void)flags;
    fftw_plan p = plan_alloc(n);
    if (p == NULL)
        return NULL;
    p->kind = kind == FFTW_HC2R ? PLAN_HC2R : PLAN_R2HC;
    p->inverse = kind == FFTW_HC2R;
    p->in = in;
    p->out = out;
    return p;
}

void fftw_execute(const fftw_plan plan)
{
    if (plan == NULL)
        return;

    const int n = plan->n;

    if (plan->kind == PLAN_DFT) {
        const double* in = (const double*)plan->in;
        double* out = (double*)plan->out;
        memcpy(plan->work, in, sizeof(double) * 2 * (size_t)n);
        dft_work(plan, plan->inverse);
        memcpy(out, plan->work, sizeof(double) * 2 * (size_t)n);
        return;
    }

    /* FFTW's halfcomplex layout: r0, r1..r(n/2), i((n-1)/2)..i1 — the real
       parts counting up from DC and the imaginary parts counting back down
       from the top, with no slot for the two that are always zero. */
    if (plan->kind == PLAN_R2HC) {
        const double* in = (const double*)plan->in;
        double* out = (double*)plan->out;
        for (int j = 0; j < n; j++) {
            plan->work[2 * j] = in[j];
            plan->work[2 * j + 1] = 0.0;
        }
        dft_work(plan, 0);
        out[0] = plan->work[0];
        for (int k = 1; k <= n / 2; k++)
            out[k] = plan->work[2 * k];
        for (int k = 1; k <= (n - 1) / 2; k++)
            out[n - k] = plan->work[2 * k + 1];
        return;
    }

    {
        const double* in = (const double*)plan->in;
        double* out = (double*)plan->out;
        plan->work[0] = in[0];
        plan->work[1] = 0.0;
        for (int k = 1; k <= (n - 1) / 2; k++) {
            const double re = in[k], im = in[n - k];
            plan->work[2 * k] = re;
            plan->work[2 * k + 1] = im;
            plan->work[2 * (n - k)] = re;
            plan->work[2 * (n - k) + 1] = -im;
        }
        if (n % 2 == 0) {
            plan->work[2 * (n / 2)] = in[n / 2];
            plan->work[2 * (n / 2) + 1] = 0.0;
        }
        dft_work(plan, 1);
        for (int j = 0; j < n; j++)
            out[j] = plan->work[2 * j];
    }
}

void fftw_destroy_plan(fftw_plan plan) { plan_free(plan); }
