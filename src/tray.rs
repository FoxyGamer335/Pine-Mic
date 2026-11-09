//! System tray via `ksni` (StatusNotifierItem: KDE Plasma, Hyprland + waybar,
//! GNOME with the AppIndicator extension, ...).
//!
//! Deadlock note: ksni runs menu callbacks while holding the tray's own mutex,
//! and `Handle::update` takes that same mutex. So `SharedState`'s change hook
//! never calls `update` directly; it pings a refresher thread through a channel.

use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{Category, Icon, MenuItem, Status, ToolTip, Tray, TrayService};

use crate::state::{lock, make_icon_bytes, SharedState};

struct PhoneTray {
    state: Arc<SharedState>,
}

/// Converts the shared 32x32 ARGB32 icon buffer into a `ksni::Icon`.
fn make_icon(muted: bool, connected: bool) -> Icon {
    let (w, h, data) = make_icon_bytes(muted, connected, true);
    Icon {
        width: w as i32,
        height: h as i32,
        data,
    }
}

impl Tray for PhoneTray {
    fn id(&self) -> String {
        "phone-mic".into()
    }

    fn title(&self) -> String {
        "Phone Mic".into()
    }

    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    fn status(&self) -> Status {
        Status::Active
    }

    fn icon_name(&self) -> String {
        String::new() // use the generated ARGB32 pixmap below
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![make_icon(
            self.state.is_muted(),
                       self.state.status().is_connected(),
        )]
    }

    fn tool_tip(&self) -> ToolTip {
        let mut desc = self.state.status().to_string();
        if self.state.is_muted() {
            desc.push_str(" (muted)");
        }
        if self.state.is_mic_active() {
            desc.push_str(&format!("\nMic: {}", self.state.mic_name()));
        } else {
            desc.push_str("\nMic: not created");
        }
        ToolTip {
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
            title: "Phone Mic".into(),
            description: desc,
        }
    }

    /// Left click on the tray icon: toggle window visibility.
    fn activate(&mut self, _x: i32, _y: i32) {
        if self.state.window_visible() {
            self.state.hide_window();
        } else {
            self.state.show_window();
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let muted = self.state.is_muted();
        let denoise = self.state.denoise_enabled();
        let visible = self.state.window_visible();

        vec![
            StandardItem {
                label: if visible {
                    "Hide Window".into()
                } else {
                    "Show Window".into()
                },
                activate: Box::new(move |t: &mut Self| {
                    if visible {
                        t.state.hide_window();
                    } else {
                        t.state.show_window();
                    }
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: if muted { "Unmute" } else { "Mute" }.into(),
                activate: Box::new(|t: &mut Self| {
                    t.state.toggle_mute();
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "RNNoise noise suppression".into(),
                checked: denoise,
                activate: Box::new(|t: &mut Self| {
                    t.state.toggle_denoise();
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut Self| t.state.request_quit()),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Best-effort check that something on the session bus hosts tray icons.
/// If not, the GUI must quit on close instead of hiding (or the app would be
/// unreachable).
pub fn is_available() -> bool {
    let has = |name: &str| {
        Command::new("busctl")
        .args(["--user", "status", name])
        .output()
        .map(|o| o.status.success())
    };
    match (
        has("org.kde.StatusNotifierWatcher"),
           has("org.freedesktop.StatusNotifierWatcher"),
    ) {
        (Err(_), _) | (_, Err(_)) => true, // busctl missing: cannot tell, assume yes
        (Ok(a), Ok(b)) => a || b,
    }
}

/// Start the tray in background threads.
pub fn spawn(state: Arc<SharedState>) {
    let service = TrayService::new(PhoneTray {
        state: state.clone(),
    });
    let handle = service.handle();
    service.spawn();

    // State changes -> channel -> refresher thread -> handle.update (see module docs).
    let (tx, rx) = mpsc::channel::<()>();
    let tx = Mutex::new(tx);
    state.set_change_hook(Arc::new(move || {
        let _ = lock(&tx).send(());
    }));

    let st = state;
    thread::Builder::new()
    .name("phonemic-tray-refresh".into())
    .spawn(move || {
        while rx.recv().is_ok() {
            while rx.try_recv().is_ok() {}
            handle.update(|_| {});
            if st.is_shutdown() {
                break;
            }
        }
    })
    .ok();
}
