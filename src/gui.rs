use std::sync::Arc;
use std::time::Duration;

use eframe::egui;

use crate::audio;
use crate::net::{NetController, Target};
use crate::state::{make_icon_bytes, ConnectionStatus, SharedState, MAX_GAIN, WINDOW_TITLE};

const AUTO_LABEL: &str = "Auto (VB-Cable)";
const GREEN: egui::Color32 = egui::Color32::from_rgb(0x3F, 0xB9, 0x50);
const YELLOW: egui::Color32 = egui::Color32::from_rgb(0xE3, 0xB3, 0x41);
const RED: egui::Color32 = egui::Color32::from_rgb(0xF8, 0x51, 0x49);

pub struct PhoneMicApp {
    state: Arc<SharedState>,
    net: Arc<NetController>,
    manual_ip: String,
    /// Cached list of render devices (refreshed with the "Refresh list" button).
    devices: Vec<String>,
    tray_enabled: bool,
}

impl PhoneMicApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        state: Arc<SharedState>,
        net: Arc<NetController>,
        tray_enabled: bool,
    ) -> Self {
        // Let the tray / other threads repaint this window.
        state.set_egui_ctx(cc.egui_ctx.clone());
        Self {
            state,
            net,
            manual_ip: String::new(),
            devices: audio::list_output_devices(),
            tray_enabled,
        }
    }
}

pub fn run(
    state: Arc<SharedState>,
    net: Arc<NetController>,
    tray_enabled: bool,
) -> eframe::Result<()> {
    let (width, height, rgba) = make_icon_bytes(false, false);
    let icon = Arc::new(egui::IconData {
        rgba,
        width,
        height,
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // `winutil` finds the window by this exact title (to hide/show it).
            .with_title(WINDOW_TITLE)
            .with_app_id("pine-mic")
            .with_icon(icon)
            .with_inner_size([450.0, 700.0])
            .with_min_inner_size([380.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(move |cc| Ok(Box::new(PhoneMicApp::new(cc, state, net, tray_enabled)))),
    )
}

impl eframe::App for PhoneMicApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ---- close-to-tray -------------------------------------------------
        // (Showing / hiding from the tray is done with Win32 calls in
        // `winutil`, so there is no visibility sync to do here.)
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

        let status = self.state.status();
        let port = self.state.config.audio_port;

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.heading("Pine Mic");
                    ui.add_space(4.0);

                    let color = match &status {
                        ConnectionStatus::Connected(_) => GREEN,
                        ConnectionStatus::Connecting(_) => YELLOW,
                        ConnectionStatus::Error(_) => RED,
                        ConnectionStatus::Disconnected => egui::Color32::GRAY,
                    };
                    ui.colored_label(color, status.to_string());
                    ui.separator();

                    // ---- virtual microphone (output device) ---------------------
                    ui.label("Virtual Microphone (output device):");
                    let mic_active = self.state.is_mic_active();
                    if let Some(msg) = self.state.mic_status() {
                        ui.colored_label(if mic_active { GREEN } else { YELLOW }, msg);
                    }

                    let current = self.state.output_device();
                    let selected_text = current
                        .clone()
                        .unwrap_or_else(|| AUTO_LABEL.to_string());
                    let mut chosen: Option<Option<String>> = None;
                    egui::ComboBox::from_label("Output")
                        .selected_text(selected_text)
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(current.is_none(), AUTO_LABEL)
                                .clicked()
                            {
                                chosen = Some(None);
                            }
                            for d in &self.devices {
                                let selected = current.as_deref() == Some(d.as_str());
                                if ui.selectable_label(selected, d).clicked() {
                                    chosen = Some(Some(d.clone()));
                                }
                            }
                        });
                    if let Some(choice) = chosen {
                        audio::choose_output_device(&self.state, choice);
                    }

                    ui.horizontal(|ui| {
                        if ui.button("Refresh list").clicked() {
                            self.devices = audio::list_output_devices();
                        }
                        if ui.button("Reopen output").clicked() {
                            self.state.bump_mic_generation();
                        }
                    });
                    if !mic_active {
                        ui.horizontal_wrapped(|ui| {
                            ui.weak("No virtual cable? Windows needs a driver for virtual microphones:");
                            ui.hyperlink_to(
                                "get VB-Audio Virtual Cable (free)",
                                "https://vb-audio.com/Cable/",
                            );
                        });
                    }
                    ui.separator();

                    // ---- discovered phones --------------------------------------
                    ui.label("Discovered phones (click to connect over Wi-Fi):");
                    let phones = self.state.phones();
                    if phones.is_empty() {
                        ui.weak("Searching... open the Pine Mic app on your phone.");
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

                    // ---- manual connection --------------------------------------
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

                    // ---- audio controls -----------------------------------------
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
                        egui::ProgressBar::new((self.state.level() * 3.0).clamp(0.0, 1.0))
                            .text("Level"),
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
        });

        // Keep status, level meter and phone list live.
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}
