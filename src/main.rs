mod audio;
mod gui;
mod net;
mod state;
mod tray;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;

use crate::audio::AudioEngine;
use crate::net::{spawn_discovery, NetController, Target};
use crate::state::{
    Config, SharedState, DEFAULT_AUDIO_PORT, DEFAULT_DISCOVERY_PORT, DEFAULT_LATENCY_MS,
};

/// Use an Android phone as a low-latency virtual microphone.
#[derive(Parser, Debug)]
#[command(name = "phone-mic", version)]
struct Args {
    /// Run headless (no GUI, no tray, no Wayland/X11 initialisation)
    #[arg(long)]
    cli: bool,

    /// Phone IP address for Wi-Fi (UDP). Without --ip/--usb in --cli mode the
    /// first phone found by LAN discovery is used.
    #[arg(long)]
    ip: Option<String>,

    /// Audio port (UDP for Wi-Fi, TCP for USB)
    #[arg(long, default_value_t = DEFAULT_AUDIO_PORT)]
    port: u16,

    /// UDP port for phone auto-discovery
    #[arg(long, default_value_t = DEFAULT_DISCOVERY_PORT)]
    discovery_port: u16,

    /// Connect over USB via `adb forward tcp:<port> tcp:<port>`
    #[arg(long)]
    usb: bool,

    /// Enable RNNoise noise suppression
    #[arg(long)]
    denoise: bool,

    /// Linear gain applied to the microphone signal (0.0 - 4.0)
    #[arg(long, default_value_t = 1.0)]
    gain: f32,

    /// Playback buffer target in milliseconds
    #[arg(long, default_value_t = DEFAULT_LATENCY_MS)]
    latency_ms: u32,

    /// Permanently create the virtual microphone with the given name on startup
    #[arg(long)]
    create_mic: Option<String>,

    /// Permanently remove the virtual microphone and exit
    #[arg(long)]
    remove_mic: bool,

    /// GUI only: do not create a tray icon (closing the window quits)
    #[arg(long)]
    no_tray: bool,

    /// GUI only: force X11/XWayland (enabled by default so close-to-tray works on KDE Wayland)
    #[arg(long)]
    x11: bool,

    /// GUI only: force native Wayland (note: winit cannot hide windows to tray on native Wayland)
    #[arg(long)]
    wayland: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // On KDE Plasma 6 (Wayland), winit's `Visible(false)` is a no-op under native Wayland.
    // Defaulting to XWayland (unless `--wayland` is explicitly passed) makes close-to-tray
    // and restoring from tray work reliably out of the box.
    if !args.cli && (!args.wayland || args.x11) {
        std::env::remove_var("WAYLAND_DISPLAY");
    }

    let state = SharedState::new(Config {
        audio_port: args.port,
        discovery_port: args.discovery_port,
        latency_ms: args.latency_ms,
    });
    state.set_denoise(args.denoise);
    state.set_gain(args.gain);

    if args.remove_mic {
        audio::remove_permanent_mic(&state)?;
        println!("Virtual microphone permanently removed.");
        return Ok(());
    }

    if let Some(ref custom_name) = args.create_mic {
        audio::create_permanent_mic(&state, custom_name)
        .context("permanent virtual microphone creation failed")?;
        println!("Virtual microphone permanently created: \"{}\"", state.mic_name());
    } else {
        audio::sync_existing_mic(&state);
        if state.is_mic_active() {
            println!("Virtual microphone active: \"{}\"", state.mic_name());
        } else {
            println!("Virtual microphone is not created yet. Create it in the GUI or pass --create-mic <NAME>.");
        }
    }

    // Ctrl-C / SIGTERM: ask everything to stop; permanent virtual mic is kept intact.
    {
        let st = state.clone();
        ctrlc::set_handler(move || st.request_quit()).context("cannot install Ctrl-C handler")?;
    }

    let result = run(&args, &state);

    state.request_shutdown();
    result
}

fn run(args: &Args, state: &Arc<SharedState>) -> Result<()> {
    // Declaration order matters: `net` drops before `engine`.
    let engine = AudioEngine::spawn(state.clone()).context("audio engine failed to start")?;
    let net = NetController::new(state.clone(), engine.sender());

    let discovery = match spawn_discovery(state.clone()) {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("warning: auto-discovery disabled: {e:#}");
            None
        }
    };

    let result = if args.cli {
        run_cli(args, state, &net)
    } else {
        run_gui(args, state, &net)
    };

    state.request_shutdown();
    net.disconnect();
    if let Some(h) = discovery {
        let _ = h.join();
    }
    result
}

fn initial_target(args: &Args) -> Option<Target> {
    if args.usb {
        Some(Target::Usb { port: args.port })
    } else {
        args.ip
        .as_ref()
        .map(|ip| Target::Wifi { host: ip.clone(), port: args.port })
    }
}

fn run_cli(args: &Args, state: &Arc<SharedState>, net: &Arc<NetController>) -> Result<()> {
    if !state.is_mic_active() {
        let default_name = state.mic_name();
        println!("CLI mode: creating virtual microphone \"{default_name}\"…");
        audio::create_permanent_mic(state, &default_name)
        .context("virtual microphone setup failed")?;
    }

    let target = match initial_target(args) {
        Some(t) => t,
        None => {
            println!("No --ip/--usb given; waiting for a phone on UDP {} …", args.discovery_port);
            loop {
                if state.is_shutdown() {
                    return Ok(());
                }
                if let Some(p) = state.phones().into_iter().next() {
                    println!("Found {} at {}", p.name, p.ip);
                    break Target::Wifi { host: p.ip.to_string(), port: p.audio_port };
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    };

    net.connect(target);
    let mut last = String::new();
    while !state.is_shutdown() {
        let now = state.status().to_string();
        if now != last {
            println!("{now}");
            last = now;
        }
        thread::sleep(Duration::from_millis(200));
    }
    println!("Shutting down…");
    Ok(())
}

fn run_gui(args: &Args, state: &Arc<SharedState>, net: &Arc<NetController>) -> Result<()> {
    if let Some(t) = initial_target(args) {
        net.connect(t);
    }

    let tray_enabled = !args.no_tray && tray::is_available();
    if tray_enabled {
        tray::spawn(state.clone());
    } else if !args.no_tray {
        eprintln!("warning: no StatusNotifier host found on the session bus; closing the window will quit the app");
    }

    gui::run(state.clone(), net.clone(), tray_enabled).map_err(|e| anyhow!("GUI error: {e}"))
}
