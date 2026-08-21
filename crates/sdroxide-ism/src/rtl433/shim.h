/* The whole C surface of the embedded rtl_433, and the only header bindgen reads.
 *
 * rtl_433's own headers are not exposed to Rust: r_cfg_t drags in mongoose, the
 * pulse detector and every output backend, none of which Rust has any business
 * naming. Everything here is plain C99 with no rtl_433 types in it.
 *
 * Threading: an instance is single-threaded, like the r_cfg_t it wraps. Create
 * it, feed it and destroy it from one thread.
 */

#ifndef SDROXIDE_RTL433_SHIM_H_
#define SDROXIDE_RTL433_SHIM_H_

#include <stdint.h>

typedef struct sdrx_rtl433 sdrx_rtl433;

/* Value types a decode event can carry. rtl_433's data_t knows more (arrays,
 * nested data), but no decoder we map uses them at the top level, so they are
 * dropped in the walker rather than represented here. */
enum {
    SDRX_RTL433_KV_INT    = 0,
    SDRX_RTL433_KV_DOUBLE = 1,
    SDRX_RTL433_KV_STRING = 2
};

/* One key/value pair of a decode event.
 *
 * `key` and `v_str` point into rtl_433's own data_t, which is freed as soon as
 * the callback returns — copy anything you keep. */
typedef struct {
    const char *key;
    int         type;   /* one of SDRX_RTL433_KV_* */
    int         v_int;
    double      v_dbl;
    const char *v_str;  /* NULL unless type is STRING */
} sdrx_rtl433_kv;

/* Fired once per decode, on the thread inside sdrx_rtl433_feed_cs16.
 * Must not throw, longjmp, or unwind: the frame below it is C. */
typedef void (*sdrx_rtl433_event_cb)(void *user, const sdrx_rtl433_kv *kv, int n);

/* rtl_433's internal logging. Process-global, like the sink it installs. */
typedef void (*sdrx_rtl433_log_cb)(void *user, int level, const char *src, const char *msg);

/* Create an instance fed at `samp_rate` and tuned at `center_hz`.
 * Returns NULL if rtl_433 could not allocate its config. */
sdrx_rtl433 *sdrx_rtl433_create(uint32_t samp_rate, uint32_t center_hz,
                               sdrx_rtl433_event_cb cb, void *user);

void sdrx_rtl433_destroy(sdrx_rtl433 *h);

/* Register every built-in decoder that is not disabled by default.
 * Returns how many are now registered. */
int sdrx_rtl433_register_defaults(sdrx_rtl433 *h);

/* Register one user "flex" decoder.
 *
 * DANGER: rtl_433 calls exit() on a malformed spec — see flex.c's usage().
 * `spec` must already have passed the Rust-side validator. Returns the new
 * decoder count. */
int sdrx_rtl433_register_flex(sdrx_rtl433 *h, const char *spec);

/* Retune. Only changes what decodes report as their frequency; the samples
 * still arrive through feed_cs16. */
void sdrx_rtl433_set_center_freq(sdrx_rtl433 *h, uint32_t hz);

/* Push interleaved signed 16-bit IQ. `n_complex` is sample pairs, not shorts.
 * Returns the number of decode events the callback fired for. */
int sdrx_rtl433_feed_cs16(sdrx_rtl433 *h, const int16_t *iq, uint32_t n_complex);

/* Drop filter and pulse-detector state. Use after a retune or a gap in the
 * stream, so a half-seen burst cannot merge into the next one. */
void sdrx_rtl433_reset(sdrx_rtl433 *h);

/* Flush a pulse package the detector is still holding. Returns events fired.
 * Only useful at end of stream, e.g. when replaying a capture. */
int sdrx_rtl433_flush(sdrx_rtl433 *h);

/* Route rtl_433's logging into the host. Process-global: the last call wins,
 * and passing NULL restores rtl_433's default (stderr). */
void sdrx_rtl433_set_log_handler(sdrx_rtl433_log_cb cb, void *user);

/* The vendored rtl_433 version string, e.g. "25.12-353-g8fa6364c". */
const char *sdrx_rtl433_version(void);

#endif /* SDROXIDE_RTL433_SHIM_H_ */
