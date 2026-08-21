/* Embeds rtl_433 as a library: drives its demodulator from samples we already
 * have, and turns each decode into a flat key/value list for Rust.
 *
 * The vendored tree is used unmodified, so this file is written against the
 * same public surface rtl_433's own Android port uses: r_api.h for the config
 * and protocol registry, r_flow.h to push samples through the demod chain, and
 * a data_output_t of our own on cfg->output_handler to catch the results.
 */

#include "shim.h"

#include <stdlib.h>
#include <string.h>

#include "data.h"
#include "list.h"
#include "logger.h"
#include "r_api.h"
#include "r_flow.h"
#include "r_private.h"
#include "r_version.h"
#include "rtl_433.h"
#include "rtl_433_devices.h"

/* Samples per push. rtl_433 sizes its own scratch buffers off the length it is
 * handed, so this is kept well inside the buffer length its SDR paths use
 * rather than pushing a whole engine block of unknown size in one call. */
#define SDRX_CHUNK_SAMPLES 16384

/* Upper bound on the key/value pairs one decode can produce. The widest
 * built-in decoders report on the order of twenty fields; the meta fields add
 * five more. Anything past this is dropped rather than grown into, because the
 * array is a stack frame under a callback that must not allocate. */
#define SDRX_MAX_KV 64

/* Our sink on cfg->output_handler. data_output_t must be first: rtl_433 hands
 * these back as data_output_t* and we cast them straight back. */
typedef struct {
    data_output_t base;
    sdrx_rtl433  *owner;
} sdrx_output_t;

struct sdrx_rtl433 {
    r_cfg_t             *cfg;
    sdrx_output_t       *out;
    sdrx_rtl433_event_cb cb;
    void                *user;
    int                  events; /* incremented per decode, read back per feed */
    int16_t             *scratch; /* SDRX_CHUNK_SAMPLES * 2 shorts, for const-correctness */
};

/* ---- event walking ---- */

/* Append one scalar field. Returns the new count. */
static int sdrx_push_kv(sdrx_rtl433_kv *kv, int n, data_t *d)
{
    if (n >= SDRX_MAX_KV || !d->key) {
        return n;
    }
    kv[n].key   = d->key;
    kv[n].v_int = 0;
    kv[n].v_dbl = 0.0;
    kv[n].v_str = NULL;

    switch (d->type) {
    case DATA_INT:
        kv[n].type  = SDRX_RTL433_KV_INT;
        kv[n].v_int = d->value.v_int;
        break;
    case DATA_DOUBLE:
        kv[n].type  = SDRX_RTL433_KV_DOUBLE;
        kv[n].v_dbl = d->value.v_dbl;
        break;
    case DATA_STRING:
        kv[n].type = SDRX_RTL433_KV_STRING;
        kv[n].v_str = (const char *)d->value.v_ptr;
        if (!kv[n].v_str) {
            return n;
        }
        break;
    default:
        return n;
    }
    return n + 1;
}

/* Flatten one data_t list and hand it to Rust.
 *
 * Mostly a flat walk, with one level of descent into arrays. That level is not
 * optional: a flex decoder written without `unique` — a fifth of the specs
 * upstream ships — reports its getters inside a "rows" array of nested records
 * rather than at the top level, and a device whose readings all sit one level
 * down would otherwise arrive as an empty row.
 *
 * Only the first row is taken. The rows of one package are repeats of the same
 * transmission, so the later ones say nothing new.
 */
static void R_API_CALLCONV sdrx_output_print(data_output_t *output, data_t *data)
{
    sdrx_output_t *self = (sdrx_output_t *)output;
    sdrx_rtl433   *h    = self->owner;
    if (!h || !h->cb) {
        return;
    }

    sdrx_rtl433_kv kv[SDRX_MAX_KV];
    int            n = 0;

    for (data_t *d = data; d && n < SDRX_MAX_KV; d = d->next) {
        if (!d->key) {
            continue;
        }

        if (d->type == DATA_ARRAY) {
            data_array_t *arr = (data_array_t *)d->value.v_ptr;
            if (!arr || arr->num_values <= 0 || !arr->values) {
                continue;
            }
            if (arr->type == DATA_DATA) {
                /* "rows": flatten the first record's fields up to this level. */
                data_t **rows = (data_t **)arr->values;
                for (data_t *r = rows[0]; r && n < SDRX_MAX_KV; r = r->next) {
                    n = sdrx_push_kv(kv, n, r);
                }
            }
            else if (arr->type == DATA_STRING) {
                /* "codes": the first is the frame as {bits}hex. */
                char **strs = (char **)arr->values;
                if (strs[0] && n < SDRX_MAX_KV) {
                    kv[n].key   = d->key;
                    kv[n].type  = SDRX_RTL433_KV_STRING;
                    kv[n].v_int = 0;
                    kv[n].v_dbl = 0.0;
                    kv[n].v_str = strs[0];
                    n++;
                }
            }
            continue;
        }

        n = sdrx_push_kv(kv, n, d);
    }

    if (n > 0) {
        h->events++;
        h->cb(h->user, kv, n);
    }
}

/* rtl_433 frees our output on r_free_cfg. Nothing of ours is separately owned
 * except the struct itself. */
static void R_API_CALLCONV sdrx_output_free(data_output_t *output)
{
    free(output);
}

/* ---- lifecycle ---- */

sdrx_rtl433 *sdrx_rtl433_create(uint32_t samp_rate, uint32_t center_hz,
                               sdrx_rtl433_event_cb cb, void *user)
{
    sdrx_rtl433 *h = calloc(1, sizeof(*h));
    if (!h) {
        return NULL;
    }

    h->cb   = cb;
    h->user = user;

    h->scratch = calloc(SDRX_CHUNK_SAMPLES * 2, sizeof(int16_t));
    if (!h->scratch) {
        free(h);
        return NULL;
    }

    h->cfg = r_create_cfg();
    if (!h->cfg) {
        free(h->scratch);
        free(h);
        return NULL;
    }

    h->cfg->samp_rate        = samp_rate;
    h->cfg->center_frequency = center_hz;
    /* The "-M level" equivalent: appends freq/rssi/snr/noise to every decode.
     * The frequency is what puts a decode on the waterfall, so it is not
     * optional here the way it is on the command line. */
    h->cfg->report_meta = 1;

    /* rtl_433's own log level, for diagnosing a band that is not decoding.
     * Off unless asked for; 4 makes the auto-level tracker report what it
     * thinks the noise floor is, which is the first thing worth knowing. The
     * messages reach `tracing` through the handler installed from Rust. */
    {
        const char *v = getenv("SDROXIDE_RTL433_VERBOSITY");
        if (v) {
            h->cfg->verbosity = atoi(v);
        }
    }

    /* CS16: four bytes per complex sample. rtl_433 has a native magnitude and
     * FM path for it, so nothing converts on the way in. */
    h->cfg->demod->sample_size     = 4;
    h->cfg->demod->enable_FM_demod = 1;

    /* Track the noise floor and move the detection threshold down to meet it.
     *
     * NOT optional here, and the reason is the difference between rtl_433's
     * usual input and ours. An RTL-SDR sets its own gain, so its samples arrive
     * near full scale and rtl_433's default minimum detection level of about
     * -12 dBFS is below everything worth hearing. What arrives here is a
     * decimated window from a receiver with no such gain control — on an RX-888
     * at 868 MHz a sensor burst lands somewhere around -35 to -65 dBFS, i.e.
     * entirely beneath that default. Without this the pulse detector never
     * opens and *nothing at all* decodes, which is not a subtle failure but it
     * is a silent one.
     *
     * It is `-Y autolevel` on the command line, and like `raw_handler` above it
     * is set by rtl_433's own main() rather than by r_create_cfg(), so an
     * embedder has to know to ask for it. */
    h->cfg->demod->auto_level = 1.0f;

    h->out = calloc(1, sizeof(*h->out));
    if (!h->out) {
        r_free_cfg(h->cfg);
        free(h->scratch);
        free(h);
        return NULL;
    }
    h->out->base.output_print = sdrx_output_print;
    h->out->base.output_free  = sdrx_output_free;
    h->out->owner             = h;
    list_push(&h->cfg->output_handler, h->out);

    reset_sdr_flow(h->cfg);
    return h;
}

void sdrx_rtl433_destroy(sdrx_rtl433 *h)
{
    if (!h) {
        return;
    }
    /* Stop the callback before anything is torn down: r_free_cfg walks the
     * decoders and could still surface a held package. */
    h->cb = NULL;
    if (h->cfg) {
        r_free_cfg(h->cfg); /* frees our output via sdrx_output_free */
    }
    free(h->scratch);
    free(h);
}

int sdrx_rtl433_register_defaults(sdrx_rtl433 *h)
{
    if (!h) {
        return 0;
    }
    /* 0: skip the decoders upstream ships disabled. They are the noisy and
     * ambiguous ones, and enabling them wholesale fills a device table with
     * phantoms. */
    register_all_protocols(h->cfg, 0);
    return (int)h->cfg->demod->r_devs.len;
}

int sdrx_rtl433_register_flex(sdrx_rtl433 *h, const char *spec)
{
    if (!h || !spec) {
        return 0;
    }
    /* register_protocol -> flex_decoder.create_fn -> flex_create_device, which
     * calls exit() on anything it dislikes. The caller has already run the spec
     * past the validator that mirrors those checks. */
    register_protocol(h->cfg, &flex_decoder, (char *)spec);
    return (int)h->cfg->demod->r_devs.len;
}

void sdrx_rtl433_set_center_freq(sdrx_rtl433 *h, uint32_t hz)
{
    if (!h) {
        return;
    }
    h->cfg->center_frequency = hz;
}

/* Copy the config's demod parameters onto the demod itself.
 *
 * rtl_433 does this on every SDR callback, in sdr_handler() — which lives in
 * rtl_433.c beside main(), the one file the r_433 library leaves out. So an
 * embedder has to do it, and `raw_handler` in particular is not optional:
 * push_sdr_flow() dereferences it unconditionally, and a config that has never
 * been through sdr_handler() has it still NULL. */
static void sdrx_sync_demod_params(r_cfg_t *cfg)
{
    /* Which FSK discriminator to use. Chosen from the tuned frequency because
     * the newer detector is the one that works on the higher bands, which is
     * where the FSK devices are. */
    unsigned fpdm = cfg->fsk_pulse_detect_mode;
    if (cfg->fsk_pulse_detect_mode == FSK_PULSE_DETECT_AUTO) {
        fpdm = cfg->center_frequency > FSK_PULSE_DETECTOR_LIMIT ? FSK_PULSE_DETECT_NEW
                                                                : FSK_PULSE_DETECT_OLD;
    }

    cfg->demod->raw_handler           = &cfg->raw_handler;
    cfg->demod->fsk_pulse_detect_mode = fpdm;
    cfg->demod->report_noise          = cfg->report_noise;
    cfg->demod->verbosity             = cfg->verbosity;
    cfg->demod->raw_mode              = cfg->raw_mode;
    cfg->demod->grab_mode             = cfg->grab_mode;
}

int sdrx_rtl433_feed_cs16(sdrx_rtl433 *h, const int16_t *iq, uint32_t n_complex)
{
    if (!h || !iq || n_complex == 0) {
        return 0;
    }

    h->events = 0;
    sdrx_sync_demod_params(h->cfg);

    /* A changed rate or centre means the stream is no longer the one the pulse
     * detector has been accumulating; finish that package before the new samples
     * arrive rather than splicing the two together. */
    if (h->cfg->demod->center_frequency != h->cfg->center_frequency
            || h->cfg->demod->samp_rate != h->cfg->samp_rate) {
        flush_sdr_flow(h->cfg);
    }
    h->cfg->demod->center_frequency = h->cfg->center_frequency;
    h->cfg->demod->samp_rate        = h->cfg->samp_rate;

    uint32_t done = 0;
    while (done < n_complex) {
        uint32_t take = n_complex - done;
        if (take > SDRX_CHUNK_SAMPLES) {
            take = SDRX_CHUNK_SAMPLES;
        }

        /* push_sdr_flow takes a mutable buffer; copy rather than cast away the
         * caller's const, since rtl_433's conversion paths write through it. */
        memcpy(h->scratch, iq + (size_t)done * 2, (size_t)take * 2 * sizeof(int16_t));

        push_sdr_flow(h->cfg, (unsigned char *)h->scratch, take * 2 * (uint32_t)sizeof(int16_t));
        done += take;
    }

    return h->events;
}

void sdrx_rtl433_reset(sdrx_rtl433 *h)
{
    if (!h) {
        return;
    }
    reset_sdr_flow(h->cfg);
}

int sdrx_rtl433_flush(sdrx_rtl433 *h)
{
    if (!h) {
        return 0;
    }
    h->events = 0;
    sdrx_sync_demod_params(h->cfg);
    flush_sdr_flow(h->cfg);
    return h->events;
}

/* ---- logging ---- */

static sdrx_rtl433_log_cb sdrx_log_cb   = NULL;
static void              *sdrx_log_user = NULL;

static void R_API_CALLCONV sdrx_log_trampoline(log_level_t level, char const *src,
                                              char const *msg, void *userdata)
{
    (void)userdata;
    if (sdrx_log_cb) {
        sdrx_log_cb(sdrx_log_user, (int)level, src ? src : "", msg ? msg : "");
    }
}

void sdrx_rtl433_set_log_handler(sdrx_rtl433_log_cb cb, void *user)
{
    sdrx_log_cb   = cb;
    sdrx_log_user = user;
    r_logger_set_log_handler(cb ? sdrx_log_trampoline : NULL, NULL);
}

const char *sdrx_rtl433_version(void)
{
    return version_string();
}
