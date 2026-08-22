//! The two operator overlays opened from the top bar's WIN group: the memory
//! channel list and the voice keyer.
//!
//! Both are thin views over engine state — the memories come from the radio
//! backend, the keyer slots from the audio engine — so all either does is
//! draw the list and push the [`Command`] a click means.

use eframe::egui::{self, Color32, RichText};
use sdroxide_types::{Command, MemoryChannel, MemoryFolder};

use crate::app::SdroxideApp;

/// What a drag out of the memory list carries: the memory's id. Its own type
/// so no unrelated drag could ever be mistaken for one.
struct DraggedMemory(u32);

/// Wrap `add` in a frame that accepts a dragged memory, and say which memory
/// was dropped on it this frame, if any. While a drag is live every target
/// shows a faint outline, and the one under the pointer answers in cyan —
/// hand-rolled rather than `dnd_drop_zone`, which repaints the frame in the
/// widget palette and would put a button-grey slab behind every folder.
fn mem_drop_target<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> (R, Option<u32>) {
    let dragging = egui::DragAndDrop::has_payload_of_type::<DraggedMemory>(ui.ctx());
    let out = egui::Frame::default().inner_margin(4.0).corner_radius(4.0).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        add(ui)
    });
    if dragging {
        let stroke = if out.response.dnd_hover_payload::<DraggedMemory>().is_some() {
            egui::Stroke::new(1.5, crate::theme::CYAN())
        } else {
            egui::Stroke::new(1.0, crate::theme::LINE_LIT())
        };
        ui.painter().rect_stroke(out.response.rect, 4.0, stroke, egui::StrokeKind::Inside);
    }
    let dropped = out.response.dnd_release_payload::<DraggedMemory>().map(|p| p.0);
    (out.inner, dropped)
}

/// One memory: recall, name/frequency/mode, delete. The label is the drag
/// handle — deliberately not the whole row, whose drag sense would sit over
/// the buttons and turn a sloppy RCL or DEL press into a drag.
fn memory_row(ui: &mut egui::Ui, m: &MemoryChannel, cmds: &mut Vec<Command>) {
    ui.horizontal(|ui| {
        if crate::chrome::chip(ui, false, "RCL").on_hover_text("Recall").clicked() {
            cmds.push(Command::RecallMemory(m.id));
        }
        ui.dnd_drag_source(
            crate::layout::salted_id(ui.ctx(), "mem-drag").with(m.id),
            DraggedMemory(m.id),
            |ui| {
                // An RTTY memory recalls its modem setup with it; show that setup
                // so two memories on the same dial read as the different stations
                // they are (f32's Display keeps 45.45 as-is and 170.0 as "170").
                let rtty = m.rtty.map_or(String::new(), |r| {
                    format!(" {}/{}{}", r.baud, r.shift_hz, if r.reverse { " R" } else { "" })
                });
                ui.label(
                    RichText::new(format!(
                        "{:<12} {:>12.6} MHz  {}{}",
                        m.name,
                        m.freq_hz / 1e6,
                        m.mode.label(),
                        rtty
                    ))
                    .monospace(),
                );
            },
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if crate::chrome::chip_accent(
                ui,
                false,
                RichText::new("DEL").size(11.0),
                crate::theme::PINK(),
                Color32::WHITE,
            )
            .on_hover_text("Delete")
            .clicked()
            {
                cmds.push(Command::DeleteMemory(m.id));
            }
        });
    });
}

impl SdroxideApp {
    pub(in crate::app) fn memories_window(&mut self, ctx: &egui::Context, cmds: &mut Vec<Command>) {
        let mut open = self.show_memories;
        let resp = egui::Window::new("Memories")
            .id(crate::layout::salted_id(ctx, "Memories"))
            .open(&mut open)
            .frame(crate::chrome::window_frame())
            .resizable(true)
            .default_width(crate::layout::window_w(ctx, 400.0))
            // A real starting height, and a floor under what egui remembers:
            // the window used to hug its (often short) list and come up a few
            // rows tall. See the voice keyer below for why the minimum matters
            // as much as the default.
            .default_height(crate::layout::window_h(ctx, 460.0))
            .min_height(crate::layout::window_h(ctx, 280.0))
            .show(ctx, |ui| {
                crate::chrome::window_body_bg(ui);
                self.memories_ui(ui, cmds)
            });
        if let Some(r) = &resp {
            crate::chrome::paint_window_border(ctx, &r.response);
        }
        self.show_memories = open;
    }

    fn memories_ui(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        ui.horizontal(|ui| {
            crate::chrome::field(
                ui,
                egui::TextEdit::singleline(&mut self.mem_name)
                    .hint_text("memory name")
                    .desired_width(ui.available_width() - 100.0),
            );
            let name_ok = !self.mem_name.trim().is_empty();
            if ui.add_enabled(name_ok, egui::Button::new("Store")).clicked() {
                cmds.push(Command::StoreMemory { name: self.mem_name.trim().to_string() });
                self.mem_name.clear();
            }
        });
        ui.horizontal(|ui| {
            crate::chrome::field(
                ui,
                egui::TextEdit::singleline(&mut self.mem_folder_name)
                    .hint_text("folder name")
                    .desired_width(ui.available_width() - 100.0),
            );
            let name_ok = !self.mem_folder_name.trim().is_empty();
            if ui.add_enabled(name_ok, egui::Button::new("New folder")).clicked() {
                cmds.push(Command::CreateMemoryFolder {
                    name: self.mem_folder_name.trim().to_string(),
                });
                self.mem_folder_name.clear();
            }
        });
        ui.separator();
        if self.memories.is_empty() && self.mem_folders.is_empty() {
            ui.label(RichText::new("no memories yet").color(crate::theme::gray(120)));
        } else if !self.mem_folders.is_empty() {
            ui.label(
                RichText::new("drag a memory onto a folder to file it, below them to unfile it")
                    .weak()
                    .size(11.0),
            );
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            // While a memory is being dragged, holding it near the top or
            // bottom edge crawls the list, so a folder outside the visible
            // area can still be reached. Instant scrolling (no animation):
            // this runs every frame, and easing would fight the next frame's
            // delta. The repaint request keeps it crawling while the pointer
            // sits still at the edge, which is the whole gesture.
            if egui::DragAndDrop::has_payload_of_type::<DraggedMemory>(ui.ctx())
                && let Some(pos) = ui.ctx().pointer_interact_pos()
            {
                const MARGIN: f32 = 28.0;
                const SPEED: f32 = 600.0; // pt/s right at the edge
                let view = ui.clip_rect();
                if pos.x >= view.left() - 40.0 && pos.x <= view.right() + 40.0 {
                    let step = SPEED * ui.input(|i| i.stable_dt).min(0.1);
                    let delta = if pos.y < view.top() + MARGIN {
                        ((view.top() + MARGIN - pos.y) / MARGIN).min(1.0) * step
                    } else if pos.y > view.bottom() - MARGIN {
                        -((pos.y - (view.bottom() - MARGIN)) / MARGIN).min(1.0) * step
                    } else {
                        0.0
                    };
                    if delta != 0.0 {
                        ui.scroll_with_delta_animation(
                            egui::vec2(0.0, delta),
                            egui::style::ScrollAnimation::none(),
                        );
                        ui.ctx().request_repaint();
                    }
                }
            }
            let folders = self.mem_folders.clone();
            for f in &folders {
                self.folder_section(ui, f, cmds);
            }
            // The top level: everything unfiled — including anything whose
            // folder has gone from under it — and, while a drag is live, the
            // place to drop a memory to take it out of its folder.
            let (_, dropped) = mem_drop_target(ui, |ui| {
                let known = |id: u32| folders.iter().any(|f| f.id == id);
                let mut any = false;
                for m in &self.memories {
                    if m.folder.is_none_or(|id| !known(id)) {
                        memory_row(ui, m, cmds);
                        any = true;
                    }
                }
                if !any && egui::DragAndDrop::has_payload_of_type::<DraggedMemory>(ui.ctx()) {
                    ui.label(RichText::new("drop here to unfile").weak().size(11.0));
                }
            });
            if let Some(id) = dropped
                && self.memories.iter().any(|m| m.id == id && m.folder.is_some())
            {
                cmds.push(Command::MoveMemoryToFolder { id, folder: None });
            }
        });
    }

    /// One folder of the memory list: a collapsible section whose header
    /// carries the rename and delete controls, the whole of it a drop target.
    fn folder_section(&mut self, ui: &mut egui::Ui, f: &MemoryFolder, cmds: &mut Vec<Command>) {
        let count = self.memories.iter().filter(|m| m.folder == Some(f.id)).count();
        let (_, dropped) = mem_drop_target(ui, |ui| {
            let state = egui::collapsing_header::CollapsingState::load_with_default_open(
                ui.ctx(),
                ui.make_persistent_id(("mem-folder", f.id)),
                true,
            );
            state
                .show_header(ui, |ui| {
                    let editing = matches!(&self.mem_folder_edit, Some((id, _)) if *id == f.id);
                    if editing {
                        let (_, text) = self.mem_folder_edit.as_mut().expect("checked above");
                        let edit = crate::chrome::field(
                            ui,
                            egui::TextEdit::singleline(text).desired_width(150.0),
                        );
                        if self.mem_folder_focus {
                            edit.request_focus();
                            self.mem_folder_focus = false;
                        }
                        // Escape abandons the edit; Enter or clicking away
                        // commits it (Enter surrenders focus in egui).
                        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            self.mem_folder_edit = None;
                        } else if edit.lost_focus() {
                            let (id, name) = self.mem_folder_edit.take().expect("checked above");
                            let name = name.trim().to_string();
                            if !name.is_empty() && name != f.name {
                                cmds.push(Command::RenameMemoryFolder { id, name });
                            }
                        }
                    } else {
                        ui.label(RichText::new(&f.name).strong());
                        ui.label(RichText::new(format!("({count})")).weak().size(11.0));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if crate::chrome::chip_accent(
                            ui,
                            false,
                            RichText::new("DEL").size(11.0),
                            crate::theme::PINK(),
                            Color32::WHITE,
                        )
                        .on_hover_text("Delete folder — its memories move to the top level")
                        .clicked()
                        {
                            cmds.push(Command::DeleteMemoryFolder(f.id));
                        }
                        if crate::chrome::chip(ui, false, RichText::new("REN").size(11.0))
                            .on_hover_text("Rename folder")
                            .clicked()
                        {
                            self.mem_folder_edit = Some((f.id, f.name.clone()));
                            self.mem_folder_focus = true;
                        }
                    });
                })
                .body(|ui| {
                    if count == 0 {
                        ui.label(RichText::new("empty — drop memories here").weak().size(11.0));
                    }
                    for m in &self.memories {
                        if m.folder == Some(f.id) {
                            memory_row(ui, m, cmds);
                        }
                    }
                });
        });
        if let Some(id) = dropped
            && self.memories.iter().any(|m| m.id == id && m.folder != Some(f.id))
        {
            cmds.push(Command::MoveMemoryToFolder { id, folder: Some(f.id) });
        }
    }

    /// The voice keyer: ten recorded messages with record / transmit / erase
    /// per slot.
    ///
    /// Everything the window shows comes from the engine (it owns the
    /// recordings and the transmitter), so the buttons only ever send commands
    /// — there is no local latch that could disagree with what is on the air.
    pub(in crate::app) fn voice_window(&mut self, ctx: &egui::Context, cmds: &mut Vec<Command>) {
        // Entering a digital mode other than RADE takes the feature away; the
        // window goes with it rather than sitting there doing nothing.
        if !self.state.rx[0].mode.allows_voice_keyer() {
            self.show_voice = false;
            return;
        }
        let mut open = self.show_voice;
        let recording = self.voice.recording;
        let playing = self.voice.playing;
        let previewing = self.voice.previewing;
        let pos = self.voice.position_s;
        let max_len = self.voice.max_len_s;
        // TUNE holds the transmitter at the tune level, so a message would go
        // nowhere; the engine refuses, and the buttons say so up front.
        let tuning = self.state.tx.tune;
        let slots: Vec<sdroxide_types::VoiceSlotInfo> = self.voice.slots.clone();

        let resp = egui::Window::new("Voice keyer")
            .id(crate::layout::salted_id(ctx, "Voice keyer"))
            .open(&mut open)
            .frame(crate::chrome::window_frame())
            .resizable(false)
            // `min_width` as well as `default_width`: the default only applies
            // the first time the window is ever shown, and egui persists its
            // size — without the minimum, a build that shipped a narrower
            // window would keep squeezing the slot-name fields forever.
            //
            // Both are held inside the viewport, for the same reason in
            // reverse: a keyer opened at 600 pt on a desktop would otherwise
            // stay 600 pt wide on the phone that later loads the same storage,
            // with a third of it off the side of the screen.
            .default_width(crate::layout::window_w(ctx, 600.0))
            .min_width(crate::layout::window_w(ctx, 600.0))
            .show(ctx, |ui| {
                crate::chrome::window_body_bg(ui);
                ui.label(
                    RichText::new(
                        "REC records from your microphone, PLAY lets you listen to what you \
                         recorded, TX puts it on the air — as does a numpad key, a MIDI pad, \
                         or rigctld's send_voice_mem.",
                    )
                    .weak()
                    .size(11.5),
                );
                ui.add_space(6.0);
                egui::Grid::new("voice-grid")
                    .num_columns(6)
                    .spacing([8.0, 6.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for (i, slot) in slots.iter().enumerate() {
                            let is_rec = recording == Some(i as u8);
                            let is_play = playing == Some(i as u8);
                            let is_prev = previewing == Some(i as u8);

                            ui.label(
                                RichText::new(format!("{:>2}", i + 1))
                                    .monospace()
                                    .color(crate::theme::CYAN_DIM()),
                            );

                            // The slot label. Only the row being typed into is
                            // UI-owned; every other row shows the engine's copy.
                            let mut text = match &self.voice_name_edit {
                                Some((row, s)) if *row == i => s.clone(),
                                _ => slot.name.clone(),
                            };
                            // `add_sized`, not `desired_width`: inside a Grid a
                            // desired width is clamped by the column width egui
                            // measured (and persisted) last frame, so a field
                            // that once came up narrow would stay narrow.
                            let edit = crate::chrome::field_sized(
                                ui,
                                [190.0, 20.0],
                                egui::TextEdit::singleline(&mut text)
                                    .hint_text(format!("Slot {}", i + 1)),
                            );
                            if edit.changed() {
                                self.voice_name_edit = Some((i, text.clone()));
                            }
                            if edit.lost_focus()
                                && let Some((row, name)) = self.voice_name_edit.take()
                                && row == i
                            {
                                cmds.push(Command::VoiceRename { slot: i as u8, name });
                            }

                            // REC — starts/stops recording this slot. Refused
                            // while the transmitter is up (same microphone).
                            let busy_elsewhere = (recording.is_some() && !is_rec)
                                || playing.is_some()
                                || previewing.is_some()
                                || self.state.tx.ptt
                                || tuning;
                            let rec = ui
                                .add_enabled_ui(!busy_elsewhere, |ui| {
                                    crate::chrome::chip_accent(
                                        ui,
                                        is_rec,
                                        RichText::new("REC").size(11.5),
                                        crate::theme::ALERT(),
                                        Color32::WHITE,
                                    )
                                })
                                .inner
                                .on_hover_text(if is_rec {
                                    "Stop and store".to_string()
                                } else {
                                    format!("Record from the microphone (up to {max_len:.0} s)")
                                });
                            if rec.clicked() {
                                cmds.push(Command::VoiceRecord(if is_rec {
                                    None
                                } else {
                                    Some(i as u8)
                                }));
                            }

                            // PLAY — listen to the message locally. Nothing goes
                            // on the air, so this is safe to press any time the
                            // receiver is running.
                            let can_prev = !slot.is_empty()
                                && recording.is_none()
                                && !self.state.tx.ptt
                                && !tuning
                                && (is_prev || previewing.is_none());
                            let prev = ui
                                .add_enabled_ui(can_prev || is_prev, |ui| {
                                    crate::chrome::chip(
                                        ui,
                                        is_prev,
                                        RichText::new(if is_prev { "STOP" } else { "PLAY" })
                                            .size(11.5),
                                    )
                                })
                                .inner
                                .on_hover_text(if is_prev {
                                    "Stop listening"
                                } else if slot.is_empty() {
                                    "Nothing recorded in this slot"
                                } else if self.state.tx.ptt || tuning {
                                    "Not while transmitting"
                                } else {
                                    "Listen to this message — nothing is transmitted"
                                });
                            if prev.clicked() {
                                cmds.push(if is_prev {
                                    Command::VoicePreview(None)
                                } else {
                                    Command::VoicePreview(Some(i as u8))
                                });
                            }

                            // TX — puts the message on the air.
                            let can_play = !slot.is_empty()
                                && recording.is_none()
                                && !tuning
                                && (is_play || playing.is_none());
                            let play = ui
                                .add_enabled_ui(can_play || is_play, |ui| {
                                    crate::chrome::chip_accent(
                                        ui,
                                        is_play,
                                        RichText::new(if is_play { "STOP" } else { "TX" })
                                            .size(11.5),
                                        crate::theme::ALERT(),
                                        Color32::WHITE,
                                    )
                                })
                                .inner
                                .on_hover_text(if is_play {
                                    "Stop transmitting"
                                } else if slot.is_empty() {
                                    "Nothing recorded in this slot"
                                } else if tuning {
                                    "TUNE is active — switch it off first"
                                } else {
                                    "Transmit this message"
                                });
                            if play.clicked() {
                                cmds.push(if is_play {
                                    Command::VoicePlay(None)
                                } else {
                                    Command::VoicePlay(Some(i as u8))
                                });
                            }

                            // Length, or the running position of whichever of
                            // record / listen / transmit this row owns.
                            ui.horizontal(|ui| {
                                let (text, colour) = if is_rec {
                                    (format!("● {pos:.1} s"), crate::theme::ALERT())
                                } else if is_play {
                                    (
                                        format!("▶ {pos:.1} / {:.1} s", slot.len_s),
                                        crate::theme::ALERT(),
                                    )
                                } else if is_prev {
                                    (
                                        format!("▶ {pos:.1} / {:.1} s", slot.len_s),
                                        crate::theme::CYAN(),
                                    )
                                } else if slot.is_empty() {
                                    ("—".to_string(), crate::theme::gray(110))
                                } else {
                                    (format!("{:.1} s", slot.len_s), crate::theme::gray(170))
                                };
                                ui.add_sized(
                                    [88.0, 18.0],
                                    egui::Label::new(
                                        RichText::new(text).monospace().size(11.5).color(colour),
                                    )
                                    .selectable(false),
                                );
                                let erasable = !slot.is_empty() && !is_rec && !is_play && !is_prev;
                                if ui
                                    .add_enabled_ui(erasable, |ui| {
                                        crate::chrome::chip_accent(
                                            ui,
                                            false,
                                            RichText::new("DEL").size(11.0),
                                            crate::theme::PINK(),
                                            Color32::WHITE,
                                        )
                                    })
                                    .inner
                                    .on_hover_text("Erase this recording")
                                    .clicked()
                                {
                                    cmds.push(Command::VoiceClear(i as u8));
                                }
                            });
                            ui.end_row();
                        }
                    });

                if self.state.rx[0].mode.is_rade() {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "RADE: the message is encoded by the digital-voice codec, \
                             exactly as a live over would be.",
                        )
                        .weak()
                        .size(11.0),
                    );
                }
            });
        if let Some(r) = &resp {
            crate::chrome::paint_window_border(ctx, &r.response);
        }
        // Keep the position readout moving while something is running; the app
        // otherwise idles between spectrum frames.
        if self.voice.busy() {
            crate::repaint::after_ms(ctx, 100);
        }
        self.show_voice = open;
    }
}
