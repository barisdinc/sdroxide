//! The AtCHAT NET panel: the roster, the conversation, the transfers in flight
//! and a viewer for the images that have arrived over the air.
//!
//! Two panes an operator watches move independently: CHAT is who is on the net
//! and what has been said (common channel or a directed line to one station);
//! FILES is the block-CRC-ARQ transfers still running and the received-image
//! viewer, which shows who sent each picture and when, with ◀ ▶ between them.
//!
//! The mode runs a whole NET protocol station on its own thread — master
//! election, roster ageing, ARQ — so this panel only reads
//! [`sdroxide_types::AtChatStatus`] and pushes the four `Command::AtChat*`.

use eframe::egui::{self, RichText};
use sdroxide_types::{AtChatStatus, Command};

use crate::app::{SdroxideApp, tx_gated};
use crate::theme;

impl SdroxideApp {
    pub(in crate::app) fn atchat_panel(
        &mut self,
        ui: &mut egui::Ui,
        cmds: &mut Vec<Command>,
        panel_h: f32,
    ) {
        let st: Option<Box<AtChatStatus>> =
            self.digi_status.as_ref().and_then(|s| s.atchat.clone());
        let Some(st) = st else {
            ui.label(RichText::new("starting the AtCHAT NET station…").weak());
            return;
        };
        let tx_ok = self.tx_capable();

        self.atchat_header(ui, cmds, &st);
        ui.add_space(6.0);

        let pane = self.phone_pane(ui, sdroxide_types::Mode::AtChat);
        ui.horizontal_top(|ui| {
            if pane.is_none_or(|p| p == 0) {
                ui.vertical(|ui| {
                    if pane.is_none() {
                        ui.set_width(ui.available_width() * 0.56);
                    }
                    self.atchat_chat_pane(ui, cmds, &st, panel_h, tx_ok);
                });
            }
            if pane.is_none() {
                ui.separator();
            }
            if pane.is_none_or(|p| p == 1) {
                ui.vertical(|ui| {
                    self.atchat_files_pane(ui, cmds, &st, panel_h, tx_ok);
                });
            }
        });
    }

    /// Title, join state, role, master, carrier, and the virtual-channel field.
    fn atchat_header(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>, st: &AtChatStatus) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("ATCHAT").size(11.0).strong().color(theme::CYAN()));
            ui.label(
                RichText::new("NET — multi-station keyboard & file")
                    .size(10.5)
                    .color(theme::CYAN_DIM()),
            );

            if st.connected {
                ui.label(RichText::new(" ON NET ").size(10.5).strong().color(theme::GREEN()));
            } else {
                ui.label(RichText::new(" JOINING ").size(10.5).strong().color(theme::YELLOW()));
            }
            if let Some(role) = &st.role {
                let colour = match role.as_str() {
                    "MASTER" => theme::GREEN(),
                    "BACKUP" => theme::YELLOW(),
                    _ => theme::gray(140),
                };
                ui.label(RichText::new(role).size(10.5).strong().color(colour));
            }
            if let Some(m) = &st.master {
                ui.label(RichText::new(format!("master {m}")).size(10.5).color(theme::gray(150)));
            }
            if st.carrier {
                ui.label(RichText::new("● CARRIER").size(10.5).color(theme::YELLOW()))
                    .on_hover_text("Another station is transmitting — the channel is busy.");
            }
            if st.keyed {
                ui.label(RichText::new("● TX").size(10.5).strong().color(theme::ALERT()));
            }

            crate::chrome::row_tail(ui, |ui| {
                if crate::chrome::chip(ui, self.show_digi_settings, RichText::new("⚙ SETUP").size(9.5))
                    .on_hover_text("Station callsign and the virtual-channel address")
                    .clicked()
                {
                    self.show_digi_settings = !self.show_digi_settings;
                }
            });
        });

        // Virtual channel: a channel_server-compatible TCP endpoint that stands
        // in for the RF path, for developing and testing without a radio.
        ui.horizontal_wrapped(|ui| {
            let on = self.digi_cfg_edit.atchat_virtual;
            let resp = crate::chrome::chip(ui, on, RichText::new("VIRTUAL CHANNEL").size(10.0));
            if resp.clicked() && self.digi_cfg_seeded {
                self.digi_cfg_edit.atchat_virtual = !on;
                cmds.push(Command::SetDigiConfig(self.digi_cfg_edit.clone()));
            }
            resp.on_hover_text(
                "Work the net over a TCP endpoint instead of the radio — a \
                 channel_server-compatible loopback for radio-less development. \
                 Off puts the station on the air.",
            );
            ui.add_enabled_ui(on, |ui| {
                let r = crate::chrome::field(
                    ui,
                    egui::TextEdit::singleline(&mut self.digi_cfg_edit.atchat_virtual_addr)
                        .desired_width(140.0)
                        .hint_text("127.0.0.1:6000"),
                );
                if r.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    && self.digi_cfg_seeded
                {
                    cmds.push(Command::SetDigiConfig(self.digi_cfg_edit.clone()));
                }
            });
            if on {
                ui.label(
                    RichText::new(st.virtual_addr.as_deref().unwrap_or("—"))
                        .size(9.5)
                        .color(theme::gray(130)),
                );
            }
        });
    }

    /// The roster strip and the conversation.
    fn atchat_chat_pane(
        &mut self,
        ui: &mut egui::Ui,
        cmds: &mut Vec<Command>,
        st: &AtChatStatus,
        panel_h: f32,
        tx_ok: bool,
    ) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("ROSTER").strong().size(10.5).color(theme::CYAN()));
            if st.roster.is_empty() {
                ui.label(RichText::new("nobody heard yet").weak());
            }
            for r in &st.roster {
                let colour = if r.status == "active" { theme::GREEN() } else { theme::gray(110) };
                let face = RichText::new(&r.call).monospace().size(10.5).color(colour);
                if ui
                    .add(egui::Label::new(face).sense(egui::Sense::click()))
                    .on_hover_text(format!("{} — last heard {:.0}s ago", r.status, r.age_s))
                    .clicked()
                {
                    self.atchat_dst = r.call.clone();
                }
            }
        });
        ui.add_space(4.0);

        let input_h = 30.0;
        egui::ScrollArea::vertical()
            .id_salt("atchat-chat")
            .max_height((panel_h - 96.0 - input_h).max(60.0))
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if st.chat.is_empty() {
                    ui.label(RichText::new("No messages yet.").weak());
                }
                for c in &st.chat {
                    let when = hms(c.when);
                    let who = if c.own { "me".to_string() } else { c.from.clone() };
                    let tag = if c.private {
                        format!("[{}→{}]", who, if c.own { c.dst.clone() } else { "me".into() })
                    } else {
                        format!("<{who}>")
                    };
                    let colour = if c.own {
                        theme::GREEN()
                    } else if c.private {
                        theme::YELLOW()
                    } else {
                        theme::TEXT()
                    };
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(when).monospace().size(9.5).color(theme::gray(110)));
                        ui.label(RichText::new(tag).monospace().size(10.5).color(colour));
                        ui.label(RichText::new(&c.text).size(11.0).color(colour));
                    });
                }
            });

        // Destination + the line to type on.
        let mut send = false;
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("atchat-dst")
                .selected_text(if self.atchat_dst.is_empty() {
                    "ALL".to_string()
                } else {
                    self.atchat_dst.clone()
                })
                .width(74.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.atchat_dst, String::new(), "ALL");
                    for r in &st.roster {
                        ui.selectable_value(&mut self.atchat_dst, r.call.clone(), &r.call);
                    }
                });
            let room = (ui.available_width() - 52.0).max(80.0);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.atchat_draft)
                    .desired_width(room)
                    .hint_text(if self.atchat_dst.is_empty() {
                        "message to everyone"
                    } else {
                        "private message"
                    }),
            );
            send |= resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if tx_gated(ui, tx_ok, |ui| {
                crate::chrome::chip_accent(
                    ui,
                    false,
                    RichText::new(" SEND ").size(10.0).strong(),
                    theme::GREEN(),
                    theme::INK_ON_CYAN(),
                )
            })
            .clicked()
            {
                send = true;
            }
        });

        if send && tx_ok && !self.atchat_draft.trim().is_empty() {
            cmds.push(Command::AtChatSendChat {
                to: self.atchat_dst.trim().to_string(),
                text: self.atchat_draft.trim().to_string(),
            });
            self.atchat_draft.clear();
        }
    }

    /// The transfers in flight and the received-image viewer.
    fn atchat_files_pane(
        &mut self,
        ui: &mut egui::Ui,
        cmds: &mut Vec<Command>,
        st: &AtChatStatus,
        panel_h: f32,
        tx_ok: bool,
    ) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("TRANSFERS").strong().size(10.5).color(theme::CYAN()));
            crate::chrome::row_tail(ui, |ui| {
                if tx_gated(ui, tx_ok, |ui| {
                    crate::chrome::chip(ui, false, RichText::new(" SEND FILE ").size(10.0))
                })
                .on_hover_text("Send a file or image over the air, block-CRC-ARQ")
                .clicked()
                    && let Some(path) = rfd::FileDialog::new().pick_file()
                {
                    cmds.push(Command::AtChatSendFile {
                        to: self.atchat_dst.trim().to_string(),
                        path: path.to_string_lossy().into_owned(),
                    });
                }
            });
        });

        egui::ScrollArea::vertical()
            .id_salt("atchat-transfers")
            .max_height((panel_h * 0.32).max(48.0))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if st.transfers.is_empty() {
                    ui.label(RichText::new("nothing in flight").weak());
                }
                for t in &st.transfers {
                    let dir = if t.incoming { "◀" } else { "▶" };
                    let frac = if t.total == 0 {
                        0.0
                    } else {
                        t.have as f32 / t.total as f32
                    };
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("{dir} {}", t.filename))
                                .monospace()
                                .size(10.0)
                                .color(if t.complete { theme::GREEN() } else { theme::TEXT() }),
                        );
                        ui.label(
                            RichText::new(format!("{} {}/{}", t.peer, t.have, t.total))
                                .size(9.5)
                                .color(theme::gray(130)),
                        );
                    });
                    ui.add(egui::ProgressBar::new(frac).desired_height(4.0));
                }
            });

        ui.add_space(6.0);
        ui.label(RichText::new("IMAGES").strong().size(10.5).color(theme::CYAN()));

        // Only the images, oldest first — the viewer walks this subset.
        let images: Vec<&sdroxide_types::AtChatFile> =
            st.files.iter().filter(|f| f.is_image).collect();
        if images.is_empty() {
            ui.label(RichText::new("No images received.").weak());
            return;
        }
        if self.atchat_img_at >= images.len() {
            self.atchat_img_at = images.len() - 1;
        }

        ui.horizontal(|ui| {
            if ui.add_enabled(self.atchat_img_at > 0, egui::Button::new("◀")).clicked() {
                self.atchat_img_at -= 1;
            }
            ui.label(
                RichText::new(format!("{}/{}", self.atchat_img_at + 1, images.len()))
                    .size(10.0)
                    .color(theme::gray(140)),
            );
            if ui
                .add_enabled(self.atchat_img_at + 1 < images.len(), egui::Button::new("▶"))
                .clicked()
            {
                self.atchat_img_at += 1;
            }
            let f = images[self.atchat_img_at];
            ui.label(
                RichText::new(format!("{}  ·  {}  ·  {}", f.filename, f.from, hms(f.when)))
                    .size(10.0)
                    .color(theme::TEXT_STRONG()),
            );
        });

        let f = images[self.atchat_img_at];
        let tex = self.atchat_image_texture(ui.ctx(), &f.path);
        match tex {
            Some(tex) => {
                let avail = ui.available_size();
                let [w, h] = tex.size();
                let scale = (avail.x / w as f32).min((panel_h * 0.42) / h as f32).min(1.0);
                ui.add(
                    egui::Image::new(&tex)
                        .fit_to_exact_size(egui::vec2(w as f32 * scale, h as f32 * scale)),
                );
            }
            None => {
                ui.label(
                    RichText::new(format!("cannot display {}", f.path))
                        .size(10.0)
                        .color(theme::YELLOW()),
                );
            }
        }
    }

    /// Decode a received image from disk once and keep it as a texture.
    fn atchat_image_texture(
        &mut self,
        ctx: &egui::Context,
        path: &str,
    ) -> Option<egui::TextureHandle> {
        if let Some(slot) = self.atchat_img_cache.get(path) {
            return slot.clone();
        }
        let handle = std::fs::read(path)
            .ok()
            .and_then(|bytes| image::load_from_memory(&bytes).ok())
            .map(|img| {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                let ci = egui::ColorImage::from_rgba_unmultiplied(
                    [w as usize, h as usize],
                    rgba.as_raw(),
                );
                ctx.load_texture("atchat-image", ci, egui::TextureOptions::LINEAR)
            });
        self.atchat_img_cache.insert(path.to_string(), handle.clone());
        handle
    }
}

/// Unix seconds → `HH:MM:SS` UTC.
fn hms(unix: u64) -> String {
    let (_, _, _, h, mi, s) = sdroxide_types::utc_ymd_hms(unix as i64);
    format!("{h:02}:{mi:02}:{s:02}")
}
