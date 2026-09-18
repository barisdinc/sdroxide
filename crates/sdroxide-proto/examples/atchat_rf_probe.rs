//! Headless AtCHAT NET status probe over a running sdroxide server's
//! WebSocket — for watching (and optionally driving) a real-radio AtCHAT
//! session from a terminal, with no GUI in the loop.
//!
//! `DigiStatus.atchat` (carried in `ServerMsg::Ft8Status`, the generic
//! digital-mode status lane — see its doc comment) already has everything the
//! ATCHAT panel shows: roster, chat, the station's own log, carrier and
//! keyed. This just prints it as it changes, and can fire one chat line on
//! the way in.
//!
//! Usage:
//!   cargo run -p sdroxide-proto --example atchat_rf_probe -- \
//!       127.0.0.1:18089 60 [send_to] [send_text...]
//!
//! `send_to` empty ("") sends to the common channel; omit both to only watch.

use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use tungstenite::Message;

use sdroxide_proto::{AudioCaps, ClientMsg, PROTO_VERSION, ServerMsg, decode, encode};
use sdroxide_types::Command;

fn send(ws: &mut tungstenite::WebSocket<TcpStream>, msg: &ClientMsg) {
    ws.send(Message::Binary(encode(msg).unwrap().into())).unwrap();
}

fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

fn main() {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:4950".to_string());
    let watch_secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let send_to = args.next();
    let send_text: String = args.collect::<Vec<_>>().join(" ");

    println!("[{}] connecting to {addr} ...", now());
    let sockaddr = addr.to_socket_addrs().unwrap().next().unwrap();
    let stream = TcpStream::connect_timeout(&sockaddr, Duration::from_secs(3)).unwrap();
    stream.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let (mut ws, _) = tungstenite::client(format!("ws://{addr}/ws").as_str(), stream).unwrap();

    send(
        &mut ws,
        &ClientMsg::Hello {
            proto: PROTO_VERSION,
            audio: AudioCaps { opus_decode: false, opus_encode: false },
        },
    );

    if let Some(to) = &send_to {
        if !send_text.is_empty() {
            // Give HelloAck / the connect-time replay a moment before keying.
            std::thread::sleep(Duration::from_millis(800));
            println!(
                "[{}] sending to {:?}: {send_text:?}",
                now(),
                if to.is_empty() { "(common)" } else { to }
            );
            send(&mut ws, &ClientMsg::Command(Command::AtChatSendChat { to: to.clone(), text: send_text }));
        }
    }

    let mut seen_roster = 0usize;
    let mut seen_chat = 0usize;
    let mut seen_log = 0usize;
    let mut last_carrier = false;
    let mut last_keyed = false;
    let mut last_role: Option<String> = None;
    let mut last_master: Option<String> = None;
    let mut last_connected: Option<bool> = None;

    let deadline = Instant::now() + Duration::from_secs(watch_secs);
    while Instant::now() < deadline {
        match ws.read() {
            Ok(Message::Binary(bytes)) => {
                let Ok(ServerMsg::Ft8Status(status)) = decode::<ServerMsg>(&bytes) else { continue };
                let Some(a) = status.atchat else { continue };

                if Some(a.connected) != last_connected {
                    println!("[{}] connected = {}", now(), a.connected);
                    last_connected = Some(a.connected);
                }
                if a.role != last_role {
                    println!("[{}] role = {:?}", now(), a.role);
                    last_role = a.role.clone();
                }
                if a.master != last_master {
                    println!("[{}] master = {:?}", now(), a.master);
                    last_master = a.master.clone();
                }
                if a.carrier != last_carrier {
                    println!("[{}] carrier {}", now(), if a.carrier { "DETECTED" } else { "clear" });
                    last_carrier = a.carrier;
                }
                if a.keyed != last_keyed {
                    println!("[{}] TX {}", now(), if a.keyed { "keyed" } else { "released" });
                    last_keyed = a.keyed;
                }
                if a.roster.len() != seen_roster {
                    for r in &a.roster {
                        println!("[{}] roster: {r:?}", now());
                    }
                    seen_roster = a.roster.len();
                }
                for l in a.log.iter().skip(seen_log) {
                    println!("[{}] LOG: {l}", now());
                }
                seen_log = a.log.len();
                for c in a.chat.iter().skip(seen_chat) {
                    println!(
                        "[{}] CHAT {}{} : {:?}",
                        now(),
                        if c.own { "(own) " } else { "" },
                        c.from,
                        c.text
                    );
                }
                seen_chat = a.chat.len();
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => {
                println!("[{}] socket error: {e}", now());
                break;
            }
        }
    }
    println!("[{}] done watching", now());
}
