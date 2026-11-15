// Release builds use the GUI subsystem (no console window). `--cli` and
// `--help` re-attach to the parent console, see `winutil::attach_parent_console`.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod gui;
mod net;
mod state;
mod tray;
mod winutil;

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;

use crate::audio::AudioEngine;
use crate::net::{spawn_discovery, NetController, Target};
use crate::state::{
    Config, SharedState, DEFAULT_AUDIO_PORT, DEFAULT_DISCOVERY_PORT, DEFAULT_LATENCY_MS,
};

/// Use an Android phone as a low-latency virtual microphone.
#[derive(Parser, Debug)]
#[command(name = "pine-mic", version)]
struct Args {
    /// Run headless (no GUI, no tray)
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

    /// Linear gain applied to the microphone signal (0.0 - 8.0)
    #[arg(long, default_value_t = 1.0)]
    gain: f32,

    /// Playback buffer target in milliseconds
    #[arg(long, default_value_t = DEFAULT_LATENCY_MS)]
    latency_ms: u32,

    /// Output device to play into (name or part of it; remembered for the next
    /// start). Default: the VB-Audio Virtual Cable ("CABLE Input"), which other
    /// programs then see as the microphone "CABLE Output".
    /// Replaces the Linux --create-mic / --remove-mic.
    #[arg(long, value_name = "NAME")]
    device: Option<String>,

    /// List the available output devices and exit
    #[arg(long)]
    list_devices: bool,

    /// GUI only: do not create a tray icon (closing the window quits)
    #[arg(long)]
    no_tray: bool,
}

fn main() -> Result<()> {
    // Needed for --help / --cli output in the GUI-subsystem release build.
    winutil::attach_parent_console();

    let args = Args::parse();

    if args.list_devices {
        audio::print_output_devices();
        return Ok(());
    }

    let state = SharedState::new(Config {
        audio_port: args.port,
        discovery_port: args.discovery_port,
        latency_ms: args.latency_ms,
    });
    state.set_denoise(args.denoise);
    state.set_gain(args.gain);

    // Output device: the one remembered from last time, unless given explicitly.
    audio::load_saved_device(&state);
    if let Some(ref name) = args.device {
        audio::choose_output_device(&state, Some(name.clone()));
    }

    // Ctrl-C: ask everything to stop.
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
        args.ip.as_ref().map(|ip| Target::Wifi {
            host: ip.clone(),
            port: args.port,
        })
    }
}

/// The audio thread opens the output device asynchronously; wait for it.
fn wait_for_output(state: &SharedState, timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end && !state.is_shutdown() {
        if state.is_mic_active() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    state.is_mic_active()
}

fn run_cli(args: &Args, state: &Arc<SharedState>, net: &Arc<NetController>) -> Result<()> {
    if !wait_for_output(state, Duration::from_secs(5)) {
        let why = state
            .mic_status()
            .unwrap_or_else(|| "no output device".to_string());
        bail!("virtual microphone output unavailable: {why}");
    }
    if let Some(msg) = state.mic_status() {
        println!("{msg}");
    }

    let target = match initial_target(args) {
        Some(t) => t,
        None => {
            println!(
                "No --ip/--usb given; waiting for a phone on UDP {} ...",
                args.discovery_port
            );
            loop {
                if state.is_shutdown() {
                    return Ok(());
                }
                if let Some(p) = state.phones().into_iter().next() {
                    println!("Found {} at {}", p.name, p.ip);
                    break Target::Wifi {
                        host: p.ip.to_string(),
                        port: p.audio_port,
                    };
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
    println!("Shutting down...");
    Ok(())
}

fn run_gui(args: &Args, state: &Arc<SharedState>, net: &Arc<NetController>) -> Result<()> {
    if let Some(t) = initial_target(args) {
        net.connect(t);
    }

    // `tray` is `None` with --no-tray or if the icon could not be created; the
    // window then quits on close instead of hiding to the tray.
    let tray = if args.no_tray {
        None
    } else {
        tray::spawn(state.clone())
    };
    let tray_enabled = tray.is_some();

    let result = gui::run(state.clone(), net.clone(), tray_enabled)
        .map_err(|e| anyhow!("GUI error: {e}"));

    // Dropping the handle stops the tray thread and removes the icon.
    drop(tray);
    result
}
