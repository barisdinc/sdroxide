//! Live HamQTH logbook upload round-trip: insert a throwaway QSO through our
//! real upload path, then delete it so nothing is left behind. The account is
//! read from the `HAMQTH_USER` / `HAMQTH_PASS` env vars (never committed); the
//! test is skipped if either is unset.
//!
//!   HAMQTH_USER=oe3jjs HAMQTH_PASS=secret MY_CALL=OE3JJS \
//!     cargo test -p sdroxide-net --test hamqth_upload -- --ignored --nocapture
//!
//! The delete half is not politeness, it is the point: HamQTH's real-time
//! endpoint writes into the operator's own permanent log, and a test that only
//! inserted would leave a fictional contact in it.

use sdroxide_net::upload_qso;
use sdroxide_types::{Credentials, NetworkConfig, QsoRecord, UploadTarget, qso_log_to_adif};

/// The one QSO this test creates and removes. A fixed date, so a run that
/// somehow fails to clean up leaves something obvious to find by hand.
const TEST_CALL: &str = "TE1ST";
const TEST_DATE: &str = "20231114";
const TEST_TIME: &str = "2213";
const TEST_BAND: &str = "20m";
const TEST_MODE: &str = "SSB";

#[test]
#[ignore = "uploads to the live HamQTH logbook (needs HAMQTH_USER, HAMQTH_PASS and MY_CALL)"]
fn hamqth_insert_then_delete() {
    let (Ok(user), Ok(pass)) = (std::env::var("HAMQTH_USER"), std::env::var("HAMQTH_PASS")) else {
        eprintln!("HAMQTH_USER/HAMQTH_PASS not set — skipping");
        return;
    };
    let Ok(my_call) = std::env::var("MY_CALL") else {
        eprintln!("MY_CALL not set — skipping");
        return;
    };

    // 2023-11-14 22:13:20 UTC — the same instant TEST_DATE/TEST_TIME name, so
    // the delete below can address the record the insert created.
    let rec = QsoRecord {
        call: TEST_CALL.into(),
        rst_sent: Some(59),
        rst_rcvd: Some(59),
        freq_hz: 14_250_000.0,
        mode: TEST_MODE.into(),
        band: TEST_BAND.into(),
        start_utc: 1_700_000_000,
        end_utc: 1_700_000_000,
        my_call: my_call.clone(),
        ..Default::default()
    };
    let adif = qso_log_to_adif(std::slice::from_ref(&rec));

    let cfg = NetworkConfig {
        hamqth: Credentials { user: user.clone(), password: pass.clone() },
        ..Default::default()
    };
    let result = upload_qso(&cfg, &my_call, UploadTarget::HamQth, &adif);
    println!("HamQTH insert result: {result:?}");

    // Delete before asserting, so a rejected assertion still cleans up after a
    // partial success (HamQTH answering 400 for a duplicate, say, after an
    // earlier run left one behind).
    let del = hamqth_delete(&user, &pass, &my_call);
    println!("HamQTH delete result: {del:?}");

    assert!(result.is_ok(), "HamQTH upload failed: {result:?}");
    assert!(del.is_ok(), "HamQTH delete failed — check the log by hand: {del:?}");
}

/// Remove the test QSO, addressed the way HamQTH documents: `cmd=delete` with
/// the record's identifying fields carried under an `OLD_` prefix.
fn hamqth_delete(user: &str, pass: &str, my_call: &str) -> Result<String, String> {
    let field = |name: &str, value: &str| format!("<{}:{}>{}", name, value.len(), value);
    let adif = format!(
        "{}{}{}{}{}<EOR>",
        field("OLD_QSO_DATE", TEST_DATE),
        field("OLD_TIME_ON", TEST_TIME),
        field("OLD_CALL", TEST_CALL),
        field("OLD_BAND", TEST_BAND),
        field("OLD_MODE", TEST_MODE),
    );
    let mut resp = ureq::post("https://www.hamqth.com/qso_realtime.php")
        .config()
        .http_status_as_error(false)
        .build()
        .send_form([
            ("u", user),
            ("p", pass),
            ("c", my_call),
            ("adif", adif.as_str()),
            ("prg", "sdroxide"),
            ("cmd", "delete"),
        ])
        .map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    if status == 200 { Ok(body) } else { Err(format!("HTTP {status}: {}", body.trim())) }
}
