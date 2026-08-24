//! The process-global connection to the SDRplay API service.
//!
//! The vendor API is process-global state — one `sdrplay_api_Open` serves
//! every device — so this module owns it behind one mutex. Everything that
//! touches the service's device table (enumerate, select, release) runs under
//! that mutex, which is what makes the settings dialog's Rescan safe while a
//! device is streaming: the API's own `LockDeviceApi`/`UnlockDeviceApi` pair
//! is designed for exactly that, and the mutex keeps this process from
//! interleaving its halves.

use std::sync::{Arc, Mutex, OnceLock};

use sdroxide_types::{SdrPlayDevice, SdrPlayDuoTuner, SdrPlayModel};

use crate::error::{Error, Result};
use crate::ffi;

struct ApiState {
    /// The loaded library, kept for the life of the process. Never unloaded:
    /// callback pointers handed to the service must stay valid.
    api: Option<Arc<ffi::Api>>,
    /// Whether `sdrplay_api_Open` has succeeded and not been reset.
    opened: bool,
    /// One log line per absence, not one per rescan tick.
    complained: bool,
}

fn state() -> &'static Mutex<ApiState> {
    static STATE: OnceLock<Mutex<ApiState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(ApiState { api: None, opened: false, complained: false }))
}

/// Load the library and connect to the service, both idempotent.
fn ensure_open(s: &mut ApiState) -> Result<Arc<ffi::Api>> {
    let api = match &s.api {
        Some(a) => Arc::clone(a),
        None => match ffi::Api::load() {
            Ok(a) => {
                let a = Arc::new(a);
                s.api = Some(Arc::clone(&a));
                a
            }
            Err(e) => {
                if !s.complained {
                    s.complained = true;
                    tracing::debug!("SDRplay backend unavailable: {e}");
                }
                return Err(Error::LibMissing(e));
            }
        },
    };
    if !s.opened {
        let err = unsafe { (api.open)() };
        if err != ffi::ERR_SUCCESS {
            // A present library whose Open fails means the background service
            // is not there to answer, whatever the exact code says.
            if !s.complained {
                s.complained = true;
                tracing::debug!("SDRplay service not reachable: {}", api.err_text(err));
            }
            return Err(Error::ServiceDown(api.err_text(err)));
        }
        s.opened = true;
        s.complained = false;
        let mut ver = 0.0f32;
        if unsafe { (api.api_version)(&mut ver) } == ffi::ERR_SUCCESS {
            tracing::info!("SDRplay API connected, version {ver:.2}");
            if !(3.0..4.0).contains(&ver) {
                tracing::warn!(
                    "SDRplay API version {ver:.2} is not the 3.x these bindings were written \
                     against — proceeding, but expect the service to refuse"
                );
            }
        }
    }
    Ok(api)
}

/// Drop the service connection so the next call reconnects from scratch.
///
/// Called when the service stops answering mid-session: the engine's
/// background retry keeps calling [`crate::spawn`], and without this it would
/// keep talking down a dead socket instead of dialling again.
pub(crate) fn reset() {
    let mut s = state().lock().expect("sdrplay api state poisoned");
    if s.opened
        && let Some(api) = &s.api
    {
        unsafe { (api.close)() };
    }
    s.opened = false;
}

/// Fetch the service's device table. Must be called with the state lock held;
/// takes and always releases the API's own device lock.
fn devices_locked(api: &ffi::Api) -> Result<Vec<ffi::DeviceT>> {
    let err = unsafe { (api.lock_device_api)() };
    if err != ffi::ERR_SUCCESS {
        return Err(Error::from_status(api, "LockDeviceApi", err));
    }
    let mut devs = [ffi::DeviceT::zeroed(); ffi::MAX_DEVICES];
    let mut n: u32 = 0;
    let err = unsafe { (api.get_devices)(devs.as_mut_ptr(), &mut n, ffi::MAX_DEVICES as u32) };
    unsafe { (api.unlock_device_api)() };
    if err != ffi::ERR_SUCCESS {
        return Err(Error::from_status(api, "GetDevices", err));
    }
    Ok(devs[..(n as usize).min(ffi::MAX_DEVICES)]
        .iter()
        .filter(|d| d.valid != 0)
        .copied()
        .collect())
}

/// List the RSPs the service reports, or say why that is impossible — the
/// distinction `--probe` needs between "no library", "no service" and simply
/// "no devices".
pub fn try_list() -> Result<Vec<SdrPlayDevice>> {
    let mut s = state().lock().expect("sdrplay api state poisoned");
    let api = ensure_open(&mut s)?;
    Ok(devices_locked(&api)?
        .iter()
        .map(|d| SdrPlayDevice { serial: d.serial(), hw_ver: d.hw_ver })
        .collect())
}

/// List the RSPs the service reports. Best-effort: no library, no service or
/// no devices all come back as an empty list, because for a Rescan button
/// those are the same answer.
pub fn list() -> Vec<SdrPlayDevice> {
    match try_list() {
        Ok(devs) => devs,
        Err(e) => {
            tracing::debug!("SDRplay enumeration failed: {e}");
            Vec::new()
        }
    }
}

/// Select the device `serial` names (empty = the first one found) and hand
/// its API handle over.
///
/// An RSPduo is put in single-tuner mode on the requested tuner, or — with
/// `dual` — in dual-tuner mode on both. Either way the choice is fixed here,
/// at selection time, which is why changing it costs a reopen. Dual-tuner
/// mode also fixes the ADC clock, and that number goes in the device record
/// rather than in the parameter block: `SelectDevice` is what programs it.
pub(crate) fn select(
    serial: &str,
    duo_tuner: SdrPlayDuoTuner,
    dual: bool,
) -> Result<(Arc<ffi::Api>, ffi::DeviceT)> {
    let mut s = state().lock().expect("sdrplay api state poisoned");
    let api = ensure_open(&mut s)?;

    let err = unsafe { (api.lock_device_api)() };
    if err != ffi::ERR_SUCCESS {
        return Err(Error::from_status(&api, "LockDeviceApi", err));
    }
    // From here every path must unlock.
    let result = (|| {
        let mut devs = [ffi::DeviceT::zeroed(); ffi::MAX_DEVICES];
        let mut n: u32 = 0;
        let err = unsafe { (api.get_devices)(devs.as_mut_ptr(), &mut n, ffi::MAX_DEVICES as u32) };
        if err != ffi::ERR_SUCCESS {
            return Err(Error::from_status(&api, "GetDevices", err));
        }
        let want = serial.trim();
        let mut dev = *devs[..(n as usize).min(ffi::MAX_DEVICES)]
            .iter()
            .filter(|d| d.valid != 0)
            .find(|d| want.is_empty() || d.serial() == want)
            .ok_or_else(|| {
                Error::NotFound(if want.is_empty() {
                    "no SDRplay RSP found — is one plugged in, and is the SDRplay API service \
                     running?"
                        .into()
                } else {
                    format!(
                        "no SDRplay RSP with serial {want} found — replug it, or pick another \
                         receiver in Settings → Radio"
                    )
                })
            })?;

        let model = SdrPlayModel::from_hw_ver(dev.hw_ver);
        if model == SdrPlayModel::RspDuo {
            if dual {
                dev.tuner = ffi::TUNER_BOTH;
                dev.rsp_duo_mode = ffi::RSPDUO_MODE_DUAL_TUNER;
                dev.rsp_duo_sample_freq = crate::device::DUAL_FS_HZ;
            } else {
                dev.tuner = match duo_tuner {
                    SdrPlayDuoTuner::Tuner1 => ffi::TUNER_A,
                    SdrPlayDuoTuner::Tuner2 => ffi::TUNER_B,
                };
                dev.rsp_duo_mode = ffi::RSPDUO_MODE_SINGLE_TUNER;
                // Single-tuner mode lets the ADC clock follow the sample rate.
                dev.rsp_duo_sample_freq = 0.0;
            }
        }

        let err = unsafe { (api.select_device)(&mut dev) };
        if err != ffi::ERR_SUCCESS {
            // The service arbitrates ownership: a select that fails on a
            // device the table just listed is nearly always "someone else has
            // it" — but keep the API's own words in the message too.
            return Err(Error::InUse(format!(
                "{} (serial {}) — {}",
                model.label(),
                dev.serial(),
                api.err_text(err)
            )));
        }
        Ok(dev)
    })();
    unsafe { (api.unlock_device_api)() };
    result.map(|dev| (api, dev))
}

/// Hand a selected device back to the service. Best-effort: on an unplugged
/// device the service has already reclaimed it and the error means nothing.
pub(crate) fn release(dev: &mut ffi::DeviceT) {
    let s = state().lock().expect("sdrplay api state poisoned");
    if let Some(api) = &s.api {
        let err = unsafe { (api.release_device)(dev) };
        if err != ffi::ERR_SUCCESS {
            tracing::debug!("ReleaseDevice: {}", api.err_text(err));
        }
    }
}
