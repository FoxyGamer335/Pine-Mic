//! Shared state between the GUI, the system tray, the network threads and the
//! audio thread. Everything hot (mute, denoise, gain, level) is lock-free
//! atomics; everything structured is behind a small `Mutex`.

use std::fmt;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use eframe::egui;

pub const DEFAULT_AUDIO_PORT: u16 = 54545;
pub const DEFAULT_DISCOVERY_PORT: u16 = 54546;
/// Target PulseAudio playback buffer (tlength). 15 ms keeps total latency low.
pub const DEFAULT_LATENCY_MS: u32 = 15;
pub const MAX_GAIN: f32 = 8.0;
pub const DEFAULT_MIC_NAME: &str = "Android_Phone_Mic";

/// Lock a mutex, ignoring poisoning (a panicking thread must not take the
/// whole app down with it).
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Generates a 32x32 microphone icon.
/// If `argb_big_endian` is true, each pixel is `[A, R, G, B]` (for `ksni` D-Bus pixmap).
/// If `argb_big_endian` is false, each pixel is `[R, G, B, A]` (for `egui::IconData` taskbar icon).
pub fn make_icon_bytes(muted: bool, connected: bool, argb_big_endian: bool) -> (u32, u32, Vec<u8>) {
    const N: i32 = 32;
    let (r, g, b) = if muted {
        (0xD3u8, 0x2Fu8, 0x2Fu8)
    } else if connected {
        (0x2Eu8, 0xA0u8, 0x43u8)
    } else {
        (0x2Eu8, 0x7Du8, 0x32u8)
    };

    let mut data = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;
            let dist = ((fx - 16.0).powi(2) + (fy - 16.0).powi(2)).sqrt();
            let in_disk = dist <= 15.5;

            // Capsule-shaped mic body, stem and base.
            let body = ((fx - 16.0) / 3.8).powi(2) + ((fy - 12.0) / 6.5).powi(2) <= 1.0;
            let stem = (15.0..17.0).contains(&fx) && (18.0..22.5).contains(&fy);
            let base = (11.0..21.0).contains(&fx) && (22.5..24.5).contains(&fy);
            let white = body || stem || base;

            let (pr, pg, pb, pa) = if !in_disk {
                (0, 0, 0, 0)
            } else if white {
                (255, 255, 255, 255)
            } else {
                (r, g, b, 255)
            };

            if argb_big_endian {
                data.extend_from_slice(&[pa, pr, pg, pb]);
            } else {
                data.extend_from_slice(&[pr, pg, pb, pa]);
            }
        }
    }
    (N as u32, N as u32, data)
}

#[derive(Clone, Debug)]
pub struct Config {
    pub audio_port: u16,
    pub discovery_port: u16,
    pub latency_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            audio_port: DEFAULT_AUDIO_PORT,
            discovery_port: DEFAULT_DISCOVERY_PORT,
            latency_ms: DEFAULT_LATENCY_MS,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting(String),
    Connected(String),
    Error(String),
}

impl ConnectionStatus {
    /// True while a session is running (connecting or connected).
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Connecting(_) | Self::Connected(_))
    }
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected(_))
    }
}

impl fmt::Display for ConnectionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => write!(f, "Disconnected"),
            Self::Connecting(s) => write!(f, "Connecting: {s}"),
            Self::Connected(s) => write!(f, "Connected: {s}"),
            Self::Error(s) => write!(f, "Error: {s}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DiscoveredPhone {
    pub name: String,
    pub ip: Ipv4Addr,
    pub audio_port: u16,
    pub last_seen: Instant,
}

type ChangeHook = Arc<dyn Fn() + Send + Sync>;

pub struct SharedState {
    pub config: Config,

    muted: AtomicBool,
    denoise: AtomicBool,
    gain_bits: AtomicU32,
    level_bits: AtomicU32,
    shutdown: AtomicBool,
    quit_requested: AtomicBool,
    window_visible: AtomicBool,
    visibility_dirty: AtomicBool,

    packets_rx: AtomicU64,
    packets_lost: AtomicU64,

    // Virtual microphone state
    mic_active: AtomicBool,
    mic_generation: AtomicU64,
    mic_name: Mutex<String>,
    mic_status: Mutex<Option<String>>,

    status: Mutex<ConnectionStatus>,
    phones: Mutex<Vec<DiscoveredPhone>>,

    /// Set by the GUI on its first frame so the tray / other threads can wake
    /// and show/hide the window.
    egui_ctx: Mutex<Option<egui::Context>>,
    /// Optional hook (used by the tray) to refresh its menu on state changes.
    change_hook: Mutex<Option<ChangeHook>>,
}

impl SharedState {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config,
            muted: AtomicBool::new(false),
                 denoise: AtomicBool::new(false),
                 gain_bits: AtomicU32::new(1.0f32.to_bits()),
                 level_bits: AtomicU32::new(0.0f32.to_bits()),
                 shutdown: AtomicBool::new(false),
                 quit_requested: AtomicBool::new(false),
                 window_visible: AtomicBool::new(true),
                 visibility_dirty: AtomicBool::new(false),
                 packets_rx: AtomicU64::new(0),
                 packets_lost: AtomicU64::new(0),
                 mic_active: AtomicBool::new(false),
                 mic_generation: AtomicU64::new(1),
                 mic_name: Mutex::new(DEFAULT_MIC_NAME.to_string()),
                 mic_status: Mutex::new(None),
                 status: Mutex::new(ConnectionStatus::Disconnected),
                 phones: Mutex::new(Vec::new()),
                 egui_ctx: Mutex::new(None),
                 change_hook: Mutex::new(None),
        })
    }

    // ---- virtual microphone ---------------------------------------------
    pub fn mic_name(&self) -> String {
        lock(&self.mic_name).clone()
    }
    pub fn set_mic_name(&self, name: String) {
        let trimmed = name.trim();
        let final_name = if trimmed.is_empty() {
            DEFAULT_MIC_NAME.to_string()
        } else {
            trimmed.to_string()
        };
        *lock(&self.mic_name) = final_name;
        self.notify_changed();
    }
    pub fn is_mic_active(&self) -> bool {
        self.mic_active.load(Ordering::Acquire)
    }
    pub fn set_mic_active(&self, active: bool) {
        self.mic_active.store(active, Ordering::Release);
        self.notify_changed();
    }
    pub fn mic_status(&self) -> Option<String> {
        lock(&self.mic_status).clone()
    }
    pub fn set_mic_status(&self, status: Option<String>) {
        *lock(&self.mic_status) = status;
        self.notify_changed();
    }
    pub fn mic_generation(&self) -> u64 {
        self.mic_generation.load(Ordering::Acquire)
    }
    pub fn bump_mic_generation(&self) {
        self.mic_generation.fetch_add(1, Ordering::AcqRel);
        self.notify_changed();
    }

    // ---- mute -----------------------------------------------------------
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
    pub fn set_muted(&self, v: bool) {
        if self.muted.swap(v, Ordering::Relaxed) != v {
            self.notify_changed();
        }
    }
    /// Returns the new value.
    pub fn toggle_mute(&self) -> bool {
        let new = !self.is_muted();
        self.set_muted(new);
        new
    }

    // ---- denoise --------------------------------------------------------
    pub fn denoise_enabled(&self) -> bool {
        self.denoise.load(Ordering::Relaxed)
    }
    pub fn set_denoise(&self, v: bool) {
        if self.denoise.swap(v, Ordering::Relaxed) != v {
            self.notify_changed();
        }
    }
    /// Returns the new value.
    pub fn toggle_denoise(&self) -> bool {
        let new = !self.denoise_enabled();
        self.set_denoise(new);
        new
    }

    // ---- gain / level ---------------------------------------------------
    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain_bits.load(Ordering::Relaxed))
    }
    pub fn set_gain(&self, g: f32) {
        let g = if g.is_finite() { g.clamp(0.0, MAX_GAIN) } else { 1.0 };
        self.gain_bits.store(g.to_bits(), Ordering::Relaxed);
    }
    /// Output RMS level, 0.0..=1.0 (for the GUI meter).
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level_bits.load(Ordering::Relaxed))
    }
    pub fn set_level(&self, l: f32) {
        self.level_bits.store(l.to_bits(), Ordering::Relaxed);
    }

    // ---- stats ----------------------------------------------------------
    pub fn add_rx(&self) {
        self.packets_rx.fetch_add(1, Ordering::Relaxed);
    }
    pub fn add_lost(&self, n: u64) {
        self.packets_lost.fetch_add(n, Ordering::Relaxed);
    }
    pub fn packets_rx(&self) -> u64 {
        self.packets_rx.load(Ordering::Relaxed)
    }
    pub fn packets_lost(&self) -> u64 {
        self.packets_lost.load(Ordering::Relaxed)
    }

    // ---- connection status ---------------------------------------------
    pub fn status(&self) -> ConnectionStatus {
        lock(&self.status).clone()
    }
    pub fn set_status(&self, s: ConnectionStatus) {
        let changed = {
            let mut g = lock(&self.status);
            if *g != s {
                *g = s;
                true
            } else {
                false
            }
        };
        if changed {
            self.notify_changed();
        }
    }

    // ---- discovered phones ---------------------------------------------
    pub fn phones(&self) -> Vec<DiscoveredPhone> {
        lock(&self.phones).clone()
    }

    /// Insert or refresh a phone. Returns true if the visible list changed.
    pub fn upsert_phone(&self, name: String, ip: Ipv4Addr, audio_port: u16) -> bool {
        let changed = {
            let mut g = lock(&self.phones);
            if let Some(p) = g.iter_mut().find(|p| p.ip == ip) {
                let changed = p.name != name || p.audio_port != audio_port;
                p.name = name;
                p.audio_port = audio_port;
                p.last_seen = Instant::now();
                changed
            } else {
                g.push(DiscoveredPhone { name, ip, audio_port, last_seen: Instant::now() });
                true
            }
        };
        if changed {
            self.notify_changed();
        }
        changed
    }

    /// Drop phones not heard from within `max_age`. Returns true if any were removed.
    pub fn prune_phones(&self, max_age: Duration) -> bool {
        let removed = {
            let mut g = lock(&self.phones);
            let before = g.len();
            g.retain(|p| p.last_seen.elapsed() <= max_age);
            g.len() != before
        };
        if removed {
            self.notify_changed();
        }
        removed
    }

    // ---- lifecycle ------------------------------------------------------
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }
    /// Ask every worker thread to stop (used by Ctrl-C in CLI mode too).
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.notify_changed();
    }
    pub fn quit_requested(&self) -> bool {
        self.quit_requested.load(Ordering::Relaxed)
    }
    /// "Quit" from the tray: flag it, stop workers, and make the GUI really close
    /// (the GUI only cancels close requests while this flag is false).
    pub fn request_quit(&self) {
        self.quit_requested.store(true, Ordering::SeqCst);
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(ctx) = lock(&self.egui_ctx).clone() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            ctx.request_repaint();
        }
        self.notify_changed();
    }

    // ---- window / tray plumbing ----------------------------------------
    pub fn set_egui_ctx(&self, ctx: egui::Context) {
        *lock(&self.egui_ctx) = Some(ctx);
    }
    pub fn request_repaint(&self) {
        if let Some(ctx) = lock(&self.egui_ctx).as_ref() {
            ctx.request_repaint();
        }
    }
    pub fn window_visible(&self) -> bool {
        self.window_visible.load(Ordering::Acquire)
    }
    /// Used by `gui.rs` inside `update()` to check if visibility changed from the tray.
    pub fn take_visibility_dirty(&self) -> bool {
        self.visibility_dirty.swap(false, Ordering::AcqRel)
    }
    pub fn show_window(&self) {
        self.window_visible.store(true, Ordering::Release);
        self.visibility_dirty.store(true, Ordering::Release);
        if let Some(ctx) = lock(&self.egui_ctx).clone() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.request_repaint();
        }
        self.notify_changed();
    }
    pub fn hide_window(&self) {
        self.window_visible.store(false, Ordering::Release);
        self.visibility_dirty.store(true, Ordering::Release);
        if let Some(ctx) = lock(&self.egui_ctx).clone() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            ctx.request_repaint();
        }
        self.notify_changed();
    }

    pub fn set_change_hook(&self, hook: ChangeHook) {
        *lock(&self.change_hook) = Some(hook);
    }

    /// Wake the GUI and let the tray refresh. Never holds a lock while calling out.
    pub fn notify_changed(&self) {
        self.request_repaint();
        let hook = lock(&self.change_hook).clone();
        if let Some(h) = hook {
            h();
        }
    }
}
