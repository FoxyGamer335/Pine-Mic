//! Audio engine (Windows).
//!
//! Pipeline (identical to the Linux version):
//!   network thread --(AudioMsg)--> audio thread:
//!   Opus decode (+ packet-loss concealment) -> RNNoise (optional)
//!   -> mute / gain / level meter -> 10 ms mono frames.
//!
//! Output (replaces PulseAudio/PipeWire):
//!   frames -> jitter buffer -> WASAPI render stream (cpal) of the chosen
//!   output device. Windows cannot create a virtual microphone without a
//!   kernel driver, so the recommended setup is *VB-Audio Virtual Cable*:
//!   we play into its "CABLE Input" render device and other programs pick the
//!   matching "CABLE Output" capture device as their microphone. Any other
//!   render device can be chosen too (e.g. headphones, for testing).
//!
//! The jitter buffer holds mono 48 kHz samples. The WASAPI callback pulls from
//! it, resamples 48 kHz -> device rate with linear interpolation (needed
//! because shared-mode WASAPI runs at the device's mix rate, often 44.1 kHz),
//! and duplicates the mono signal to every output channel.

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use audiopus::coder::{Decoder, GenericCtl};
use audiopus::{Channels, SampleRate};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    Device, FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig,
    SupportedStreamConfig,
};
use nnnoiseless::DenoiseState;

use crate::state::{lock, SharedState};

const SAMPLE_RATE: u32 = 48_000;
/// 10 ms @ 48 kHz mono (what Opus and RNNoise operate on).
const FRAME: usize = 480;
/// Largest Opus frame (120 ms) at 48 kHz.
const MAX_DECODE_SAMPLES: usize = 5760;
/// Queue between network threads and the audio thread.
const CHANNEL_CAP: usize = 128;
/// If more than this many packets are waiting, the oldest are decoded (to keep
/// decoder state valid) but not played, so latency never grows unbounded.
const MAX_BACKLOG_PACKETS: usize = 4;
/// How often a failed output device open is retried.
const REOPEN_INTERVAL: Duration = Duration::from_secs(3);

const SAVED_DEVICE_FILE: &str = "output_device.txt";

// ===========================================================================
// Output device discovery / persistence
// ===========================================================================

/// True for VB-Audio Virtual Cable style devices ("CABLE Input (VB-Audio
/// Virtual Cable)", "CABLE-A Input", "Hi-Fi Cable Input", ...).
pub fn is_virtual_cable(name: &str) -> bool {
    name.to_ascii_lowercase().contains("cable")
}

/// The capture-side name other programs should use as microphone, derived
/// from the render device name ("CABLE Input (...)" -> "CABLE Output (...)").
pub fn capture_hint(output_name: &str) -> Option<String> {
    if !is_virtual_cable(output_name) {
        return None;
    }
    // ASCII lowercase keeps byte offsets identical to the original string.
    let pos = output_name.to_ascii_lowercase().find("input")?;
    let mut s = String::with_capacity(output_name.len() + 1);
    s.push_str(&output_name[..pos]);
    s.push_str("Output");
    s.push_str(&output_name[pos + "input".len()..]);
    Some(s)
}

/// Names of all active render (output) devices, sorted.
pub fn list_output_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut names: Vec<String> = host
        .output_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default();
    names.sort();
    names.dedup();
    names
}

/// `--list-devices`
pub fn print_output_devices() {
    let devices = list_output_devices();
    if devices.is_empty() {
        println!("No output devices found.");
        return;
    }
    println!("Output devices (pass a name, or part of it, to --device):");
    for d in devices {
        let mark = if is_virtual_cable(&d) {
            "   <- virtual cable"
        } else {
            ""
        };
        println!("  {d}{mark}");
    }
}

/// Find the device to play into.
/// * `preferred = Some(name)`: case-insensitive exact match, then substring.
/// * `preferred = None`: the virtual cable (VB-Cable) if installed.
///
/// There is deliberately no fallback to the default speakers: that would play
/// the phone's audio out loud instead of feeding a microphone.
fn find_device(preferred: Option<&str>) -> Result<(Device, String)> {
    let host = cpal::default_host();
    let devices = host
        .output_devices()
        .context("cannot enumerate audio output devices")?;
    let named: Vec<(Device, String)> = devices
        .filter_map(|d| d.name().ok().map(|n| (d, n)))
        .collect();

    match preferred {
        Some(want) => {
            let want_l = want.to_ascii_lowercase();
            let pos = named
                .iter()
                .position(|(_, n)| n.to_ascii_lowercase() == want_l)
                .or_else(|| {
                    named
                        .iter()
                        .position(|(_, n)| n.to_ascii_lowercase().contains(&want_l))
                });
            match pos {
                Some(i) => Ok(named.into_iter().nth(i).expect("index in range")),
                None => bail!("output device \"{want}\" not found (see --list-devices)"),
            }
        }
        None => {
            let pos = named
                .iter()
                .position(|(_, n)| n.to_ascii_lowercase().starts_with("cable input"))
                .or_else(|| named.iter().position(|(_, n)| is_virtual_cable(n)));
            match pos {
                Some(i) => Ok(named.into_iter().nth(i).expect("index in range")),
                None => bail!(
                    "virtual cable not found. Install VB-Audio Virtual Cable \
                     (vb-audio.com/Cable), reboot, or choose an output device below"
                ),
            }
        }
    }
}

fn saved_device_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join("pine-mic")
            .join(SAVED_DEVICE_FILE),
    )
}

fn save_device(name: Option<&str>) {
    let Some(path) = saved_device_path() else {
        return;
    };
    match name {
        Some(n) => {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Err(e) = fs::write(&path, n) {
                eprintln!("warning: cannot save output device choice: {e}");
            }
        }
        None => {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Startup: restore the output device chosen in a previous session (the
/// Windows counterpart of the "permanent" virtual microphone).
pub fn load_saved_device(state: &SharedState) {
    let Some(path) = saved_device_path() else {
        return;
    };
    if let Ok(text) = fs::read_to_string(path) {
        let name = text.trim();
        if !name.is_empty() {
            state.set_output_device(Some(name.to_string()));
        }
    }
}

/// Select the output device (`None` / empty = auto-detect the virtual cable),
/// remember the choice for the next start and make the audio thread reopen
/// its stream.
pub fn choose_output_device(state: &SharedState, name: Option<String>) {
    state.set_output_device(name);
    save_device(state.output_device().as_deref());
}

// ===========================================================================
// Messages from the network layer
// ===========================================================================

pub enum AudioMsg {
    /// One Opus packet plus its 16-bit sequence number (wraps).
    Packet { seq: u16, data: Vec<u8> },
    /// A new connection started: reset decoder, sequence tracking, denoiser.
    Reset,
    Stop,
}

pub type AudioSender = SyncSender<AudioMsg>;

// ===========================================================================
// Jitter buffer + resampling render callback
// ===========================================================================

struct Jitter {
    /// Mono 48 kHz samples waiting for playback.
    q: VecDeque<f32>,
    /// True until `start_samples` are queued (also after an underrun): the
    /// callback outputs silence meanwhile, which avoids crackling.
    buffering: bool,
    start_samples: usize,
    /// Fill level the rate controller steers towards.
    target_samples: usize,
    /// Hard cap; the oldest samples are dropped beyond it.
    max_samples: usize,
}

impl Jitter {
    fn new(latency_ms: u32) -> Self {
        let start = (latency_ms as usize * (SAMPLE_RATE as usize / 1000)).max(FRAME * 2);
        Self {
            q: VecDeque::with_capacity(start * 4),
            buffering: true,
            start_samples: start,
            target_samples: start,
            max_samples: start + FRAME * 3,
        }
    }

    fn clear(&mut self) {
        self.q.clear();
        self.buffering = true;
    }

    fn push(&mut self, frame: &[f32]) {
        self.q.extend(frame.iter().copied());
        if self.q.len() > self.max_samples {
            let excess = self.q.len() - self.max_samples;
            self.q.drain(..excess);
        }
        if self.buffering && self.q.len() >= self.start_samples {
            self.buffering = false;
        }
    }

    fn pop(&mut self) -> f32 {
        if self.buffering {
            return 0.0;
        }
        match self.q.pop_front() {
            Some(s) => s,
            None => {
                self.buffering = true; // underrun: re-buffer
                0.0
            }
        }
    }
}

struct Resampler {
    /// 48000 / device rate: input samples consumed per output sample.
    step: f64,
    frac: f64,
    last: f32,
    cur: f32,
}

/// Fill one WASAPI buffer: pull mono samples, resample, fan out to all channels.
fn render<T>(data: &mut [T], channels: usize, jitter: &Mutex<Jitter>, rs: &mut Resampler)
where
    T: Sample + SizedSample + FromSample<f32>,
{
    let mut j = lock(jitter);

    // Gentle rate control: if the buffer is fuller than the target (phone clock
    // slightly faster than the sound card) consume a little faster, and the
    // other way round. At most +-0.5 %, inaudible for speech, and it keeps
    // the buffer from slowly drifting into overflow/underrun glitches.
    let step = if j.buffering {
        rs.step
    } else {
        let err = (j.q.len() as f64 - j.target_samples as f64) / 4800.0;
        rs.step * (1.0 + (err * 0.01).clamp(-0.005, 0.005))
    };

    for frame in data.chunks_mut(channels.max(1)) {
        while rs.frac >= 1.0 {
            rs.frac -= 1.0;
            rs.last = rs.cur;
            rs.cur = j.pop();
        }
        let s = rs.last + (rs.cur - rs.last) * rs.frac as f32;
        rs.frac += step;
        let v = T::from_sample(s);
        for out in frame.iter_mut() {
            *out = v;
        }
    }
}

fn build_stream<T>(
    device: &Device,
    config: &StreamConfig,
    jitter: Arc<Mutex<Jitter>>,
    failed: Arc<AtomicBool>,
) -> std::result::Result<Stream, cpal::BuildStreamError>
where
    T: Sample + SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut rs = Resampler {
        step: SAMPLE_RATE as f64 / config.sample_rate.0 as f64,
        frac: 1.0,
        last: 0.0,
        cur: 0.0,
    };
    device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            render(data, channels, &jitter, &mut rs)
        },
        move |err| {
            eprintln!("audio: output stream error: {err}");
            failed.store(true, Ordering::Relaxed);
        },
        None,
    )
}

fn build_for(
    device: &Device,
    supported: &SupportedStreamConfig,
    jitter: Arc<Mutex<Jitter>>,
    failed: Arc<AtomicBool>,
) -> Result<Stream> {
    let config = supported.config();
    let built = match supported.sample_format() {
        SampleFormat::F32 => build_stream::<f32>(device, &config, jitter, failed),
        SampleFormat::I16 => build_stream::<i16>(device, &config, jitter, failed),
        SampleFormat::I32 => build_stream::<i32>(device, &config, jitter, failed),
        SampleFormat::U16 => build_stream::<u16>(device, &config, jitter, failed),
        other => bail!("unsupported sample format {other:?}"),
    };
    built.map_err(|e| anyhow!("{e}"))
}

struct Opened {
    stream: Stream,
    name: String,
    rate: u32,
    channels: u16,
}

fn open_output(
    preferred: Option<&str>,
    jitter: Arc<Mutex<Jitter>>,
    failed: Arc<AtomicBool>,
) -> Result<Opened> {
    let (device, name) = find_device(preferred)?;

    // Candidates: native 48 kHz configs first (no resampling), then the
    // device's default mix format.
    let mut candidates: Vec<SupportedStreamConfig> = Vec::new();
    if let Ok(ranges) = device.supported_output_configs() {
        for r in ranges {
            let ok_fmt = matches!(
                r.sample_format(),
                SampleFormat::F32 | SampleFormat::I16 | SampleFormat::I32
            );
            if ok_fmt
                && r.min_sample_rate().0 <= SAMPLE_RATE
                && r.max_sample_rate().0 >= SAMPLE_RATE
            {
                candidates.push(r.with_sample_rate(cpal::SampleRate(SAMPLE_RATE)));
            }
        }
    }
    candidates.sort_by_key(|c| (c.sample_format() != SampleFormat::F32, c.channels() != 2));
    if let Ok(default) = device.default_output_config() {
        candidates.push(default);
    }
    if candidates.is_empty() {
        bail!("device \"{name}\" reports no usable output format");
    }

    let mut last_err = anyhow!("no candidate format");
    for supported in candidates {
        match build_for(&device, &supported, jitter.clone(), failed.clone()) {
            Ok(stream) => {
                stream
                    .play()
                    .map_err(|e| anyhow!("cannot start output stream on \"{name}\": {e}"))?;
                return Ok(Opened {
                    stream,
                    name,
                    rate: supported.sample_rate().0,
                    channels: supported.channels(),
                });
            }
            Err(e) => last_err = e,
        }
    }
    Err(last_err.context(format!("cannot open output stream on \"{name}\"")))
}

// ===========================================================================
// Playback (owned by the audio thread; cpal::Stream is not Send)
// ===========================================================================

struct Playback {
    stream: Option<Stream>,
    jitter: Arc<Mutex<Jitter>>,
    /// Set by the stream error callback (device unplugged, format changed...).
    failed: Arc<AtomicBool>,
    last_attempt: Option<Instant>,
    seen_generation: u64,
    debug: bool,
    written: u64,
    peak: f32,
    last_report: Instant,
}

impl Playback {
    fn new(latency_ms: u32, generation: u64) -> Self {
        Self {
            stream: None,
            jitter: Arc::new(Mutex::new(Jitter::new(latency_ms))),
            failed: Arc::new(AtomicBool::new(false)),
            last_attempt: None,
            seen_generation: generation,
            debug: std::env::var_os("PHONEMIC_DEBUG").is_some(),
            written: 0,
            peak: 0.0,
            last_report: Instant::now(),
        }
    }

    fn close(&mut self, state: &SharedState) {
        self.stream = None;
        state.set_mic_active(false);
        state.set_active_device(None);
    }

    fn open(&mut self, state: &SharedState) {
        lock(&self.jitter).clear();
        let preferred = state.output_device();
        match open_output(preferred.as_deref(), self.jitter.clone(), self.failed.clone()) {
            Ok(o) => {
                let msg = match capture_hint(&o.name) {
                    Some(hint) => format!(
                        "Mic active: playing into \"{}\". In your apps choose \"{}\" as the microphone.",
                        o.name, hint
                    ),
                    None => format!("Playing into \"{}\" ({} Hz, {} ch).", o.name, o.rate, o.channels),
                };
                self.stream = Some(o.stream);
                state.set_active_device(Some(o.name));
                state.set_mic_status(Some(msg));
                state.set_mic_active(true);
            }
            Err(e) => {
                state.set_active_device(None);
                state.set_mic_active(false);
                state.set_mic_status(Some(format!("{e:#}")));
                if self.debug {
                    eprintln!("audio: open failed: {e:#}");
                }
            }
        }
    }

    /// Reopen on device change / stream failure and keep retrying while closed.
    fn sync_with_state(&mut self, state: &SharedState) {
        let gen = state.mic_generation();
        if gen != self.seen_generation {
            self.seen_generation = gen;
            self.close(state);
            self.last_attempt = None;
        }
        if self.failed.swap(false, Ordering::Relaxed) && self.stream.is_some() {
            self.close(state);
            state.set_mic_status(Some("Output device lost, reconnecting...".into()));
            self.last_attempt = None;
        }
        if self.stream.is_none() {
            if let Some(last) = self.last_attempt {
                if last.elapsed() < REOPEN_INTERVAL {
                    return;
                }
            }
            self.last_attempt = Some(Instant::now());
            self.open(state);
        }
    }

    fn write_frame(&mut self, frame: &[f32; FRAME]) {
        if self.stream.is_none() {
            return; // no output device: drop the audio
        }
        lock(&self.jitter).push(frame);

        if self.debug {
            for s in frame.iter() {
                self.peak = self.peak.max(s.abs());
            }
            self.written += 1;
            if self.last_report.elapsed() >= Duration::from_secs(1) {
                eprintln!("audio: {} frames written, peak {:.4}", self.written, self.peak);
                self.written = 0;
                self.peak = 0.0;
                self.last_report = Instant::now();
            }
        }
    }
}

// ===========================================================================
// Decode / denoise / gain pipeline (unchanged from the Linux version)
// ===========================================================================

struct Pipeline {
    decoder: Decoder,
    denoiser: Box<DenoiseState<'static>>,
    denoise_active: bool,
    /// RNNoise's first output frame after (re)start contains fade-in artefacts.
    denoise_warmup: bool,
    /// Decoded samples (f32, -1.0..1.0) not yet forming a full 480-sample frame.
    pending: Vec<f32>,
    scratch: Vec<f32>,
    last_seq: Option<u16>,
    last_frame_len: usize,
}

impl Pipeline {
    fn new() -> Result<Self> {
        let decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono)
            .map_err(|e| anyhow!("cannot create Opus decoder: {e}"))?;
        Ok(Self {
            decoder,
            denoiser: DenoiseState::new(),
            denoise_active: false,
            denoise_warmup: false,
            pending: Vec::with_capacity(FRAME * 4),
            scratch: vec![0.0; MAX_DECODE_SAMPLES],
            last_seq: None,
            last_frame_len: FRAME,
        })
    }

    fn reset(&mut self) {
        let _ = self.decoder.reset_state();
        self.denoiser = DenoiseState::new();
        self.denoise_warmup = self.denoise_active;
        self.pending.clear();
        self.last_seq = None;
        self.last_frame_len = FRAME;
    }

    /// Decode one packet (with packet-loss concealment for small sequence gaps).
    fn push_packet(&mut self, state: &SharedState, seq: u16, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if let Some(last) = self.last_seq {
            let diff = seq.wrapping_sub(last);
            if diff == 0 || diff > 0x8000 {
                return; // duplicate or late/out-of-order packet
            }
            let lost = (diff - 1) as usize;
            if lost > 0 {
                state.add_lost(lost as u64);
                for _ in 0..lost.min(3) {
                    let n = self.last_frame_len;
                    if let Ok(got) =
                        self.decoder
                            .decode_float(None::<&[u8]>, &mut self.scratch[..n], false)
                    {
                        self.pending.extend_from_slice(&self.scratch[..got]);
                    }
                }
            }
        }
        self.last_seq = Some(seq);

        match self
            .decoder
            .decode_float(Some(data), &mut self.scratch[..], false)
        {
            Ok(got) if got > 0 => {
                self.last_frame_len = got;
                self.pending.extend_from_slice(&self.scratch[..got]);
            }
            Ok(_) => {}
            Err(e) => eprintln!("audio: opus decode error: {e}"),
        }
    }

    /// Run every complete 10 ms frame through denoise/gain/mute; play it if `audible`.
    fn emit_frames(&mut self, state: &SharedState, playback: &mut Playback, audible: bool) {
        while self.pending.len() >= FRAME {
            let mut frame = [0f32; FRAME];
            frame.copy_from_slice(&self.pending[..FRAME]);
            self.pending.drain(..FRAME);
            self.process_frame(state, &mut frame);
            if audible {
                playback.write_frame(&frame);
            }
        }
    }

    fn process_frame(&mut self, state: &SharedState, frame: &mut [f32; FRAME]) {
        if state.denoise_enabled() {
            if !self.denoise_active {
                self.denoise_active = true;
                self.denoiser = DenoiseState::new();
                self.denoise_warmup = true;
            }
            // nnnoiseless expects i16-range values stored as f32.
            let mut input = [0f32; FRAME];
            for (dst, src) in input.iter_mut().zip(frame.iter()) {
                *dst = (*src * 32768.0).clamp(-32768.0, 32767.0);
            }
            let mut output = [0f32; FRAME];
            let _vad = self.denoiser.process_frame(&mut output, &input);
            if self.denoise_warmup {
                self.denoise_warmup = false;
                frame.fill(0.0);
            } else {
                for (dst, src) in frame.iter_mut().zip(output.iter()) {
                    *dst = (*src / 32768.0).clamp(-1.0, 1.0);
                }
            }
        } else {
            self.denoise_active = false;
        }

        if state.is_muted() {
            frame.fill(0.0);
            state.set_level(0.0);
            return;
        }

        let gain = state.gain();
        let mut sum_sq = 0.0f32;
        for s in frame.iter_mut() {
            *s = (*s * gain).clamp(-1.0, 1.0);
            sum_sq += *s * *s;
        }
        let rms = (sum_sq / FRAME as f32).sqrt();
        // Smooth a little so the GUI meter does not flicker.
        let prev = state.level();
        state.set_level(prev * 0.6 + rms * 0.4);
    }
}

// ===========================================================================
// Audio thread / engine
// ===========================================================================

pub struct AudioEngine {
    tx: AudioSender,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AudioEngine {
    /// Spawn the audio thread. The output device is opened (and re-opened
    /// when the selection changes or the device disappears) by that thread;
    /// a missing virtual cable is reported through `SharedState::mic_status`
    /// and does not stop the app.
    pub fn spawn(state: Arc<SharedState>) -> Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<AudioMsg>(CHANNEL_CAP);
        let (init_tx, init_rx) = mpsc::channel::<std::result::Result<(), String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();

        let handle = thread::Builder::new()
            .name("phonemic-audio".into())
            .spawn(move || audio_thread(state, rx, init_tx, thread_stop))
            .context("cannot spawn audio thread")?;

        match init_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = handle.join();
                bail!(e);
            }
            Err(_) => bail!("audio thread failed to initialise"),
        }
        Ok(Self {
            tx,
            stop,
            handle: Some(handle),
        })
    }

    /// Cloneable sender handed to the network layer.
    pub fn sender(&self) -> AudioSender {
        self.tx.clone()
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.tx.try_send(AudioMsg::Stop);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn audio_thread(
    state: Arc<SharedState>,
    rx: Receiver<AudioMsg>,
    init_tx: mpsc::Sender<std::result::Result<(), String>>,
    stop: Arc<AtomicBool>,
) {
    let mut pipeline = match Pipeline::new() {
        Ok(p) => p,
        Err(e) => {
            let _ = init_tx.send(Err(format!("{e:#}")));
            return;
        }
    };
    let mut playback = Playback::new(state.config.latency_ms, state.mic_generation());
    let _ = init_tx.send(Ok(()));

    while !stop.load(Ordering::Relaxed) && !state.is_shutdown() {
        playback.sync_with_state(&state);

        let first = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(m) => m,
            Err(RecvTimeoutError::Timeout) => {
                // Decay level meter to zero when no audio packets are arriving.
                let prev = state.level();
                if prev > 0.001 {
                    state.set_level(prev * 0.5);
                } else if prev != 0.0 {
                    state.set_level(0.0);
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };

        // Grab everything that is waiting so a backlog can be trimmed.
        let mut batch = vec![first];
        while let Ok(m) = rx.try_recv() {
            batch.push(m);
        }
        let packet_count = batch
            .iter()
            .filter(|m| matches!(m, AudioMsg::Packet { .. }))
            .count();

        let mut seen = 0usize;
        for msg in batch {
            match msg {
                AudioMsg::Stop => return,
                AudioMsg::Reset => pipeline.reset(),
                AudioMsg::Packet { seq, data } => {
                    seen += 1;
                    // Only the newest MAX_BACKLOG_PACKETS of a burst are played.
                    let audible = packet_count - seen < MAX_BACKLOG_PACKETS;
                    pipeline.push_packet(&state, seq, &data);
                    pipeline.emit_frames(&state, &mut playback, audible);
                }
            }
        }
    }
}
