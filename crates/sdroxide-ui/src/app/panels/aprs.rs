//! The APRS panel: who is out there, where they are, and what they said.
//!
//! Three things an operator watches, and they move independently, so each gets
//! its own space rather than sharing one:
//!
//! - **STATIONS** — everything heard, newest activity first, with the detail
//!   card for whichever one is selected.
//! - **MAP** — the same stations placed, drawn with the icon each one's symbol
//!   asks for. See [`crate::aprs_map`].
//! - **MESSAGES** — the conversation, and the box to answer from. A `RAW` chip
//!   swaps it for every frame on the channel, which is where somebody else's
//!   traffic and anything the codec could not read both end up.
//!
//! Unlike the packet panel there *is* text entry here, because unlike packet
//! there is a conversation to have: APRS messages are addressed, acknowledged
//! and retried, and answering one is a button.

use eframe::egui::{self, RichText};
use sdroxide_types::{AprsEntryKind, AprsMsgState, AprsStation, AprsStatus, Command};

use crate::app::util::fmt_age;
use crate::app::{SdroxideApp, tx_gated};
use crate::theme;
use crate::theme::ThemedScroll;

/// Longest message the format carries.
const MSG_MAX: usize = 67;

impl SdroxideApp {
    pub(in crate::app) fn aprs_panel(
        &mut self,
        ui: &mut egui::Ui,
        cmds: &mut Vec<Command>,
        panel_h: f32,
    ) {
        let st: Option<AprsStatus> =
            self.digi_status.as_ref().and_then(|s| s.aprs.as_ref().map(|a| (**a).clone()));
        let Some(st) = st else {
            ui.label(RichText::new("starting the APRS modem…").weak());
            return;
        };
        let tx_ok = self.tx_capable();
        let now = crate::time::now_unix();
        let content_bottom = ui.cursor().top() + panel_h - 26.0;

        self.aprs_header(ui, cmds, &st, tx_ok);
        ui.add_space(3.0);

        let avail_h = (content_bottom - ui.cursor().top()).max(80.0);
        let pane = self.phone_pane(ui, self.state.rx[0].mode);
        let full_w = ui.available_width();

        ui.horizontal_top(|ui| {
            if pane.is_none_or(|p| p == 0) {
                ui.allocate_ui_with_layout(
                    egui::vec2(
                        if pane.is_some() { full_w } else { (full_w * 0.34).clamp(190.0, 340.0) },
                        avail_h,
                    ),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| self.aprs_station_list(ui, &st, now, avail_h, tx_ok),
                );
            }
            if pane.is_none() {
                ui.separator();
            }
            // The map and the messages share the rest, the map taking the
            // larger share: it is the reason this mode has a panel of its own.
            if pane.is_none_or(|p| p == 1) {
                ui.vertical(|ui| {
                    if pane.is_none() {
                        let map_h = (avail_h * 0.56).max(crate::aprs_map::MIN_HEIGHT);
                        self.aprs_map_pane(ui, &st, now, map_h);
                        ui.add_space(2.0);
                        self.aprs_message_pane(ui, cmds, &st, avail_h - map_h - 6.0, tx_ok);
                    } else {
                        self.aprs_message_pane(ui, cmds, &st, avail_h, tx_ok);
                    }
                });
            }
            if pane == Some(2) {
                ui.vertical(|ui| self.aprs_map_pane(ui, &st, now, avail_h));
            }
        });
    }

    /// The channel, the beacon and the buttons.
    fn aprs_header(
        &mut self,
        ui: &mut egui::Ui,
        cmds: &mut Vec<Command>,
        st: &AprsStatus,
        tx_ok: bool,
    ) {
        let transmitting = self.digi_status.as_ref().is_some_and(|s| s.transmitting);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("APRS").size(11.0).strong().color(theme::CYAN()));
            ui.label(RichText::new("1200 baud").weak().size(10.5));
            self.digi_freq_chip(ui, cmds);
            ui.label(
                RichText::new(format!("{} stn", st.stations.len()))
                    .monospace()
                    .size(10.5)
                    .color(theme::CYAN_DIM()),
            )
            .on_hover_text("Stations heard and still inside the window set in APRS Setup");
            // The three that come and go, in an id scope of their own.
            //
            // Carrier detect follows the channel and flips several times a
            // second, so the number of widgets in this row is not constant —
            // and egui derives a widget's id from a counter, so everything
            // after them would get a new id whenever one appeared. The scope
            // costs the parent exactly one id however many are drawn inside it.
            ui.push_id("aprs-channel-state", |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    if st.dcd {
                        ui.label(RichText::new("BUSY").strong().size(10.5).color(theme::ALERT()))
                            .on_hover_text(
                                "Another station is on the channel — nothing will key until it \
                                 clears",
                            );
                    }
                    if st.bad_frames > 0 {
                        ui.label(RichText::new(format!("{} bad", st.bad_frames)).weak().size(10.5))
                            .on_hover_text(
                                "Frames that arrived but failed their check sequence — a \
                                 collision, a fade, or a signal too weak to read.",
                            );
                    }
                    if st.tx_queue > 0 {
                        ui.label(
                            RichText::new(format!("{} queued", st.tx_queue))
                                .size(10.5)
                                .color(theme::YELLOW()),
                        )
                        .on_hover_text("Waiting for the channel to clear");
                    }
                });
            });
            crate::chrome::row_tail(ui, |ui| {
                // Same reasoning as the channel state above: these two come and
                // go, and the buttons beside them must keep their ids.
                ui.push_id("aprs-tx-state", |ui| {
                    ui.horizontal(|ui| {
                        if let Some(secs) = st.next_beacon_s {
                            ui.label(
                                RichText::new(format!("{}:{:02}", secs / 60, secs % 60))
                                    .monospace()
                                    .size(10.0)
                                    .color(theme::CYAN_DIM()),
                            )
                            .on_hover_text("Until the next scheduled beacon");
                        }
                        if transmitting {
                            ui.label(RichText::new("● TX").color(theme::ALERT()).strong());
                        }
                    });
                });
                self.clear_rx_chip(ui, cmds);
                if crate::chrome::chip(
                    ui,
                    self.show_digi_settings,
                    RichText::new("SETUP").size(9.5),
                )
                .clicked()
                {
                    self.show_digi_settings = !self.show_digi_settings;
                }
                if tx_gated(ui, tx_ok, |ui| {
                    // A fixed face, with the countdown beside it rather than
                    // inside it. A label that changes width re-lays the whole
                    // row out, and egui then finds the same rectangle carrying
                    // a different widget between its two passes — which is a
                    // stream of warnings and a row that twitches once a second.
                    crate::chrome::chip_accent(
                        ui,
                        false,
                        RichText::new(" BEACON ").strong(),
                        theme::GREEN(),
                        theme::INK_ON_CYAN(),
                    )
                    .on_hover_text(match st.next_beacon_s {
                        Some(_) => "Send a position beacon now, without waiting for the timer",
                        None => {
                            "Send one position beacon. Set an interval in APRS Setup to beacon \
                             regularly — it is off until you do."
                        }
                    })
                })
                .clicked()
                {
                    cmds.push(Command::AprsBeacon);
                }
            });
        });
    }

    /// Everything heard, and the card for whichever one is selected.
    fn aprs_station_list(
        &mut self,
        ui: &mut egui::Ui,
        st: &AprsStatus,
        now: i64,
        avail_h: f32,
        tx_ok: bool,
    ) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("STATIONS").size(10.5).strong().color(theme::CYAN_DIM()));
            crate::chrome::row_tail(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.aprs_filter)
                        .hint_text("filter")
                        .desired_width(74.0),
                );
            });
        });

        let filter = self.aprs_filter.trim().to_ascii_uppercase();
        // Newest activity first: on a channel where everything is a beacon,
        // "who just moved" is the question a list answers.
        let mut rows: Vec<&AprsStation> = st
            .stations
            .iter()
            .filter(|s| {
                filter.is_empty()
                    || s.name.to_ascii_uppercase().contains(&filter)
                    || s.comment.to_ascii_uppercase().contains(&filter)
            })
            .collect();
        rows.sort_by_key(|s| -s.last_heard);

        let selected = self.aprs_map.selected.clone();
        let card = selected.as_ref().and_then(|n| st.stations.iter().find(|s| &s.name == n));
        let card_h = if card.is_some() { (avail_h * 0.42).clamp(96.0, 230.0) } else { 0.0 };
        let list_h = (avail_h - card_h - 26.0).max(50.0);

        let mut pick = None;
        egui::ScrollArea::vertical().id_salt("aprs-stations").max_height(list_h).show_themed(
            ui,
            |ui| {
                if rows.is_empty() {
                    ui.label(RichText::new("nothing heard yet").weak());
                }
                for s in &rows {
                    let sel = selected.as_deref() == Some(s.name.as_str());
                    let resp = ui
                        .horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            let (r, _) = ui
                                .allocate_exact_size(egui::vec2(15.0, 15.0), egui::Sense::hover());
                            let tint = if s.killed {
                                theme::gray(120)
                            } else if sel {
                                theme::YELLOW()
                            } else {
                                theme::CYAN()
                            };
                            self.aprs_icons.paint(ui, r, s.symbol.kind(), tint);
                            let name = RichText::new(&s.name)
                                .monospace()
                                .size(11.0)
                                .color(if s.killed { theme::gray(120) } else { theme::CYAN() });
                            ui.label(if sel { name.strong() } else { name });
                            // Direct is the fact worth a mark: on a channel
                            // where everything is digipeated, the stations you
                            // hear direct are the ones actually in range.
                            if s.direct {
                                ui.label(RichText::new("·").size(11.0).color(theme::GREEN()))
                                    .on_hover_text("Heard direct — no digipeater repeated it");
                            }
                            crate::chrome::row_tail(ui, |ui| {
                                ui.label(
                                    RichText::new(fmt_age(now - s.last_heard))
                                        .size(9.5)
                                        .weak()
                                        .monospace(),
                                );
                            });
                        })
                        .response;
                    let resp = resp.interact(egui::Sense::click());
                    if resp.clicked() {
                        pick = Some(s.name.clone());
                    }
                }
            },
        );
        if let Some(name) = pick {
            let same = self.aprs_map.selected.as_deref() == Some(name.as_str());
            self.aprs_map.selected = if same { None } else { Some(name.clone()) };
            if !same {
                // Selecting a station is also how you address a message to it,
                // which saves typing a callsign that is already on screen.
                if let Some(s) = st.stations.iter().find(|s| s.name == name) {
                    if s.entry == AprsEntryKind::Station {
                        self.aprs_target = name;
                    }
                }
            }
        }

        if let Some(s) = card {
            ui.separator();
            self.aprs_station_card(ui, s, st, now, card_h, tx_ok);
        }
    }

    /// Everything one station has told us.
    #[allow(clippy::too_many_arguments)]
    fn aprs_station_card(
        &mut self,
        ui: &mut egui::Ui,
        s: &AprsStation,
        st: &AprsStatus,
        now: i64,
        h: f32,
        tx_ok: bool,
    ) {
        let name = s.name.clone();
        egui::ScrollArea::vertical().id_salt("aprs-card").max_height(h).show_themed(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&s.name).monospace().strong().color(theme::YELLOW()));
                ui.label(RichText::new(s.symbol.kind().label()).size(10.0).weak());
                ui.label(RichText::new(s.symbol.text()).size(9.5).monospace().weak())
                    .on_hover_text("The two characters the station sent to say what it is");
                if s.killed {
                    ui.label(RichText::new("KILLED").size(9.5).color(theme::ALERT()))
                        .on_hover_text("The station that put this object here has cancelled it");
                }
            });
            if s.entry == AprsEntryKind::Object && !s.reported_by.is_empty() {
                ui.label(
                    RichText::new(format!("object reported by {}", s.reported_by))
                        .size(10.0)
                        .weak(),
                );
            }
            if !s.comment.is_empty() {
                ui.label(RichText::new(&s.comment).size(10.5));
            }
            if !s.status.is_empty() {
                ui.label(RichText::new(&s.status).size(10.5).color(theme::GREEN()));
            }
            if let Some(q) = s.pos {
                ui.label(
                    RichText::new(format!("{:.5}, {:.5}", q.lat, q.lon)).monospace().size(10.0),
                );
                if q.ambiguity > 0 {
                    ui.label(
                        RichText::new(format!("± {} digit(s) blanked", q.ambiguity))
                            .size(9.5)
                            .weak(),
                    )
                    .on_hover_text(
                        "The sender deliberately reported a square rather than a point. The \
                         map draws the square.",
                    );
                }
                if let Some(me) = st.my_pos {
                    let km = sdroxide_types::distance_km((me.lat, me.lon), (q.lat, q.lon));
                    let bear = sdroxide_types::bearing_deg((me.lat, me.lon), (q.lat, q.lon));
                    ui.label(
                        RichText::new(format!("{km:.1} km   {bear:.0}°")).monospace().size(10.0),
                    );
                }
            }
            let mut motion = Vec::new();
            if let Some(c) = s.course_deg {
                motion.push(format!("{c:03}°"));
            }
            if let Some(v) = s.speed_kn {
                motion.push(format!("{v:.0} kn ({:.0} km/h)", v * 1.852));
            }
            if let Some(a) = s.altitude_m {
                motion.push(format!("{a:.0} m"));
            }
            if !motion.is_empty() {
                ui.label(RichText::new(motion.join("   ")).monospace().size(10.0));
            }
            if let Some(w) = s.weather.filter(|w| !w.is_empty()) {
                let mut bits = Vec::new();
                if let Some(t) = w.temp_c {
                    bits.push(format!("{t:.1} °C"));
                }
                if let (Some(d), Some(v)) = (w.wind_dir_deg, w.wind_speed_ms) {
                    bits.push(format!("wind {d:03}° {v:.1} m/s"));
                }
                if let Some(g) = w.wind_gust_ms {
                    bits.push(format!("gust {g:.1}"));
                }
                if let Some(hu) = w.humidity_pct {
                    bits.push(format!("{hu}% RH"));
                }
                if let Some(p) = w.pressure_hpa {
                    bits.push(format!("{p:.1} hPa"));
                }
                if let Some(r) = w.rain_1h_mm {
                    bits.push(format!("rain {r:.1} mm/h"));
                }
                ui.label(RichText::new(bits.join("   ")).size(10.0).color(theme::CYAN_DIM()));
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!(
                        "{} frame(s), last {} ago",
                        s.packets,
                        fmt_age(now - s.last_heard)
                    ))
                    .size(9.5)
                    .weak(),
                );
            });
            if !s.via.is_empty() {
                ui.label(
                    RichText::new(format!("via {}", s.via.join(","))).size(9.5).monospace().weak(),
                )
                .on_hover_text(
                    "The digipeaters the last frame came through. A `*` marks one that \
                         actually repeated it.",
                );
            }
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                if s.entry == AprsEntryKind::Station
                    && tx_gated(ui, tx_ok, |ui| {
                        crate::chrome::chip(ui, false, RichText::new(" MESSAGE ").size(10.0))
                            .on_hover_text("Address the message box to this station")
                    })
                    .clicked()
                {
                    self.aprs_target = name.clone();
                }
                if s.pos.is_some()
                    && crate::chrome::chip(ui, false, RichText::new(" CENTRE ").size(10.0))
                        .on_hover_text(
                            "Put this station in the middle of the map. Double-click the map \
                             to hand the view back to the automatic fit.",
                        )
                        .clicked()
                    && let Some(q) = s.pos
                {
                    // Holding the view, not just re-fitting it: the auto-fit
                    // frames everything heard, which is the opposite of what
                    // somebody asking to centre on one station wants.
                    self.aprs_map.view.centre_on(q.lat, q.lon);
                }
            });
        });
    }

    fn aprs_map_pane(&mut self, ui: &mut egui::Ui, st: &AprsStatus, now: i64, h: f32) {
        let ttl = self.digi_cfg_edit.aprs_station_ttl_min;
        let icons = &mut self.aprs_icons;
        let state = &mut self.aprs_map;
        let picked = crate::aprs_map::show(ui, state, icons, &st.stations, st.my_pos, now, ttl, h);
        if let Some(name) = picked {
            if st.stations.iter().any(|s| s.name == name && s.entry == AprsEntryKind::Station) {
                self.aprs_target = name;
            }
        }
    }

    /// The conversation, or the raw channel, plus the box to answer from.
    fn aprs_message_pane(
        &mut self,
        ui: &mut egui::Ui,
        cmds: &mut Vec<Command>,
        st: &AprsStatus,
        h: f32,
        tx_ok: bool,
    ) {
        let me = self.digi_cfg_edit.aprs_mycall.trim().to_ascii_uppercase();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(if self.aprs_show_traffic { "CHANNEL" } else { "MESSAGES" })
                    .size(10.5)
                    .strong()
                    .color(theme::CYAN_DIM()),
            );
            crate::chrome::row_tail(ui, |ui| {
                if crate::chrome::chip(ui, self.aprs_show_traffic, RichText::new("RAW").size(9.5))
                    .on_hover_text(
                        "Every frame on the channel, as it arrived — other people's traffic \
                         included, and anything the decoder could not read.",
                    )
                    .clicked()
                {
                    self.aprs_show_traffic = !self.aprs_show_traffic;
                }
            });
        });

        let compose_h = 30.0;
        let list_h = (h - compose_h - 22.0).max(40.0);
        egui::ScrollArea::vertical()
            .id_salt("aprs-messages")
            .max_height(list_h)
            .min_scrolled_height(list_h)
            .stick_to_bottom(true)
            // Fill the height it was given rather than shrinking to the
            // messages in it, so the box you type into stays where it was
            // instead of climbing up the panel as the conversation grows.
            .auto_shrink([false, false])
            .show_themed(ui, |ui| {
                if self.aprs_show_traffic {
                    if st.traffic.is_empty() {
                        ui.label(RichText::new("nothing on the channel yet").weak());
                    }
                    for t in &st.traffic {
                        let via = if t.via.is_empty() {
                            String::new()
                        } else {
                            format!(",{}", t.via.join(","))
                        };
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            let colour = if t.sent { theme::GREEN() } else { theme::CYAN_DIM() };
                            ui.label(
                                RichText::new(format!("{}>{}{via}", t.from, t.to))
                                    .monospace()
                                    .size(10.0)
                                    .color(colour),
                            );
                            ui.label(RichText::new(&t.kind).size(9.0).weak());
                            ui.label(RichText::new(&t.info).monospace().size(10.0));
                        });
                    }
                    return;
                }
                if st.messages.is_empty() {
                    ui.label(
                        RichText::new(
                            "No messages. Pick a station and write to it — a message is \
                             acknowledged and retried, unlike everything else on this channel.",
                        )
                        .weak()
                        .size(10.5),
                    );
                }
                for m in &st.messages {
                    let ours = m.from.eq_ignore_ascii_case(&me) && !me.is_empty();
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.label(
                            RichText::new(crate::app::util::time_str(m.at))
                                .monospace()
                                .size(9.0)
                                .weak(),
                        );
                        let who = if ours { format!("→{}", m.to) } else { m.from.clone() };
                        ui.label(RichText::new(who).monospace().size(10.5).color(if ours {
                            theme::GREEN()
                        } else {
                            theme::YELLOW()
                        }));
                        ui.label(RichText::new(&m.text).size(10.5));
                        if ours {
                            let (face, colour, tip) = match m.state {
                                AprsMsgState::Queued => {
                                    ("…", theme::gray(150), "waiting for the channel")
                                }
                                AprsMsgState::Sent => {
                                    ("↑", theme::CYAN_DIM(), "sent, waiting to be acknowledged")
                                }
                                AprsMsgState::Acked => {
                                    ("✓", theme::GREEN(), "acknowledged by the far end")
                                }
                                AprsMsgState::Rejected => {
                                    ("✗", theme::ALERT(), "refused by the far end")
                                }
                                AprsMsgState::Failed => {
                                    ("✗", theme::ALERT(), "no answer after every retry")
                                }
                                AprsMsgState::Received => ("", theme::gray(150), ""),
                            };
                            if !face.is_empty() {
                                let mut txt = face.to_string();
                                if m.state == AprsMsgState::Sent && m.tries > 1 {
                                    txt = format!("↑{}", m.tries);
                                }
                                ui.label(RichText::new(txt).size(10.0).color(colour))
                                    .on_hover_text(tip);
                            }
                        } else if !m.id.is_empty()
                            && crate::chrome::chip(ui, false, RichText::new("reply").size(9.0))
                                .clicked()
                        {
                            self.aprs_target = m.from.clone();
                        }
                    });
                }
            });

        ui.add_space(2.0);
        let mut send = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let to = ui.add(
                egui::TextEdit::singleline(&mut self.aprs_target)
                    .hint_text("to")
                    .desired_width(78.0)
                    .font(egui::TextStyle::Monospace),
            );
            if to.changed() {
                self.aprs_target = self.aprs_target.to_ascii_uppercase();
            }
            let room = (ui.available_width() - 56.0).max(60.0);
            let text = ui.add(
                egui::TextEdit::singleline(&mut self.aprs_draft)
                    .hint_text("message")
                    .desired_width(room)
                    .char_limit(MSG_MAX),
            );
            if text.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                send = true;
            }
            let ready = !self.aprs_target.trim().is_empty() && !self.aprs_draft.trim().is_empty();
            if tx_gated(ui, tx_ok && ready, |ui| {
                crate::chrome::chip_accent(
                    ui,
                    false,
                    RichText::new(" SEND ").size(10.0).strong(),
                    theme::GREEN(),
                    theme::INK_ON_CYAN(),
                )
                .on_hover_text(
                    "Send it, and keep retrying until the far end acknowledges. Messages are \
                     the one thing on this channel that is answered.",
                )
            })
            .clicked()
            {
                send = true;
            }
        });
        if send
            && !self.aprs_target.trim().is_empty()
            && !self.aprs_draft.trim().is_empty()
            && tx_ok
        {
            cmds.push(Command::AprsSendMessage {
                to: self.aprs_target.trim().to_ascii_uppercase(),
                text: self.aprs_draft.trim().to_string(),
            });
            self.aprs_draft.clear();
        }
    }
}
