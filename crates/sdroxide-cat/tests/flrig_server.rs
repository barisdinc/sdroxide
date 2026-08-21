//! The flrig profile against a fake flrig — a real TCP server speaking
//! XML-RPC over HTTP, driven through the crate's public API only, so the whole
//! stack runs: the driver thread, the write-fed correlation queue, the open
//! sequence, the polls.
//!
//! What the fake serves is a KX3 as flrig presents one: 15 W of maximum power,
//! the Elecraft driver's mode names, a dial adopted rather than imposed.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
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
    /// How many times a client has connected. A driver that keeps having to
    /// remake the link to put its answers back on their questions is a driver
    /// that has not solved anything.
    connections: Arc<AtomicUsize>,
}

/// Serve XML-RPC on an ephemeral port until the process ends.
fn fake_flrig() -> FakeFlrig {
    fake_flrig_that_is_busy_for(Duration::ZERO)
}

/// The same fake, made to behave the way flrig's own server does while it is
/// busy with the radio — see the test that uses it. `busy` is how long each
/// request is left waiting before the server looks at the socket at all.
fn fake_flrig_that_is_busy_for(busy: Duration) -> FakeFlrig {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let state =
        Arc::new(Mutex::new(RigState { freq: "14074000".to_string(), mode: "DATA".to_string() }));
    let (tx, calls) = channel();
    let server_state = Arc::clone(&state);
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&connections);
    std::thread::spawn(move || {
        // Connections are sequential here: query_once opens and drops one,
        // the driver thread then holds one for the rest of the test.
        for conn in listener.incoming() {
            let Ok(conn) = conn else { break };
            counter.fetch_add(1, Ordering::Relaxed);
            serve_connection(conn, &tx, &server_state, busy);
        }
    });
    FakeFlrig { addr, calls, state, connections }
}

fn serve_connection(
    mut conn: TcpStream,
    tx: &Sender<(String, String)>,
    state: &Mutex<RigState>,
    busy: Duration,
) {
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
            // What the real server's tardiness looks like from out here: the
            // socket is left unread for as long as the radio has it busy, so
            // whatever the client sends meanwhile piles up in the kernel.
            std::thread::sleep(busy);
            match conn.read(&mut read_buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&read_buf[..n]),
            }
        };
        // ...and this is what it does with the pile: XmlRpc++ reads every byte
        // waiting on the socket into one buffer, answers the first request in
        // it, and then clears the buffer whole — see `writeResponse`'s
        // `_header = ""; _request = ""`. Anything that arrived behind the
        // request it answered is thrown away, unanswered and unreported.
        if !busy.is_zero() {
            buf.clear();
        }
        let method = between(&request, "<methodName>", "</methodName>").unwrap_or_default();
        let _ = tx.send((method.clone(), request.clone()));
        // The one call flrig really does sit on: `rig.set_ptt` polls the radio
        // for confirmation of the change, up to a second, and holds the single
        // server thread the whole time. Every over is therefore a moment when
        // whatever else a client sends is thrown away.
        if method == "rig.set_ptt" {
            std::thread::sleep(busy * 5);
        }
        let value = {
            let state = state.lock().unwrap();
            match method.as_str() {
                "rig.get_vfo" => format!("<string>{}</string>", state.freq),
                "rig.get_mode" => format!("<string>{}</string>", state.mode),
                "rig.get_xcvr" => "<string>KX3</string>".to_string(),
                "rig.get_maxpwr" => "<i4>15</i4>".to_string(),
                "rig.get_power" => "<i4>10</i4>".to_string(),
                "rig.get_DBM" => "<i4>-73</i4>".to_string(),
                "rig.get_ptt" => "<i4>0</i4>".to_string(),
                // The two faces that fit on a dial: a small number that is a
                // perfectly good meter reading and a catastrophic frequency.
                "rig.get_swrmeter" | "rig.get_pwrmeter" => "<i4>7</i4>".to_string(),
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

/// Drain what the driver has reported, failing on anything the fake never
/// answered: a dial that is not the rig's, or an over nobody keyed. Both are
/// what a reply read against the wrong request looks like from up here.
fn drain_sane(handle: &CatHandle) -> Vec<CatUpdate> {
    let batch = handle.poll();
    for u in &batch {
        match u {
            CatUpdate::Freq(hz) => assert!(
                (hz - 14_074_000.0).abs() < 1.0 || (hz - 7_030_000.0).abs() < 1.0,
                "the dial took an answer meant for another question: {hz} Hz"
            ),
            CatUpdate::Ptt(true) => panic!("an over nobody keyed was reported"),
            _ => {}
        }
    }
    batch
}

/// [`drain_sane`] until one update matches, or the deadline passes.
fn await_sane(
    handle: &CatHandle,
    deadline: Duration,
    what: &str,
    pred: impl Fn(&CatUpdate) -> bool,
) {
    let end = Instant::now() + deadline;
    loop {
        if drain_sane(handle).iter().any(&pred) {
            return;
        }
        assert!(Instant::now() < end, "no {what} arrived within {deadline:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
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

/// The failure users report against a real flrig: a dial that flips between
/// the frequency they tuned and something absurd — 7 Hz — while flrig's own
/// window shows the radio exactly where they left it.
///
/// flrig's XML-RPC server reads every byte waiting on the socket in one go,
/// answers the first request in it, and throws the rest away unanswered. A
/// second request sent before the first was answered therefore does not exist
/// as far as the server is concerned — and correlation by *order*, which is
/// all XML-RPC leaves a client, is broken from that moment for the rest of the
/// session: the dial reads the S-meter's answer, the meter reads the dial's,
/// and the transmit read takes a frequency for a rig that has keyed itself.
///
/// So nothing may go out while an answer is still owed. This test is that rule
/// against a server tardy enough to make the collision certain.
#[test]
fn nothing_goes_out_while_flrig_still_owes_an_answer() {
    // Longer than the gap the driver leaves between two frames, so a poll that
    // wrote both of its requests without waiting has both of them queued up
    // behind one another before the server so much as reads the socket.
    let server = fake_flrig_that_is_busy_for(Duration::from_millis(60));
    let handle = sdroxide_cat::spawn(config(&server.addr));
    let wait = Duration::from_secs(5);

    await_sane(&handle, wait, "the rig's dial", |u| *u == CatUpdate::Freq(14_074_000.0));

    // Several seconds of the ordinary traffic — dial, mode, S-meter, transmit
    // read — through a server answering one at a time. Every answer has to
    // land on the question that asked it.
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        drain_sane(&handle);
        std::thread::sleep(Duration::from_millis(5));
    }

    // And the poll still follows the radio, which is the thing the rule must
    // not have cost: a driver that went quiet would pass every assertion above.
    server.state.lock().unwrap().freq = "7030000".to_string();
    await_sane(&handle, wait, "the dial change", |u| *u == CatUpdate::Freq(7_030_000.0));

    // An over, which is where this fails hardest: keying is the one call flrig
    // sits on, so it is the moment a client that does not wait piles requests
    // up to be discarded. Everything about it has to stay readable — and the
    // SWR face in particular, which the protection trip reads: a frequency
    // landing there is a 5:1 that would stand the operator down mid-over.
    handle.set_ptt(true);
    let end = Instant::now() + Duration::from_secs(2);
    let mut swr = None;
    while Instant::now() < end {
        drain_sane(&handle);
        if let Some(t) = handle.poll_telemetry() {
            swr = t.swr.or(swr);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    handle.set_ptt(false);
    let swr = swr.expect("the SWR face is read while transmitting");
    assert!(swr < 2.0, "an SWR of {swr}:1 is a frequency on the meter, not a reading");

    // ...and the poll still follows the radio afterwards. A dial that has not
    // moved is not reported twice, so the rig is moved back to give it
    // something to say.
    server.state.lock().unwrap().freq = "14074000".to_string();
    await_sane(&handle, wait, "the dial after the over", |u| *u == CatUpdate::Freq(14_074_000.0));

    // All of it down one connection. Noticing the answers have slipped and
    // remaking the link would keep the panel honest too — it is the last line
    // of defence and it is meant to be there — but a session that spends
    // itself reconnecting has not stopped losing requests, only stopped
    // believing them.
    assert_eq!(
        server.connections.load(Ordering::Relaxed),
        1,
        "the link was remade: requests are still going out unanswered"
    );
}
