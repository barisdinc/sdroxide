//! Callsign lookup via QRZ.com XML or HamQTH. Both use a session-key login
//! (cached briefly to avoid re-authenticating on every call), then a per-call
//! query. Responses are XML, parsed with `roxmltree`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use sdroxide_types::{CallsignInfo, Credentials, LookupProvider};

use crate::http;

/// Cached session keys: `"provider:user" -> (key, obtained_at)`.
fn cache() -> &'static Mutex<HashMap<String, (String, Instant)>> {
    static C: OnceLock<Mutex<HashMap<String, (String, Instant)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_key(id: &str, ttl: Duration) -> Option<String> {
    let map = cache().lock().ok()?;
    map.get(id).filter(|(_, t)| t.elapsed() < ttl).map(|(k, _)| k.clone())
}

fn store_key(id: &str, key: &str) {
    if let Ok(mut map) = cache().lock() {
        map.insert(id.to_string(), (key.to_string(), Instant::now()));
    }
}

fn drop_key(id: &str) {
    if let Ok(mut map) = cache().lock() {
        map.remove(id);
    }
}

/// Look up `call` with the configured provider.
pub fn lookup(
    provider: LookupProvider,
    qrz: &Credentials,
    hamqth: &Credentials,
    call: &str,
) -> Result<CallsignInfo, String> {
    let call = call.trim().to_ascii_uppercase();
    if call.is_empty() {
        return Err("empty callsign".into());
    }
    match provider {
        LookupProvider::None => Err("lookup provider not configured".into()),
        LookupProvider::Qrz => qrz_lookup(qrz, &call),
        LookupProvider::HamQth => hamqth_lookup(hamqth, &call),
    }
}

// ── QRZ.com XML ──────────────────────────────────────────────────────────

fn qrz_lookup(creds: &Credentials, call: &str) -> Result<CallsignInfo, String> {
    if creds.user.trim().is_empty() {
        return Err("QRZ username/password not set".into());
    }
    let id = format!("qrz:{}", creds.user.trim());
    // Try a cached session first, re-authenticating once if it's stale.
    for attempt in 0..2 {
        let key = match cached_key(&id, Duration::from_secs(12 * 3600)) {
            Some(k) => k,
            None => {
                let k = qrz_login(creds)?;
                store_key(&id, &k);
                k
            }
        };
        let url = format!("https://xmldata.qrz.com/xml/current/?s={key};callsign={call}");
        let body = http::get(&url)?;
        let doc = roxmltree::Document::parse(&body).map_err(|e| e.to_string())?;
        if let Some(err) = tag_text(&doc, "Error") {
            let el = err.to_ascii_lowercase();
            if attempt == 0
                && (el.contains("session") || el.contains("timeout") || el.contains("invalid"))
            {
                drop_key(&id);
                continue;
            }
            return Err(err);
        }
        return Ok(parse_qrz(&doc, call));
    }
    Err("QRZ session could not be established".into())
}

fn qrz_login(creds: &Credentials) -> Result<String, String> {
    let url = format!(
        "https://xmldata.qrz.com/xml/current/?username={};password={};agent=sdroxide",
        urlencode(creds.user.trim()),
        urlencode(creds.password.trim())
    );
    let body = http::get(&url)?;
    let doc = roxmltree::Document::parse(&body).map_err(|e| e.to_string())?;
    if let Some(k) = tag_text(&doc, "Key") {
        return Ok(k);
    }
    Err(tag_text(&doc, "Error").unwrap_or_else(|| "QRZ login failed".into()))
}

fn parse_qrz(doc: &roxmltree::Document, call: &str) -> CallsignInfo {
    let mut info = CallsignInfo::for_call(call);
    let fname = tag_text(doc, "fname").unwrap_or_default();
    let lname = tag_text(doc, "name").unwrap_or_default();
    let name = format!("{fname} {lname}").trim().to_string();
    info.name = (!name.is_empty()).then_some(name);
    info.qth = tag_text(doc, "addr2").filter(|s| !s.is_empty());
    info.grid = tag_text(doc, "grid").filter(|s| !s.is_empty());
    info.state = tag_text(doc, "state").filter(|s| !s.is_empty());
    info.county = tag_text(doc, "county").filter(|s| !s.is_empty());
    info.country = tag_text(doc, "country").filter(|s| !s.is_empty());
    info.dxcc = tag_text(doc, "dxcc").and_then(|s| s.parse().ok());
    info.cq_zone = tag_text(doc, "cqzone").and_then(|s| s.parse().ok());
    info.itu_zone = tag_text(doc, "ituzone").and_then(|s| s.parse().ok());
    info.qsl_via = tag_text(doc, "qslmgr").filter(|s| !s.is_empty());
    info
}

// ── HamQTH XML ───────────────────────────────────────────────────────────

fn hamqth_lookup(creds: &Credentials, call: &str) -> Result<CallsignInfo, String> {
    if creds.user.trim().is_empty() {
        return Err("HamQTH username/password not set".into());
    }
    let id = format!("hamqth:{}", creds.user.trim());
    for attempt in 0..2 {
        let key = match cached_key(&id, Duration::from_secs(50 * 60)) {
            Some(k) => k,
            None => {
                let k = hamqth_login(creds)?;
                store_key(&id, &k);
                k
            }
        };
        let url = format!("https://www.hamqth.com/xml.php?id={key}&callsign={call}&prg=sdroxide");
        let body = http::get(&url)?;
        let doc = roxmltree::Document::parse(&body).map_err(|e| e.to_string())?;
        if let Some(err) = tag_text(&doc, "error") {
            let el = err.to_ascii_lowercase();
            if attempt == 0 && el.contains("session") {
                drop_key(&id);
                continue;
            }
            return Err(err);
        }
        return Ok(parse_hamqth(&doc, call));
    }
    Err("HamQTH session could not be established".into())
}

/// Ask HamQTH for a session id, which is also the cheapest read-only proof
/// that a username and password are good — [`crate::upload::test_login`] uses
/// it for the logbook, since HamQTH authenticates both with the same account.
pub(crate) fn hamqth_login(creds: &Credentials) -> Result<String, String> {
    let url = format!(
        "https://www.hamqth.com/xml.php?u={}&p={}",
        urlencode(creds.user.trim()),
        urlencode(creds.password.trim())
    );
    let body = http::get(&url)?;
    let doc = roxmltree::Document::parse(&body).map_err(|e| e.to_string())?;
    if let Some(k) = tag_text(&doc, "session_id") {
        return Ok(k);
    }
    Err(tag_text(&doc, "error").unwrap_or_else(|| "HamQTH login failed".into()))
}

fn parse_hamqth(doc: &roxmltree::Document, call: &str) -> CallsignInfo {
    let mut info = CallsignInfo::for_call(call);
    info.name =
        tag_text(doc, "adr_name").or_else(|| tag_text(doc, "nick")).filter(|s| !s.is_empty());
    info.qth = tag_text(doc, "qth").or_else(|| tag_text(doc, "adr_city")).filter(|s| !s.is_empty());
    info.grid = tag_text(doc, "grid").filter(|s| !s.is_empty());
    info.state = tag_text(doc, "us_state").filter(|s| !s.is_empty());
    info.county = tag_text(doc, "us_county").filter(|s| !s.is_empty());
    info.country = tag_text(doc, "country").filter(|s| !s.is_empty());
    info.dxcc = tag_text(doc, "adif").and_then(|s| s.parse().ok());
    info.cq_zone = tag_text(doc, "cq").and_then(|s| s.parse().ok());
    info.itu_zone = tag_text(doc, "itu").and_then(|s| s.parse().ok());
    info.qsl_via = tag_text(doc, "qsl_via").filter(|s| !s.is_empty());
    info
}

// ── helpers ──────────────────────────────────────────────────────────────

/// First descendant element with tag `name` (case-insensitive), trimmed text.
fn tag_text(doc: &roxmltree::Document, name: &str) -> Option<String> {
    doc.descendants()
        .find(|n| n.tag_name().name().eq_ignore_ascii_case(name))
        .and_then(|n| n.text())
        .map(|t| t.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Minimal percent-encoding for credentials in a query string.
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
