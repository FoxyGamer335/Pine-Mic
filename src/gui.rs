use std::sync::Arc;

use std::time::Duration;

use eframe::egui;

use crate::audio;
use crate::net::{NetController, Target};
use crate::state::{make_icon_bytes, ConnectionStatus, SharedState, MAX_GAIN};

pub struct PhoneMicApp {
    state: Arc<SharedState>,
    net: Arc<NetController>,
    manual_ip: String,
    mic_name_input: String,
    tray_enabled: bool,
}

impl PhoneMicApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        state: Arc<SharedState>,
        net: Arc<NetController>,
        tray_enabled: bool,
    ) -> Self {
        // Let the tray / other threads show, hide and repaint this window.
        state.set_egui_ctx(cc.egui_ctx.clone());
        let mic_name_input = state.mic_name();
        Self {
            state,
            net,
            manual_ip: String::new(),
            mic_name_input,
            tray_enabled,
        }
    }
}

pub fn run(
    state: Arc<SharedState>,
    net: Arc<NetController>,
    tray_enabled: bool,
) -> eframe::Result<()> {
    let (width, height, rgba) = make_icon_bytes(false, false, false);
    let icon = Arc::new(egui::IconData { rgba, width, height });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
        .with_title("Pine Mic")
        .with_app_id("pine-mic")
        .with_icon(icon)
        .with_inner_size([430.0, 650.0])
        .with_min_inner_size([380.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Pine Mic",
        options,
        Box::new(move |cc| Ok(Box::new(PhoneMicApp::new(cc, state, net, tray_enabled)))),
    )
}

impl eframe::App for PhoneMicApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ---- close-to-tray & visibility sync ------------------------------
        let exiting = self.state.is_shutdown() || self.state.quit_requested();
        if exiting {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        if ctx.input(|i| i.viewport().close_requested()) {
            if self.tray_enabled {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.state.hide_window();
            } else {
                self.state.request_shutdown();
            }
        }

        if self.state.take_visibility_dirty() {
            if self.state.window_visible() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }

        let status = self.state.status();
        let port = self.state.config.audio_port;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Pine Mic");
            ui.add_space(4.0);

            let color = match &status {
                ConnectionStatus::Connected(_) => egui::Color32::from_rgb(0x3F, 0xB9, 0x50),
                                           ConnectionStatus::Connecting(_) => egui::Color32::from_rgb(0xE3, 0xB3, 0x41),
                                           ConnectionStatus::Error(_) => egui::Color32::from_rgb(0xF8, 0x51, 0x49),
                                           ConnectionStatus::Disconnected => egui::Color32::GRAY,
            };
            ui.colored_label(color, status.to_string());
            ui.separator();

            // ---- permanent virtual microphone -----------------------------
            ui.label("Virtual Microphone (PipeWire):");
            let mic_active = self.state.is_mic_active();
            if let Some(msg) = self.state.mic_status() {
                let mic_color = if mic_active {
                    egui::Color32::from_rgb(0x3F, 0xB9, 0x50)
                } else {
                    egui::Color32::from_rgb(0xE3, 0xB3, 0x41)
                };
                ui.colored_label(mic_color, msg);
            }

            ui.horizontal(|ui| {
                ui.label("Mic name:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.mic_name_input)
                    .hint_text("Android_Phone_Mic")
                    .desired_width(200.0),
                );
            });

            ui.horizontal(|ui| {
                let create_label = if mic_active {
                    "Recreate / Rename Mic"
                } else {
                    "Create Mic (Permanent)"
                };
                if ui.button(create_label).clicked() {
                    if let Ok(()) = audio::create_permanent_mic(&self.state, &self.mic_name_input) {
                        self.mic_name_input = self.state.mic_name();
                    }
                }
                if ui
                    .add_enabled(mic_active, egui::Button::new("Remove Mic"))
                    .clicked()
                    {
                        let _ = audio::remove_permanent_mic(&self.state);
                    }
            });
            ui.separator();

            // ---- discovered phones ----------------------------------------
            ui.label("Discovered phones (click to connect over Wi-Fi):");
            let phones = self.state.phones();
            if phones.is_empty() {
                ui.weak("Searching… open the Pine Mic app on your phone.");
            }
            for p in &phones {
                if ui.button(format!("{}   ({})", p.name, p.ip)).clicked() {
                    self.manual_ip = p.ip.to_string();
                    self.net.connect(Target::Wifi {
                        host: p.ip.to_string(),
                                     port: p.audio_port,
                    });
                }
            }
            ui.separator();

            // ---- manual connection ----------------------------------------
            ui.horizontal(|ui| {
                ui.label("Phone IP:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.manual_ip)
                    .hint_text("192.168.1.23")
                    .desired_width(170.0),
                );
            });
            ui.horizontal(|ui| {
                if ui.button("Connect Wi-Fi (UDP)").clicked() {
                    if self.manual_ip.trim().is_empty() {
                        self.state.set_status(ConnectionStatus::Error(
                            "enter the IP shown on the phone".into(),
                        ));
                    } else {
                        self.net.connect(Target::Wifi {
                            host: self.manual_ip.trim().to_string(),
                                         port,
                        });
                    }
                }
                if ui.button("Connect USB (ADB TCP)").clicked() {
                    self.net.connect(Target::Usb { port });
                }
            });
            let can_disconnect = !matches!(status, ConnectionStatus::Disconnected);
            if ui
                .add_enabled(can_disconnect, egui::Button::new("Disconnect"))
                .clicked()
                {
                    self.net.disconnect();
                }
                ui.separator();

            // ---- audio controls -------------------------------------------
            let mut denoise = self.state.denoise_enabled();
            if ui
                .checkbox(&mut denoise, "Enable RNNoise (noise suppression)")
                .changed()
                {
                    self.state.set_denoise(denoise);
                }
                let mut muted = self.state.is_muted();
            if ui.checkbox(&mut muted, "Mute").changed() {
                self.state.set_muted(muted);
            }
            let mut gain = self.state.gain();
            if ui
                .add(
                    egui::Slider::new(&mut gain, 0.0..=MAX_GAIN)
                    .text("Gain")
                    .step_by(0.05),
                )
                .changed()
                {
                    self.state.set_gain(gain);
                }
                ui.add(
                    egui::ProgressBar::new((self.state.level() * 3.0).clamp(0.0, 1.0)).text("Level"),
                );
                ui.add_space(6.0);
                ui.weak(format!(
                    "Packets: {} received, {} lost",
                    self.state.packets_rx(),
                                self.state.packets_lost()
                ));
                if self.tray_enabled {
                    ui.weak("Closing this window keeps streaming from the system tray.");
                }
        });

        // Keep status, level meter and phone list live.
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}
