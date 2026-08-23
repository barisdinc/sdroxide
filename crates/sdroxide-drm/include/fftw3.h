/* The slice of FFTW's interface that Dream's matlib uses, over the transform in
 * `fftw_compat.c`. Dream reaches this by `#include <fftw3.h>`, so putting this
 * directory on the include path is all it takes — no change to Dream's source.
 *
 * FFTW itself is a build-time dependency sdroxide does not otherwise have, on
 * three platforms; six entry points and one buffer convention are cheaper to
 * carry than that. Real FFTW would work equally well: the declarations here are
 * a subset of its own, and `tests/fftw_vectors.rs` checks the results against
 * the real library when it is installed. */
#ifndef SDRX_FFTW3_COMPAT_H
#define SDRX_FFTW3_COMPAT_H

#ifdef __cplusplus
extern "C" {
#endif

typedef double fftw_complex[2];
typedef struct sdrx_fftw_plan_s* fftw_plan;

/* The sign of the exponent, as in FFTW. */
#define FFTW_FORWARD  (-1)
#define FFTW_BACKWARD (+1)

/* Planner flags. Nothing here plans adaptively, so these only have to exist. */
#define FFTW_MEASURE        (0U)
#define FFTW_DESTROY_INPUT  (1U << 0)
#define FFTW_UNALIGNED      (1U << 1)
#define FFTW_ESTIMATE       (1U << 6)

typedef enum {
    FFTW_R2HC = 0,   /* real in, halfcomplex out */
    FFTW_HC2R = 1    /* halfcomplex in, real out */
} fftw_r2r_kind;

/* As in FFTW, a plan binds its input and output buffers at planning time and
   `fftw_execute` transforms whatever they hold at the time of the call. Both
   transforms are unnormalised in both directions. */
fftw_plan fftw_plan_dft_1d(int n, fftw_complex* in, fftw_complex* out,
                           int sign, unsigned flags);
fftw_plan fftw_plan_r2r_1d(int n, double* in, double* out,
                           fftw_r2r_kind kind, unsigned flags);
void fftw_execute(const fftw_plan plan);
void fftw_destroy_plan(fftw_plan plan);

#ifdef __cplusplus
}
#endif

#endif
