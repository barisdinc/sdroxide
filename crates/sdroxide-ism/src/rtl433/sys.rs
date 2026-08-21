//! The raw FFI surface and a safe owner for it.
//!
//! Everything unsafe about the embedded rtl_433 is confined here: the generated
//! bindings, the callback trampoline, and the rule that an [`Instance`] belongs
//! to exactly one thread.

#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code)]

use std::ffi::{CStr, CString};

mod ffi {
    #![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code)]
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

/// One key/value pair of a decode, owned so it outlives the callback.
#[derive(Debug, Clone, PartialEq)]
pub struct Kv {
    pub key: String,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Double(f64),
    Str(String),
}

impl Value {
    /// The value as a number, whatever it arrived as.
    ///
    /// rtl_433 is inconsistent about this by design — a decoder reports whatever
    /// its device's field really is, so `humidity` is an int on one and a double
    /// on the next, and `id` is sometimes a string.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(v) => Some(*v as f64),
            Value::Double(v) => Some(*v),
            Value::Str(s) => s.trim().parse().ok(),
        }
    }

    /// The value as text, for the key/value lane of a report.
    pub fn to_display(&self) -> String {
        match self {
            Value::Int(v) => v.to_string(),
            Value::Double(v) => {
                // Trailing zeroes read as false precision in a table cell.
                let s = format!("{v:.3}");
                s.trim_end_matches('0').trim_end_matches('.').to_string()
            }
            Value::Str(s) => s.clone(),
        }
    }
}

/// What the trampoline collects into. Separate from [`Instance`] so the callback
/// can hold `&mut` to it while rtl_433 holds the instance.
#[derive(Default)]
struct Sink {
    events: Vec<Vec<Kv>>,
    /// Set if a callback panicked. The instance is not trusted afterwards.
    poisoned: bool,
}

/// Fired from C, once per decode.
///
/// Any panic is caught here: unwinding through the C frame below would be
/// undefined behaviour, and a mapping bug is not worth aborting the process for.
unsafe extern "C" fn on_event(user: *mut std::ffi::c_void, kv: *const ffi::sdrx_rtl433_kv, n: i32) {
    if user.is_null() || kv.is_null() || n <= 0 {
        return;
    }
    let sink = unsafe { &mut *(user as *mut Sink) };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let items = unsafe { std::slice::from_raw_parts(kv, n as usize) };
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            if item.key.is_null() {
                continue;
            }
            let Ok(key) = (unsafe { CStr::from_ptr(item.key) }).to_str() else { continue };
            let value = match item.type_ {
                x if x == ffi::SDRX_RTL433_KV_INT as i32 => Value::Int(item.v_int as i64),
                x if x == ffi::SDRX_RTL433_KV_DOUBLE as i32 => Value::Double(item.v_dbl),
                x if x == ffi::SDRX_RTL433_KV_STRING as i32 => {
                    if item.v_str.is_null() {
                        continue;
                    }
                    match unsafe { CStr::from_ptr(item.v_str) }.to_str() {
                        Ok(s) => Value::Str(s.to_string()),
                        Err(_) => continue,
                    }
                }
                _ => continue,
            };
            out.push(Kv { key: key.to_string(), value });
        }
        out
    }));

    match result {
        Ok(kvs) if !kvs.is_empty() => sink.events.push(kvs),
        Ok(_) => {}
        Err(_) => sink.poisoned = true,
    }
}

/// A live rtl_433 instance.
///
/// Owns its config, its decoders and the sink they report into. Single-threaded,
/// like the `r_cfg_t` underneath: it is `Send` so a worker can take it, but
/// never `Sync`.
pub struct Instance {
    handle: *mut ffi::sdrx_rtl433,
    /// Boxed so its address is stable while C holds it.
    sink: Box<Sink>,
    decoders: u32,
    flex: u32,
}

// The pointer is owned exclusively and every call goes through &mut self, so
// moving the whole instance to another thread is sound. It is deliberately not
// Sync: two threads inside push_sdr_flow at once would corrupt the demod state.
unsafe impl Send for Instance {}

impl Instance {
    /// Start rtl_433 on a stream of `samp_rate` samples per second centred on
    /// `center_hz`, with every built-in decoder that is enabled by default.
    pub fn new(samp_rate: u32, center_hz: u32) -> Option<Instance> {
        let mut sink = Box::new(Sink::default());
        let user = (&raw mut *sink) as *mut std::ffi::c_void;
        let handle = unsafe { ffi::sdrx_rtl433_create(samp_rate, center_hz, Some(on_event), user) };
        if handle.is_null() {
            return None;
        }
        let decoders = unsafe { ffi::sdrx_rtl433_register_defaults(handle) };
        Some(Instance { handle, sink, decoders: decoders.max(0) as u32, flex: 0 })
    }

    /// Add one user flex decoder.
    ///
    /// # Panics in C
    ///
    /// rtl_433 calls `exit()` on a spec it cannot parse, which would take the
    /// whole process with it. `spec` must have come through
    /// [`super::flex::validate`] first; this is not checked here because the
    /// check is the caller's contract, not a runtime guard.
    pub fn register_flex(&mut self, spec: &str) -> Result<(), String> {
        let c = CString::new(spec).map_err(|_| "spec contains a NUL byte".to_string())?;
        let count = unsafe { ffi::sdrx_rtl433_register_flex(self.handle, c.as_ptr()) };
        self.flex += 1;
        self.decoders = count.max(0) as u32;
        Ok(())
    }

    /// Push interleaved IQ. Returns the decodes it produced.
    pub fn feed(&mut self, iq: &[i16]) -> Vec<Vec<Kv>> {
        if iq.len() < 2 {
            return Vec::new();
        }
        self.sink.events.clear();
        let n = (iq.len() / 2) as u32;
        unsafe { ffi::sdrx_rtl433_feed_cs16(self.handle, iq.as_ptr(), n) };
        std::mem::take(&mut self.sink.events)
    }

    /// Push any package the pulse detector is still holding. Only meaningful at
    /// the end of a capture — a live stream simply keeps arriving.
    pub fn flush(&mut self) -> Vec<Vec<Kv>> {
        self.sink.events.clear();
        unsafe { ffi::sdrx_rtl433_flush(self.handle) };
        std::mem::take(&mut self.sink.events)
    }

    /// Retune. Changes what decodes report, not where the samples come from.
    pub fn set_center_hz(&mut self, hz: u32) {
        unsafe { ffi::sdrx_rtl433_set_center_freq(self.handle, hz) };
    }

    /// Drop filter and pulse state, so a burst cut off by a retune cannot merge
    /// into the next one.
    pub fn reset(&mut self) {
        unsafe { ffi::sdrx_rtl433_reset(self.handle) };
    }

    pub fn decoder_count(&self) -> u32 {
        self.decoders
    }

    pub fn flex_count(&self) -> u32 {
        self.flex
    }

    /// True if a callback panicked. The instance should be rebuilt.
    pub fn poisoned(&self) -> bool {
        self.sink.poisoned
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe { ffi::sdrx_rtl433_destroy(self.handle) };
    }
}

/// The vendored rtl_433's version, e.g. `"25.12-353-g8fa6364c"`.
pub fn version() -> String {
    let p = unsafe { ffi::sdrx_rtl433_version() };
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// Route rtl_433's own logging into `tracing`.
///
/// Process-global, so this is done once. rtl_433 logs decoder-level detail at
/// its higher verbosities; at the default it is quiet apart from real problems.
pub fn install_log_handler() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        ffi::sdrx_rtl433_set_log_handler(Some(log_trampoline), std::ptr::null_mut());
    });
}

unsafe extern "C" fn log_trampoline(
    _user: *mut std::ffi::c_void,
    level: i32,
    src: *const std::ffi::c_char,
    msg: *const std::ffi::c_char,
) {
    let _ = std::panic::catch_unwind(|| {
        let src = if src.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(src) }.to_string_lossy().into_owned()
        };
        let msg = if msg.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(msg) }.to_string_lossy().into_owned()
        };
        // rtl_433's levels run 0=fatal upward; anything at or below warning is
        // worth surfacing, the rest is decoder chatter.
        match level {
            0..=2 => tracing::warn!(target: "rtl_433", %src, "{msg}"),
            3..=4 => tracing::debug!(target: "rtl_433", %src, "{msg}"),
            _ => tracing::trace!(target: "rtl_433", %src, "{msg}"),
        }
    });
}
