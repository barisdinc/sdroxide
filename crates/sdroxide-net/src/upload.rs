//! One-click QSO upload to eQSL, QRZ Logbook, HamQTH and Club Log, plus
//! QSL-confirmation download from LoTW and eQSL. LoTW *upload* is intentionally
//! not automated (it requires TQSL signing) — the UI exports ADIF for the
//! operator to sign; but confirmations can still be downloaded here to drive
//! award tracking.

use sdroxide_types::{LoginTarget, NetworkConfig, QsoRecord, UploadTarget};

use crate::http;

/// Upload one QSO's ADIF to `target`, returning a human-readable status on
/// success or an error string.
pub fn upload(
    cfg: &NetworkConfig,
    my_call: &str,
    target: UploadTarget,
    adif: &str,
) -> Result<String, String> {
    match target {
        UploadTarget::Eqsl => upload_eqsl(cfg, adif),
        UploadTarget::QrzLogbook => upload_qrz(cfg, adif),
        UploadTarget::ClubLog => upload_clublog(cfg, my_call, adif),
        UploadTarget::HamQth => upload_hamqth(cfg, my_call, adif),
    }
}

fn upload_eqsl(cfg: &NetworkConfig, adif: &str) -> Result<String, String> {
    if cfg.eqsl.user.trim().is_empty() {
        return Err("eQSL username/password not set".into());
    }
    let body = http::post_form(
        "https://www.eqsl.cc/qslcard/importADIF.cfm",
        &[
            ("EQSL_USER", cfg.eqsl.user.trim()),
            ("EQSL_PSWD", cfg.eqsl.password.trim()),
            ("ADIFData", adif),
        ],
    )?;
    // eQSL returns HTML; success contains "Result: 1 out of 1 …". Errors carry
    // an "Error:" / "Warning:" line.
    let text = strip_html(&body);
    if text.to_ascii_lowercase().contains("added") || text.contains("Result: 1") {
        Ok("eQSL: accepted".into())
    } else if let Some(line) = text.lines().find(|l| {
        let l = l.to_ascii_lowercase();
        l.contains("error") || l.contains("warning") || l.contains("bad")
    }) {
        Err(line.trim().to_string())
    } else {
        // Some accounts return a terse OK page; treat a 200 with no error as ok.
        Ok("eQSL: submitted".into())
    }
}

fn upload_qrz(cfg: &NetworkConfig, adif: &str) -> Result<String, String> {
    if cfg.qrz_logbook_key.trim().is_empty() {
        return Err("QRZ Logbook API key not set".into());
    }
    let body = http::post_form(
        "https://logbook.qrz.com/api",
        &[("KEY", cfg.qrz_logbook_key.trim()), ("ACTION", "INSERT"), ("ADIF", adif)],
    )?;
    // Response is url-encoded key=value pairs: RESULT=OK / FAIL / AUTH / REPLACE.
    let fields = parse_kv(&body);
    match fields.iter().find(|(k, _)| k == "RESULT").map(|(_, v)| v.as_str()) {
        Some("OK") => Ok("QRZ: logged".into()),
        Some("REPLACE") => Ok("QRZ: already logged (replaced)".into()),
        Some(other) => {
            let reason = fields
                .iter()
                .find(|(k, _)| k == "REASON")
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| other.to_string());
            Err(format!("QRZ: {reason}"))
        }
        None => {
            Err(format!("QRZ: unexpected response: {}", body.chars().take(120).collect::<String>()))
        }
    }
}

fn upload_clublog(cfg: &NetworkConfig, my_call: &str, adif: &str) -> Result<String, String> {
    if cfg.clublog.user.trim().is_empty() || cfg.clublog_api_key.trim().is_empty() {
        return Err("Club Log email/password/API key not set".into());
    }
    // Station callsign for the log: my_call, else the record's own is used by CL.
    let body = http::post_form(
        "https://clublog.org/realtime.php",
        &[
            ("email", cfg.clublog.user.trim()),
            ("password", cfg.clublog.password.trim()),
            ("callsign", my_call.trim()),
            ("api", cfg.clublog_api_key.trim()),
            ("adif", adif),
        ],
    )?;
    let t = body.trim();
    // Club Log returns 200 with an empty/OK body on success, error text otherwise.
    if t.is_empty() || t.eq_ignore_ascii_case("ok") {
        Ok("Club Log: accepted".into())
    } else if t.to_ascii_lowercase().contains("error") || t.len() > 3 {
        Err(format!("Club Log: {}", t.chars().take(160).collect::<String>()))
    } else {
        Ok("Club Log: accepted".into())
    }
}

/// HamQTH's real-time QSO endpoint (`cmd=insert`).
///
/// The credentials are `cfg.hamqth` — the same pair the callsign lookup uses,
/// not a second copy. HamQTH issues one account per operator and this endpoint
/// authenticates with it directly, so the Uploads tab offers the one pair in
/// both places rather than inviting them to disagree.
///
/// Unlike the other three targets, the answer is the HTTP status: 200 QSO OK,
/// 400 QSO Rejected (duplicate, wrong band, missing field), 403 Forbidden
/// (credentials), 500 for a server or ADIF problem, with a sentence in the
/// body. Hence [`http::post_form_status`] rather than `post_form`, which would
/// flatten all four into one error string.
fn upload_hamqth(cfg: &NetworkConfig, my_call: &str, adif: &str) -> Result<String, String> {
    if cfg.hamqth.user.trim().is_empty() || cfg.hamqth.password.trim().is_empty() {
        return Err("HamQTH username/password not set".into());
    }
    let records = sdroxide_types::adif_records(adif);
    if records.is_empty() {
        return Err("HamQTH: nothing to upload (no QSO in the ADIF)".into());
    }
    // HamQTH asks specifically that this endpoint not be used to batch-upload a
    // log, and every caller here sends exactly one QSO; the loop exists so a
    // multi-record ADIF is not silently reduced to its first contact.
    for fields in &records {
        let body = hamqth_adif(fields)?;
        let (status, reply) = http::post_form_status(
            "https://www.hamqth.com/qso_realtime.php",
            &[
                ("u", cfg.hamqth.user.trim()),
                ("p", cfg.hamqth.password.trim()),
                // The station callsign goes here, not in the ADIF: HamQTH looks
                // for it in `c` and falls back to the username when it is empty,
                // which would file the QSO under the wrong call for anyone whose
                // HamQTH login is not their station call.
                ("c", my_call.trim()),
                ("adif", &body),
                ("prg", "sdroxide"),
                ("cmd", "insert"),
            ],
        )?;
        let reply = reply.trim();
        let detail = || {
            if reply.is_empty() {
                String::new()
            } else {
                format!(": {}", reply.chars().take(160).collect::<String>())
            }
        };
        match status {
            200 => {}
            400 => return Err(format!("HamQTH: QSO rejected{}", detail())),
            403 => return Err("HamQTH: username or password rejected".into()),
            500 => return Err(format!("HamQTH: server or ADIF error{}", detail())),
            other => return Err(format!("HamQTH: HTTP {other}{}", detail())),
        }
    }
    Ok(match records.len() {
        1 => "HamQTH: logged".into(),
        n => format!("HamQTH: {n} QSOs logged"),
    })
}

/// The ADIF fields HamQTH's real-time endpoint documents, as
/// `(what sdroxide exports, what HamQTH calls it)`.
///
/// A filter as much as a rename table. HamQTH publishes the list it supports
/// and answers 500 for "a problem with your ADIF file", so a field it never
/// named — `SIG`, `CONTEST_ID`, `COUNTRY`, the `MY_*` zones — is dropped rather
/// than gambled with; `STATION_CALLSIGN` is dropped for a different reason, the
/// `c` POST parameter above. The two renames are the ones that would otherwise
/// fail every insert in the same way: HamQTH spells the signal reports `RST_S`
/// and `RST_R`, not ADIF's `RST_SENT`/`RST_RCVD`, and requires both.
const HAMQTH_FIELDS: &[(&str, &str)] = &[
    ("QSO_DATE", "QSO_DATE"),
    ("TIME_ON", "TIME_ON"),
    ("TIME_OFF", "TIME_OFF"),
    ("CALL", "CALL"),
    ("FREQ", "FREQ"),
    ("MODE", "MODE"),
    ("BAND", "BAND"),
    ("RST_SENT", "RST_S"),
    ("RST_RCVD", "RST_R"),
    ("NAME", "NAME"),
    ("QTH", "QTH"),
    ("GRIDSQUARE", "GRIDSQUARE"),
    ("MY_GRIDSQUARE", "MY_GRIDSQUARE"),
    ("STATE", "STATE"),
    ("CNTY", "CNTY"),
    ("COMMENT", "COMMENT"),
    ("IOTA", "IOTA"),
    ("TX_PWR", "TX_PWR"),
    ("ITUZ", "ITUZ"),
    ("CQZ", "CQZ"),
    ("CONT", "CONT"),
    // "Sending DXCC … with every QSO is strongly recommended!" — without it the
    // account's DXCC statistics come out wrong.
    ("DXCC", "DXCC"),
    ("QSL_SENT", "QSL_SENT"),
    ("QSL_RCVD", "QSL_RCVD"),
    ("QSL_VIA", "QSL_VIA"),
    ("LOTW_QSL_SENT", "LOTW_QSL_SENT"),
    ("LOTW_QSL_RCVD", "LOTW_QSL_RCVD"),
    ("EQSL_QSL_SENT", "EQSL_QSL_SENT"),
    ("EQSL_QSL_RCVD", "EQSL_QSL_RCVD"),
];

/// What HamQTH requires before it will accept an insert, in its own spelling.
const HAMQTH_REQUIRED: &[&str] = &["QSO_DATE", "TIME_ON", "CALL", "MODE", "BAND", "RST_S", "RST_R"];

/// Re-emit one parsed ADIF record in the dialect HamQTH documents: no file
/// header, only the fields it lists, under the names it uses.
///
/// The missing-field check is done here rather than left to the server because
/// the server's answer is "400 QSO Rejected", which is also what it says for a
/// duplicate — an operator told only that would have no way to tell a QSO with
/// no signal reports from one they had already uploaded.
fn hamqth_adif(fields: &[(String, String)]) -> Result<String, String> {
    let mut out = String::new();
    let mut present: Vec<&str> = Vec::new();
    for (name, value) in fields {
        let Some(&(_, hamqth_name)) = HAMQTH_FIELDS.iter().find(|(ours, _)| ours == name) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        // Band and mode go up. sdroxide writes bands lower-case ("20m") where
        // most loggers write "20M", and one of the things HamQTH names when it
        // answers "400 QSO Rejected" is a wrong band. Both are ADIF
        // enumerations, which are case-insensitive by specification, so this
        // cannot make the record less valid — but it is a hedge, not a measured
        // fix: no HamQTH account here has been shown to refuse the lower-case
        // form.
        let upper;
        let value = if matches!(hamqth_name, "BAND" | "MODE") {
            upper = value.to_ascii_uppercase();
            upper.as_str()
        } else {
            value
        };
        // Length in bytes, as sdroxide's own writer counts it, and no escaping:
        // HamQTH asks specifically that values not be HTML-escaped.
        out.push_str(&format!("<{}:{}>{}", hamqth_name, value.len(), value));
        present.push(hamqth_name);
    }
    let missing: Vec<&str> =
        HAMQTH_REQUIRED.iter().copied().filter(|r| !present.contains(r)).collect();
    if !missing.is_empty() {
        return Err(format!(
            "HamQTH: this QSO has no {}, which it requires on every contact",
            missing.join(", ")
        ));
    }
    out.push_str("<EOR>");
    Ok(out)
}

/// Check one service's stored credentials, without logging anything.
///
/// ⛔ NOTHING HERE MAY WRITE. A credential check that inserted a dummy QSO to
/// see whether the login worked would put a fictional contact in the operator's
/// permanent log and, for the services that forward to LoTW or an award
/// programme, somewhere it cannot be withdrawn from. Every branch below uses
/// the cheapest READ endpoint the service publishes, and the upload endpoints
/// above are deliberately not reachable from here.
///
/// The message on success is what the operator sees next to the button, so it
/// says something they can check against — the account or callsign the service
/// answered for, where the service gives one.
pub fn test_login(
    cfg: &NetworkConfig,
    my_call: &str,
    target: LoginTarget,
) -> Result<String, String> {
    match target {
        LoginTarget::Eqsl => test_eqsl(cfg),
        LoginTarget::QrzLogbook => test_qrz(cfg),
        LoginTarget::ClubLog => test_clublog(cfg, my_call),
        LoginTarget::Lotw => test_lotw(cfg),
        LoginTarget::HamQth => test_hamqth(cfg),
    }
}

fn test_eqsl(cfg: &NetworkConfig) -> Result<String, String> {
    if cfg.eqsl.user.trim().is_empty() || cfg.eqsl.password.trim().is_empty() {
        return Err("username/password not set".into());
    }
    // The inbox download, asked for a date nothing can be newer than: the
    // question is whether the login is accepted, not what is in the inbox, and
    // this keeps eQSL from building an ADIF file to answer it.
    let url = format!(
        "https://www.eqsl.cc/qslcard/DownloadInBox.cfm?UserName={}&Password={}&RcvdSince=20991231",
        urlencode(cfg.eqsl.user.trim()),
        urlencode(cfg.eqsl.password.trim())
    );
    let page = http::get(&url)?;
    let text = strip_html(&page);
    let low = text.to_ascii_lowercase();
    // Every rule below is taken from what eQSL actually returned on 17 August
    // 2026, for a good login, a good login with nothing to fetch, and a
    // deliberately wrong password. Guessing at this is how the first version
    // managed to be wrong in both directions at once: eQSL says
    // "Error: No such Username/Password found", which contains neither "invalid"
    // nor "not found", and its empty-inbox answer is "You have no log entries",
    // which contains neither "no qso" nor a ".adi" link.
    if low.contains("no such username/password") || low.contains("error:") {
        let line = text
            .lines()
            .map(str::trim)
            .find(|l| {
                l.to_ascii_lowercase().contains("error")
                    || l.to_ascii_lowercase().contains("no such")
            })
            .unwrap_or("login rejected");
        return Err(line.trim_start_matches("Error:").trim().chars().take(120).collect());
    }
    if low.contains("no log entries")
        || low.contains("has been built")
        || page.to_ascii_lowercase().contains(".adi")
    {
        return Ok(format!("signed in as {}", cfg.eqsl.user.trim()));
    }
    Err("unexpected reply; check the username and password".into())
}

fn test_qrz(cfg: &NetworkConfig) -> Result<String, String> {
    if cfg.qrz_logbook_key.trim().is_empty() {
        return Err("API key not set".into());
    }
    // STATUS reports the logbook's own details and inserts nothing. This is the
    // one service of the four with an endpoint meant for exactly this.
    let body = http::post_form(
        "https://logbook.qrz.com/api",
        &[("KEY", cfg.qrz_logbook_key.trim()), ("ACTION", "STATUS")],
    )?;
    let kv = parse_kv(&body);
    let get = |k: &str| kv.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
    match get("RESULT").as_deref() {
        Some("OK") => {
            let call = get("CALLSIGN").unwrap_or_else(|| "logbook".into());
            match get("COUNT") {
                Some(n) => Ok(format!("{call}, {n} QSOs")),
                None => Ok(call),
            }
        }
        _ => Err(get("REASON")
            .or_else(|| get("STATUS"))
            .unwrap_or_else(|| "key rejected".into())
            .chars()
            .take(120)
            .collect()),
    }
}

/// Club Log needs BOTH an account password and an API key, and the upload uses
/// both, so a check that proved only one would pass while uploads still failed.
/// They have separate read endpoints, so both are exercised.
fn test_clublog(cfg: &NetworkConfig, my_call: &str) -> Result<String, String> {
    if cfg.clublog.user.trim().is_empty() || cfg.clublog.password.trim().is_empty() {
        return Err("email/password not set".into());
    }
    let call = my_call.trim();
    if call.is_empty() {
        return Err("station callsign not set".into());
    }
    // The OQRS ADIF download. `type=dxqsl` asks for OQRS requests rather than
    // the whole log, so this stays small on a station with fifty thousand QSOs,
    // and POST is required rather than GET.
    //
    // ⚠️ THIS CHECKS THE ACCOUNT, NOT THE API KEY, and the upload needs both.
    // The documented key check is `GET /dxcc?call=&api=&full=1`, which returned
    // 403 Forbidden on every attempt here on 17 August 2026, with and without a
    // User-Agent, while the account call below succeeded from the same host
    // seconds later. Rather than report a guess about the key, the message says
    // which half was actually checked.
    let body = http::post_form(
        "https://clublog.org/getadif.php",
        &[
            ("email", cfg.clublog.user.trim()),
            ("password", cfg.clublog.password.trim()),
            ("call", call),
            ("type", "dxqsl"),
        ],
    )?;
    // A positive marker, not an error-word search: Club Log's own success text
    // is prose, and a rule that hunts for "error" in prose eventually finds one.
    if body.to_ascii_lowercase().contains("club log") {
        return Ok(format!("{call}: account accepted (API key not checked)"));
    }
    Err(body.trim().chars().take(120).collect())
}

/// HamQTH, via the XML session login the callsign lookup already uses.
///
/// ⛔ `qso_realtime.php` has no read-only mode — its only commands are insert,
/// update and delete — so checking it directly would mean logging a QSO to find
/// out whether the password works, which is exactly what this whole function is
/// forbidden from doing. `xml.php?u=&p=` reads nothing, writes nothing, and
/// answers about *the same account*: HamQTH has one login per operator and the
/// upload endpoint authenticates with it.
///
/// What it therefore proves is the account, not that the logbook will accept a
/// particular QSO — that is true of the eQSL and LoTW checks above too.
fn test_hamqth(cfg: &NetworkConfig) -> Result<String, String> {
    if cfg.hamqth.user.trim().is_empty() || cfg.hamqth.password.trim().is_empty() {
        return Err("username/password not set".into());
    }
    crate::lookup::hamqth_login(&cfg.hamqth)?;
    Ok(format!("signed in as {}", cfg.hamqth.user.trim()))
}

fn test_lotw(cfg: &NetworkConfig) -> Result<String, String> {
    if cfg.lotw.user.trim().is_empty() || cfg.lotw.password.trim().is_empty() {
        return Err("login not set".into());
    }
    // The same report the confirmation sync uses, bounded to a date that cannot
    // return records, so this costs ARRL almost nothing to answer.
    let url = format!(
        "https://lotw.arrl.org/lotwuser/lotwreport.adi?login={}&password={}&qso_query=1&qso_qsl=yes&qso_qslsince=2099-12-31",
        urlencode(cfg.lotw.user.trim()),
        urlencode(cfg.lotw.password.trim())
    );
    let body = http::get(&url)?;
    // A rejected login is an HTML page; an accepted one is ADIF, whose header
    // ends in <eoh> even when no records follow.
    if body.to_ascii_lowercase().contains("username/password") || body.contains("<!DOCTYPE") {
        return Err("login rejected (LoTW returned its log-on page)".into());
    }
    if body.to_ascii_lowercase().contains("<eoh>") {
        return Ok(format!("signed in as {}", cfg.lotw.user.trim()));
    }
    Err("unexpected reply; check the login and password".into())
}

/// Download QSL confirmations from LoTW and eQSL, returning parsed confirmation
/// records (the UI matches these to the log to set `*_rcvd`). Best-effort: a
/// service that isn't configured is skipped; per-service errors are collected.
pub fn sync_confirmations(cfg: &NetworkConfig) -> (Vec<QsoRecord>, Vec<String>) {
    let mut confirmed = Vec::new();
    let mut errors = Vec::new();

    if !cfg.lotw.user.trim().is_empty() {
        match download_lotw(cfg) {
            Ok(mut recs) => confirmed.append(&mut recs),
            Err(e) => errors.push(format!("LoTW: {e}")),
        }
    }
    if !cfg.eqsl.user.trim().is_empty() {
        match download_eqsl(cfg) {
            Ok(mut recs) => confirmed.append(&mut recs),
            Err(e) => errors.push(format!("eQSL: {e}")),
        }
    }
    (confirmed, errors)
}

fn download_lotw(cfg: &NetworkConfig) -> Result<Vec<QsoRecord>, String> {
    // Confirmed QSLs only (qso_qsl=yes), with detail so BAND/MODE/DATE parse.
    let url = format!(
        "https://lotw.arrl.org/lotwuser/lotwreport.adi?login={}&password={}&qso_query=1&qso_qsl=yes&qso_qsldetail=yes",
        urlencode(cfg.lotw.user.trim()),
        urlencode(cfg.lotw.password.trim())
    );
    let body = http::get(&url)?;
    if body.to_ascii_lowercase().contains("username/password") || body.contains("<!DOCTYPE") {
        return Err("login rejected".into());
    }
    let mut recs = sdroxide_types::adif_to_qso_log(&body);
    for r in &mut recs {
        r.lotw_rcvd = true; // this report is the set of LoTW-confirmed QSOs
    }
    Ok(recs)
}

fn download_eqsl(cfg: &NetworkConfig) -> Result<Vec<QsoRecord>, String> {
    // eQSL's inbox download is two-step: the first call builds an .adi and
    // returns a page linking to it.
    let url = format!(
        "https://www.eqsl.cc/qslcard/DownloadInBox.cfm?UserName={}&Password={}&RcvdSince=19700101",
        urlencode(cfg.eqsl.user.trim()),
        urlencode(cfg.eqsl.password.trim())
    );
    let page = http::get(&url)?;
    // Find the ".adi" link in the returned HTML.
    let link = page
        .split(['"', '\'', ' ', '\n'])
        .find(|t| t.to_ascii_lowercase().ends_with(".adi"))
        .ok_or("no download link returned (check credentials)")?;
    let adi_url = if link.starts_with("http") {
        link.to_string()
    } else {
        format!("https://www.eqsl.cc/qslcard/{}", link.trim_start_matches('/'))
    };
    let body = http::get(&adi_url)?;
    let mut recs = sdroxide_types::adif_to_qso_log(&body);
    for r in &mut recs {
        r.eqsl_rcvd = true; // eQSL inbox = received confirmations
    }
    Ok(recs)
}

// ── helpers ──────────────────────────────────────────────────────────────

/// Parse `k=v&k=v` (or newline-separated) into pairs; keys uppercased.
fn parse_kv(body: &str) -> Vec<(String, String)> {
    body.split(['&', '\n'])
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.trim().to_ascii_uppercase(), urldecode(v.trim())))
        .collect()
}

/// Very small HTML→text: drop tags, collapse whitespace.
fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v as char);
                    i += 3;
                    continue;
                }
                out.push('%');
                i += 1;
            }
            b'+' => {
                out.push(' ');
                i += 1;
            }
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_qrz_response() {
        let kv = parse_kv("RESULT=OK&COUNT=1&LOGID=123");
        assert_eq!(kv.iter().find(|(k, _)| k == "RESULT").unwrap().1, "OK");
    }

    #[test]
    fn strips_html() {
        assert_eq!(strip_html("<b>Result: 1</b> added").trim(), "Result: 1 added");
    }

    /// The whole point of the rewrite: sdroxide's own export goes in, and what
    /// comes out is HamQTH's spelling, without the header or the fields it
    /// never named.
    #[test]
    fn rewrites_export_into_hamqth_dialect() {
        let rec = sdroxide_types::QsoRecord {
            call: "OK2CQR".into(),
            rst_sent: Some(59),
            rst_rcvd: Some(57),
            freq_hz: 14_250_000.0,
            mode: "SSB".into(),
            band: "20m".into(),
            start_utc: 1_700_000_000,
            end_utc: 1_700_000_000,
            my_call: "OE3JJS".into(),
            country: "Czech Republic".into(),
            contest_id: "CQ-WW-SSB".into(),
            dxcc: Some(503),
            ..Default::default()
        };
        let adif = sdroxide_types::qso_log_to_adif(std::slice::from_ref(&rec));
        let records = sdroxide_types::adif_records(&adif);
        assert_eq!(records.len(), 1);
        let out = hamqth_adif(&records[0]).expect("all required fields present");

        // Renamed, because HamQTH requires these two names and no others.
        assert!(out.contains("<RST_S:2>59"), "{out}");
        assert!(out.contains("<RST_R:2>57"), "{out}");
        assert!(!out.contains("RST_SENT"), "{out}");
        // Kept.
        assert!(out.contains("<CALL:6>OK2CQR"), "{out}");
        assert!(out.contains("<BAND:3>20M"), "band is upper-cased: {out}");
        assert!(out.contains("<DXCC:3>503"), "{out}");
        // Dropped: not on HamQTH's published list, or carried in the POST.
        assert!(!out.contains("CONTEST_ID"), "{out}");
        assert!(!out.contains("COUNTRY"), "{out}");
        assert!(!out.contains("STATION_CALLSIGN"), "{out}");
        // No file header, one record.
        assert!(!out.contains("<EOH>") && !out.contains("ADIF_VER"), "{out}");
        assert!(out.ends_with("<EOR>"), "{out}");
    }

    /// A QSO with no signal reports is rejected here, naming the field, rather
    /// than sent off to come back as an unexplained "400 QSO Rejected".
    #[test]
    fn names_the_missing_required_field() {
        let rec = sdroxide_types::QsoRecord {
            call: "OK2CQR".into(),
            mode: "SSB".into(),
            band: "20m".into(),
            start_utc: 1_700_000_000,
            ..Default::default()
        };
        let adif = sdroxide_types::qso_log_to_adif(std::slice::from_ref(&rec));
        let err = hamqth_adif(&sdroxide_types::adif_records(&adif)[0]).unwrap_err();
        assert!(err.contains("RST_S") && err.contains("RST_R"), "{err}");
    }

    /// Values are counted in bytes, as sdroxide's own writer counts them — an
    /// accented name is where a character count would part company with it and
    /// hand HamQTH a truncated field.
    #[test]
    fn counts_value_length_in_bytes() {
        let fields = vec![
            ("QSO_DATE".to_string(), "20231114".to_string()),
            ("TIME_ON".to_string(), "2213".to_string()),
            ("CALL".to_string(), "OK2CQR".to_string()),
            ("MODE".to_string(), "SSB".to_string()),
            ("BAND".to_string(), "20m".to_string()),
            ("RST_SENT".to_string(), "59".to_string()),
            ("RST_RCVD".to_string(), "59".to_string()),
            ("NAME".to_string(), "Petr Hložek".to_string()),
        ];
        let out = hamqth_adif(&fields).unwrap();
        assert!(out.contains("<NAME:12>Petr Hložek"), "{out}");
    }
}
