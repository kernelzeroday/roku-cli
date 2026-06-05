use eframe::egui;
use roku_cli::config::Config;
use roku_cli::discover;
use roku_cli::ecp::Roku;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([380.0, 580.0])
            .with_min_inner_size([340.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Roku Remote",
        options,
        Box::new(|_cc| Ok(Box::new(RokuRemote::new()))),
    )
}

struct RokuRemote {
    roku: Option<Roku>,
    ip_input: String,
    text_input: String,
    status: String,
}

impl RokuRemote {
    fn new() -> Self {
        let mut app = Self {
            roku: None,
            ip_input: String::new(),
            text_input: String::new(),
            status: "Disconnected".into(),
        };
        if let Ok(ip) = std::env::var("ROKU_IP") {
            if !ip.is_empty() {
                app.ip_input = ip.clone();
                app.roku = Some(Roku::new(&ip));
                app.status = format!("Ready: {}", ip);
                return app;
            }
        }
        if let Ok(cfg) = Config::load() {
            if let Some(ip) = cfg.roku_ip {
                app.ip_input = ip.clone();
                app.roku = Some(Roku::new(&ip));
                app.status = format!("Ready: {}", ip);
            }
        }
        app
    }

    fn connect(&mut self) {
        let ip = self.ip_input.trim().to_string();
        if ip.is_empty() {
            self.status = "Enter an IP address".into();
            return;
        }
        self.roku = Some(Roku::new(&ip));
        self.status = format!("Ready: {}", ip);
        if let Ok(mut cfg) = Config::load() {
            cfg.roku_ip = Some(ip);
            let _ = cfg.save();
        }
    }

    fn send(&mut self, key: &str) {
        if let Some(roku) = &self.roku {
            match roku.keypress(key) {
                Ok(()) => self.status = format!("Sent: {}", key),
                Err(e) => self.status = format!("Error: {}", e),
            }
        } else {
            self.status = "Not connected".into();
        }
    }

    fn send_text(&mut self) {
        let text = self.text_input.clone();
        if text.is_empty() {
            return;
        }
        if let Some(roku) = &self.roku {
            match roku.type_text(&text) {
                Ok(()) => {
                    self.status = format!("Typed: {}", text);
                    self.text_input.clear();
                }
                Err(e) => self.status = format!("Error: {}", e),
            }
        } else {
            self.status = "Not connected".into();
        }
    }
}

fn remote_btn(label: &str, size: [f32; 2]) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(label).size(16.0))
        .min_size(egui::vec2(size[0], size[1]))
}

impl eframe::App for RokuRemote {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut text_focused = false;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);

            ui.horizontal(|ui| {
                ui.label("IP:");
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.ip_input).desired_width(140.0),
                );
                if r.has_focus() {
                    text_focused = true;
                }
                if ui.button("Connect").clicked()
                    || (r.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)))
                {
                    self.connect();
                }
                if ui.button("Discover").clicked() {
                    self.status = "Scanning...".into();
                    match discover::discover(3) {
                        Ok(found) => {
                            if let Some(f) = found.into_iter().next() {
                                self.ip_input = f.ip.clone();
                                self.roku = Some(Roku::new(&f.ip));
                                self.status = format!("Found: {}", f.ip);
                                if let Ok(mut cfg) = Config::load() {
                                    cfg.roku_ip = Some(f.ip);
                                    let _ = cfg.save();
                                }
                            } else {
                                self.status = "No Roku found".into();
                            }
                        }
                        Err(e) => self.status = format!("Discovery error: {}", e),
                    }
                }
            });

            ui.separator();

            ui.horizontal(|ui| {
                if ui.add(remote_btn("Power", [80.0, 32.0])).clicked() {
                    self.send("Power");
                }
                let space = ui.available_width() - 160.0;
                ui.add_space(space.max(8.0));
                if ui.add(remote_btn("Home", [72.0, 32.0])).clicked() {
                    self.send("Home");
                }
                if ui.add(remote_btn("Back", [72.0, 32.0])).clicked() {
                    self.send("Back");
                }
            });

            ui.add_space(8.0);

            ui.vertical_centered(|ui| {
                egui::Grid::new("dpad")
                    .spacing([4.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("");
                        if ui.add(remote_btn("Up", [64.0, 38.0])).clicked() {
                            self.send("Up");
                        }
                        ui.label("");
                        ui.end_row();

                        if ui.add(remote_btn("Left", [64.0, 38.0])).clicked() {
                            self.send("Left");
                        }
                        if ui.add(remote_btn("OK", [64.0, 38.0])).clicked() {
                            self.send("Select");
                        }
                        if ui.add(remote_btn("Right", [64.0, 38.0])).clicked() {
                            self.send("Right");
                        }
                        ui.end_row();

                        ui.label("");
                        if ui.add(remote_btn("Down", [64.0, 38.0])).clicked() {
                            self.send("Down");
                        }
                        ui.label("");
                        ui.end_row();
                    });
            });

            ui.add_space(8.0);

            ui.vertical_centered(|ui| {
                ui.horizontal(|ui| {
                    if ui.add(remote_btn("Rev", [64.0, 36.0])).clicked() {
                        self.send("Rev");
                    }
                    if ui.add(remote_btn("Play", [64.0, 36.0])).clicked() {
                        self.send("Play");
                    }
                    if ui.add(remote_btn("Fwd", [64.0, 36.0])).clicked() {
                        self.send("Fwd");
                    }
                });
            });

            ui.add_space(4.0);

            ui.vertical_centered(|ui| {
                ui.horizontal(|ui| {
                    if ui.add(remote_btn("Vol-", [64.0, 36.0])).clicked() {
                        self.send("VolumeDown");
                    }
                    if ui.add(remote_btn("Mute", [64.0, 36.0])).clicked() {
                        self.send("VolumeMute");
                    }
                    if ui.add(remote_btn("Vol+", [64.0, 36.0])).clicked() {
                        self.send("VolumeUp");
                    }
                });
            });

            ui.add_space(4.0);

            ui.vertical_centered(|ui| {
                ui.horizontal(|ui| {
                    if ui.add(remote_btn("Info", [64.0, 36.0])).clicked() {
                        self.send("Info");
                    }
                    if ui.add(remote_btn("Replay", [72.0, 36.0])).clicked() {
                        self.send("InstantReplay");
                    }
                    if ui.add(remote_btn("Search", [64.0, 36.0])).clicked() {
                        self.send("Search");
                    }
                });
            });

            ui.add_space(8.0);
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("Text:");
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.text_input).desired_width(200.0),
                );
                if r.has_focus() {
                    text_focused = true;
                }
                if ui.button("Type").clicked() {
                    self.send_text();
                }
                if r.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.send_text();
                }
            });

            ui.separator();

            ui.label(&self.status);
            ui.label(
                egui::RichText::new(
                    "Arrows:Nav  Enter:OK  Bksp:Back  Esc:Home  Space/P:Play  M:Mute  -/=:Vol",
                )
                .size(11.0)
                .weak(),
            );
        });

        if !text_focused {
            let key = ctx.input(|i| {
                if i.key_pressed(egui::Key::ArrowUp) {
                    Some("Up")
                } else if i.key_pressed(egui::Key::ArrowDown) {
                    Some("Down")
                } else if i.key_pressed(egui::Key::ArrowLeft) {
                    Some("Left")
                } else if i.key_pressed(egui::Key::ArrowRight) {
                    Some("Right")
                } else if i.key_pressed(egui::Key::Enter) {
                    Some("Select")
                } else if i.key_pressed(egui::Key::Space) {
                    Some("Play")
                } else if i.key_pressed(egui::Key::Backspace) {
                    Some("Back")
                } else if i.key_pressed(egui::Key::Escape) {
                    Some("Home")
                } else if i.key_pressed(egui::Key::M) {
                    Some("VolumeMute")
                } else if i.key_pressed(egui::Key::I) {
                    Some("Info")
                } else if i.key_pressed(egui::Key::R) {
                    Some("InstantReplay")
                } else if i.key_pressed(egui::Key::S) {
                    Some("Search")
                } else if i.key_pressed(egui::Key::P) {
                    Some("Play")
                } else if i.key_pressed(egui::Key::F) {
                    Some("Fwd")
                } else if i.key_pressed(egui::Key::W) {
                    Some("Rev")
                } else if i.key_pressed(egui::Key::Minus) {
                    Some("VolumeDown")
                } else if i.key_pressed(egui::Key::Equals) {
                    Some("VolumeUp")
                } else {
                    None
                }
            });
            if let Some(k) = key {
                self.send(k);
            }
        }
    }
}
