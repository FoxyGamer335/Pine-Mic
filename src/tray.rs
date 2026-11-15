//! System tray (notification area) via `tray-icon`.
//!
//! The tray lives in its own thread with its own Win32 message loop
//! (`winutil::run_message_loop`). Reasons:
//!
//! * `tray-icon` objects are not `Send`, and the tray window needs a message
//!   loop on the thread that created it.
//! * Keeping it away from the egui/winit thread means the tray keeps working
//!   while the main window is hidden (egui may not be driven then).
//! * Updates are pushed in from any thread through `SharedState`'s change hook:
//!   the hook only posts a wake-up message to the tray thread, which then
//!   re-reads the state and updates icon / tooltip / menu labels itself. No
//!   locks are held across the call, so there is no deadlock risk.
//!
//! Behaviour matches the Linux tray: left click toggles the window, the menu
//! has Show/Hide, Mute/Unmute, RNNoise toggle and Quit.

use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem,
};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::state::{make_icon_bytes, SharedState};
use crate::winutil;

const ID_TOGGLE: &str = "toggle";
const ID_MUTE: &str = "mute";
const ID_DENOISE: &str = "denoise";
const ID_QUIT: &str = "quit";

/// Windows truncates tooltips at 127 characters.
const MAX_TOOLTIP_CHARS: usize = 120;

/// Keeps the tray thread alive. Dropping it removes the tray icon (it requests
/// shutdown, wakes the thread and waits for it to finish).
pub struct TrayHandle {
    state: Arc<SharedState>,
    thread_id: u32,
    handle: Option<JoinHandle<()>>,
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        self.state.request_shutdown();
        winutil::post_wake(self.thread_id);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn make_icon(muted: bool, connected: bool) -> Result<Icon, String> {
    let (w, h, rgba) = make_icon_bytes(muted, connected);
    Icon::from_rgba(rgba, w, h).map_err(|e| format!("cannot build tray icon: {e}"))
}

#[derive(Clone, PartialEq)]
struct Snapshot {
    muted: bool,
    connected: bool,
    visible: bool,
    denoise: bool,
    tooltip: String,
}

impl Snapshot {
    fn take(state: &SharedState) -> Self {
        let mut tooltip = format!("Pine Mic\n{}", state.status());
        if state.is_muted() {
            tooltip.push_str(" (muted)");
        }
        match state.active_device() {
            Some(dev) => tooltip.push_str(&format!("\nMic: {dev}")),
            None => tooltip.push_str("\nMic: no output device"),
        }
        Self {
            muted: state.is_muted(),
            connected: state.status().is_connected(),
            visible: state.window_visible(),
            denoise: state.denoise_enabled(),
            tooltip: tooltip.chars().take(MAX_TOOLTIP_CHARS).collect(),
        }
    }
}

struct TrayUi {
    tray: TrayIcon,
    item_toggle: MenuItem,
    item_mute: MenuItem,
    item_denoise: CheckMenuItem,
    last: Option<Snapshot>,
}

impl TrayUi {
    fn new(state: &SharedState) -> Result<Self, String> {
        let snap = Snapshot::take(state);

        let item_toggle = MenuItem::with_id(ID_TOGGLE, "Hide Window", true, None);
        let item_mute = MenuItem::with_id(ID_MUTE, "Mute", true, None);
        let item_denoise =
            CheckMenuItem::with_id(ID_DENOISE, "RNNoise noise suppression", true, false, None);
        let item_quit = MenuItem::with_id(ID_QUIT, "Quit", true, None);
        let sep1 = PredefinedMenuItem::separator();
        let sep2 = PredefinedMenuItem::separator();

        let menu = Menu::new();
        menu.append_items(&[
            &item_toggle,
            &sep1,
            &item_mute,
            &item_denoise,
            &sep2,
            &item_quit,
        ])
        .map_err(|e| format!("cannot build tray menu: {e}"))?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            // Left click toggles the window; the menu is for the right click.
            .with_menu_on_left_click(false)
            .with_tooltip(&snap.tooltip)
            .with_icon(make_icon(snap.muted, snap.connected)?)
            .build()
            .map_err(|e| format!("cannot create tray icon: {e}"))?;

        let mut ui = Self {
            tray,
            item_toggle,
            item_mute,
            item_denoise,
            last: None,
        };
        ui.refresh(state);
        Ok(ui)
    }

    /// Re-read the shared state and update whatever changed.
    fn refresh(&mut self, state: &SharedState) {
        let snap = Snapshot::take(state);
        if self.last.as_ref() == Some(&snap) {
            return;
        }
        let prev = self.last.take();

        if prev
            .as_ref()
            .map_or(true, |p| p.muted != snap.muted || p.connected != snap.connected)
        {
            if let Ok(icon) = make_icon(snap.muted, snap.connected) {
                let _ = self.tray.set_icon(Some(icon));
            }
        }
        if prev.as_ref().map_or(true, |p| p.tooltip != snap.tooltip) {
            let _ = self.tray.set_tooltip(Some(&snap.tooltip));
        }

        self.item_toggle
            .set_text(if snap.visible { "Hide Window" } else { "Show Window" });
        self.item_mute
            .set_text(if snap.muted { "Unmute" } else { "Mute" });
        self.item_denoise.set_checked(snap.denoise);

        self.last = Some(snap);
    }
}

fn install_handlers(state: &Arc<SharedState>) {
    // Left click on the icon: toggle window visibility.
    let st = state.clone();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            if st.window_visible() {
                st.hide_window();
            } else {
                st.show_window();
            }
        }
    }));

    // Menu items.
    let st = state.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        match event.id.0.as_str() {
            ID_TOGGLE => {
                if st.window_visible() {
                    st.hide_window();
                } else {
                    st.show_window();
                }
            }
            ID_MUTE => {
                st.toggle_mute();
            }
            ID_DENOISE => {
                st.toggle_denoise();
            }
            ID_QUIT => st.request_quit(),
            _ => {}
        }
    }));
}

fn tray_thread(state: Arc<SharedState>, ready: mpsc::Sender<Result<u32, String>>) {
    let mut ui = match TrayUi::new(&state) {
        Ok(ui) => ui,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    install_handlers(&state);
    // The thread now owns a window, hence a message queue: it can be woken.
    let _ = ready.send(Ok(winutil::current_thread_id()));

    winutil::run_message_loop(|| {
        if state.is_shutdown() {
            return false;
        }
        ui.refresh(&state);
        true
    });
    // `ui` is dropped here, which removes the icon from the notification area.
}

/// Start the tray in a background thread. Returns `None` (after printing a
/// warning) if the tray icon could not be created; the GUI must then quit on
/// close instead of hiding, or the app would become unreachable.
pub fn spawn(state: Arc<SharedState>) -> Option<TrayHandle> {
    let (tx, rx) = mpsc::channel();
    let thread_state = state.clone();
    let handle = match thread::Builder::new()
        .name("phonemic-tray".into())
        .spawn(move || tray_thread(thread_state, tx))
    {
        Ok(h) => h,
        Err(e) => {
            eprintln!("warning: cannot start tray thread: {e}");
            return None;
        }
    };

    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(thread_id)) => {
            // State changes -> wake-up message -> tray thread re-reads the state.
            state.set_change_hook(Arc::new(move || winutil::post_wake(thread_id)));
            Some(TrayHandle {
                state,
                thread_id,
                handle: Some(handle),
            })
        }
        Ok(Err(e)) => {
            eprintln!("warning: tray disabled: {e}");
            let _ = handle.join();
            None
        }
        Err(_) => {
            eprintln!("warning: tray disabled: timed out while creating the tray icon");
            None
        }
    }
}
