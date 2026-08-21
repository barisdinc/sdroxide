//! The flrig profile against a fake flrig — a real TCP server speaking
//! XML-RPC over HTTP, driven through the crate's public API only, so the whole
//! stack runs: the driver thread, the write-fed correlation queue, the open
//! sequence, the polls.
//!
//! What the fake serves is a KX3 as flrig presents one: 15 W of maximum power,
//! the Elecraft driver's mode names, a dial adopted rather than imposed.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sdroxide_cat::{CatHandle, CatUpdate};
use sdroxide_types::{CatConfig, CatFamily, Mode};

/// What the rig behind the fake flrig is currently doing — the test mutates
/// this and the poll is expected to notice.
struct RigState {
    freq: String,
    mode: String,
}

struct FakeFlrig {
    addr: String,
    /// Every request the server answered, as `(method, body)` in arrival
    /// order.
    calls: Receiver<(String, String)>,
    state: Arc<Mutex<RigState>>,
}

/// Serve XML-RPC on an ephemeral port until the process ends.
fn fake_flrig() -> FakeFlrig {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let state =
        Arc::new(Mutex::new(RigState { freq: "14074000".to_string(), mode: "DATA".to_string() }));
    let (tx, calls) = channel();
    let server_state = Arc::clone(&state);
    std::thread::spawn(move || {
        // Connections are sequential here: query_once opens and drops one,
        // the driver thread then holds one for the rest of the test.
        for conn in listener.incoming() {
            let Ok(conn) = conn else { break };
            serve_connection(conn, &tx, &server_state);
        }
    });
    FakeFlrig { addr, calls, state }
}

fn serve_connection(mut conn: TcpStream, tx: &Sender<(String, String)>, state: &Mutex<RigState>) {
    let mut buf = Vec::new();
    let mut read_buf = [0u8; 1024];
    loop {
        // One complete HTTP request: headers, then Content-Length of body.
        let request = loop {
            if let Some(head_end) = find(&buf, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if buf.len() >= head_end + 4 + len {
                    let body = String::from_utf8_lossy(&buf[head_end + 4..head_end + 4 + len])
                        .into_owned();
                    buf.drain(..head_end + 4 + len);
                    break body;
                }
            }
            match conn.read(&mut read_buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&read_buf[..n]),
            }
        };
        let method = between(&request, "<methodName>", "</methodName>").unwrap_or_default();
        let _ = tx.send((method.clone(), request.clone()));
        let value = {
            let state = state.lock().unwrap();
            match method.as_str() {
                "rig.get_vfo" => format!("<string>{}</string>", state.freq),
                "rig.get_mode" => format!("<string>{}</string>", state.mode),
                "rig.get_xcvr" => "<string>KX3</string>".to_string(),
                "rig.get_maxpwr" => "<i4>15</i4>".to_string(),
                "rig.get_power" => "<i4>10</i4>".to_string(),
                "rig.get_modes" => "<array><data><value>LSB</value><value>USB</value>\
                                    <value>CW</value><value>FM</value><value>AM</value>\
                                    <value>DATA</value><value>CW-R</value><value>DATA-R</value>\
                                    </data></array>"
                    .to_string(),
                // Every setter and everything else is acknowledged; XML-RPC
                // answers every call.
                _ => "<i4>0</i4>".to_string(),
            }
        };
        let body = format!(
            "<?xml version=\"1.0\"?><methodResponse><params><param><value>{value}</value>\
             </param></params></methodResponse>"
        );
        let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
        if conn.write_all(response.as_bytes()).is_err() {
            return;
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn between(s: &str, open: &str, close: &str) -> Option<String> {
    let start = s.find(open)? + open.len();
    let end = s[start..].find(close)? + start;
    Some(s[start..end].to_string())
}

fn config(addr: &str) -> CatConfig {
    CatConfig {
        family: CatFamily::Flrig,
        flrig_addr: addr.to_string(),
        // Faster than the default so the mode poll comes round inside the
        // test's budget; a real operator pointing at a localhost daemon might
        // well run it this fast too.
        poll_hz: 10.0,
        ..CatConfig::default()
    }
}

/// Drain updates until one matches, or the deadline passes.
fn await_update(
    handle: &CatHandle,
    deadline: Duration,
    what: &str,
    pred: impl Fn(&CatUpdate) -> bool,
) -> CatUpdate {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        for u in handle.poll() {
            if pred(&u) {
                return u;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("no {what} arrived within {deadline:?}");
}

/// Wait for the server to have answered `method`, returning the request body.
fn await_call(calls: &Receiver<(String, String)>, method: &str, deadline: Duration) -> String {
    let end = Instant::now() + deadline;
    loop {
        let now = Instant::now();
        if now >= end {
            panic!("the server never saw a {method}");
        }
        match calls.recv_timeout(end - now) {
            Ok((m, body)) if m == method => return body,
            Ok(_) => continue,
            Err(_) => panic!("the server never saw a {method}"),
        }
    }
}

#[test]
fn the_startup_query_adopts_the_rigs_dial_and_mode() {
    let server = fake_flrig();
    let got = sdroxide_cat::query_once(&config(&server.addr))
        .expect("the fake answered; the query must too");
    assert_eq!(got, (Some(14_074_000.0), Some(Mode::Digu)));
}

#[test]
fn a_whole_session_against_a_fake_kx3() {
    let server = fake_flrig();
    let handle = sdroxide_cat::spawn(config(&server.addr));
    let wait = Duration::from_secs(5);

    // The dial, mode and power are all adopted from the rig — each reported
    // exactly once, so they are collected together rather than awaited one at
    // a time (a drain waiting for one would swallow the others). The power
    // fraction is the proof the open sequence learned the scale before the
    // power read was interpreted: 10 W of 15 W, not of an assumed 100.
    let mut seen = Vec::new();
    let end = Instant::now() + wait;
    let adopted = |seen: &[CatUpdate]| {
        seen.contains(&CatUpdate::Freq(14_074_000.0))
            && seen.contains(&CatUpdate::Mode(Mode::Digu))
            && seen
                .iter()
                .any(|u| matches!(u, CatUpdate::Power(frac) if (frac - 10.0 / 15.0).abs() < 1e-3))
    };
    while !adopted(&seen) {
        assert!(Instant::now() < end, "adoption incomplete within {wait:?}: {seen:?}");
        seen.extend(handle.poll());
        std::thread::sleep(Duration::from_millis(10));
    }

    // The slider's fraction reaches flrig in the rig's own watts.
    assert!(handle.commands_power());
    handle.set_power(0.5);
    let body = await_call(&server.calls, "rig.set_power", wait);
    assert!(body.contains("<i4>8</i4>"), "0.5 of 15 W is 8 W, got: {body}");

    // The filter goes out as a width for flrig to snap.
    handle.set_filter(Mode::Cw, 300.0, 800.0);
    let body = await_call(&server.calls, "rig.set_bandwidth", wait);
    assert!(body.contains("<i4>500</i4>"), "got: {body}");

    // The desync trap, end to end: a mode change reported by the rig makes
    // the driver *generate* a mode frame purely for comparison, without
    // writing it. If that generated frame fed the correlation queue, every
    // reply after it would be read one slot off — and the dial would follow
    // the S-meter or worse. It must keep tracking the truth instead.
    server.state.lock().unwrap().mode = "CW".to_string();
    await_update(&handle, wait, "reported mode change", |u| *u == CatUpdate::Mode(Mode::Cw));
    server.state.lock().unwrap().freq = "7030000".to_string();
    await_update(&handle, wait, "dial change after the trap", |u| {
        *u == CatUpdate::Freq(7_030_000.0)
    });
}
