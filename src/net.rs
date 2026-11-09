//! Networking for phone-mic.
//!
//! ## Wire protocol (the Android app must implement exactly this)
//!
//! **Audio frame** (phone -> Linux), 48 kHz mono Opus, 10 ms per frame:
//! ```text
//!   [ seq: u16 big-endian ][ opus packet bytes ... ]
//! ```
//! * **Wi-Fi / UDP** (port 54545): one frame per datagram. The phone listens on
//!   UDP 54545. The Linux side sends `PMIC_HELLO1` from its own UDP socket to
//!   `phone_ip:54545` once per second (keep-alive). The phone streams frames to
//!   the *source address of the HELLO*, and stops after ~3 s without one.
//!   Because Linux talks first, replies pass any stateful firewall untouched.
//!   `PMIC_BYE1` is sent on disconnect.
//! * **USB / TCP** (port 54545 via `adb forward tcp:54545 tcp:54545`): the phone
//!   listens on TCP 54545 (TCP_NODELAY), Linux connects to 127.0.0.1:54545.
//!   Each frame is prefixed by its length: `[ len: u16 BE ][ frame ]`
//!   where `len` counts the seq bytes + opus bytes.
//!
//! **Discovery** (phone -> LAN, UDP broadcast to 255.255.255.255:54546, ~1/s):
//! ```text
//!   PHONEMIC1;<device name>;<audio port>
//! ```

use std::io::{self, Read};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, ToSocketAddrs, UdpSocket};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::TrySendError;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use crate::audio::{AudioMsg, AudioSender};
use crate::state::{lock, ConnectionStatus, SharedState};

pub const HELLO: &[u8] = b"PMIC_HELLO1";
pub const BYE: &[u8] = b"PMIC_BYE1";
pub const BEACON_PREFIX: &str = "PHONEMIC1;";

/// seq (u16)
const HEADER_LEN: usize = 2;
/// Sanity limit for one frame (Opus packets here are < 400 bytes).
const MAX_FRAME: usize = 1500;
const PHONE_TIMEOUT: Duration = Duration::from_millis(2500);

// ===========================================================================
// Controller (used by GUI, CLI and tray)
// ===========================================================================

#[derive(Clone, Debug)]
pub enum Target {
    /// Phone reachable over Wi-Fi: hostname/IP (optionally `ip:port`) and port.
    Wifi { host: String, port: u16 },
    /// Phone connected via USB with ADB; uses `adb forward`.
    Usb { port: u16 },
}

struct Session {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

pub struct NetController {
    state: Arc<SharedState>,
    audio_tx: AudioSender,
    session: Mutex<Option<Session>>,
}

impl NetController {
    pub fn new(state: Arc<SharedState>, audio_tx: AudioSender) -> Arc<Self> {
        Arc::new(Self {
            state,
            audio_tx,
            session: Mutex::new(None),
        })
    }

    /// Start (or restart) a session. Any previous session is stopped first.
    pub fn connect(&self, target: Target) {
        self.stop_session();

        let stop = Arc::new(AtomicBool::new(false));
        let state = self.state.clone();
        let tx = self.audio_tx.clone();
        let thread_stop = stop.clone();

        let spawned = thread::Builder::new()
        .name("phonemic-net".into())
        .spawn(move || match target {
            Target::Wifi { host, port } => run_wifi(&state, &tx, &thread_stop, &host, port),
               Target::Usb { port } => run_usb(&state, &tx, &thread_stop, port),
        });

        match spawned {
            Ok(handle) => *lock(&self.session) = Some(Session { stop, handle }),
            Err(e) => self.state.set_status(ConnectionStatus::Error(format!(
                "cannot start network thread: {e}"
            ))),
        }
    }

    pub fn disconnect(&self) {
        self.stop_session();
        self.state.set_level(0.0);
        let _ = self.audio_tx.try_send(AudioMsg::Reset);
        self.state.set_status(ConnectionStatus::Disconnected);
    }

    fn stop_session(&self) {
        let session = lock(&self.session).take();
        if let Some(s) = session {
            s.stop.store(true, Ordering::Relaxed);
            let _ = s.handle.join();
        }
    }
}

impl Drop for NetController {
    fn drop(&mut self) {
        self.stop_session();
    }
}

// ===========================================================================
// helpers
// ===========================================================================

fn should_run(state: &SharedState, stop: &AtomicBool) -> bool {
    !stop.load(Ordering::Relaxed) && !state.is_shutdown()
}

/// Sleep in short steps so a stop request is honoured quickly.
fn sleep_interruptible(state: &SharedState, stop: &AtomicBool, dur: Duration) {
    let end = Instant::now() + dur;
    while Instant::now() < end && should_run(state, stop) {
        thread::sleep(Duration::from_millis(50));
    }
}

fn resolve_v4(host: &str, port: u16) -> Result<SocketAddr> {
    let host = host.trim();
    if host.is_empty() {
        bail!("no IP address given");
    }
    // Accept "ip:port" typed into the manual field.
    if let Ok(sa) = host.parse::<SocketAddrV4>() {
        return Ok(SocketAddr::V4(sa));
    }
    (host, port)
    .to_socket_addrs()
    .with_context(|| format!("cannot resolve '{host}'"))?
    .find(|a| a.is_ipv4())
    .ok_or_else(|| anyhow!("no IPv4 address for '{host}'"))
}

/// Parse `[seq][opus]` and hand it to the audio thread. Never blocks: if the
/// audio queue is full the packet is dropped (stale audio is worthless).
fn forward_frame(state: &SharedState, tx: &AudioSender, frame: &[u8]) {
    if frame.len() <= HEADER_LEN {
        return;
    }
    let seq = u16::from_be_bytes([frame[0], frame[1]]);
    state.add_rx();
    match tx.try_send(AudioMsg::Packet {
        seq,
        data: frame[HEADER_LEN..].to_vec(),
    }) {
        Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
    }
}

// ===========================================================================
// Wi-Fi (UDP)
// ===========================================================================

fn run_wifi(state: &SharedState, tx: &AudioSender, stop: &AtomicBool, host: &str, port: u16) {
    let addr = match resolve_v4(host, port) {
        Ok(a) => a,
        Err(e) => {
            state.set_status(ConnectionStatus::Error(format!("{e:#}")));
            return;
        }
    };

    // Prefer the fixed local port; fall back to an ephemeral one (replies still
    // work because the phone answers to our source address).
    let sock = match UdpSocket::bind(("0.0.0.0", port)).or_else(|_| UdpSocket::bind(("0.0.0.0", 0)))
    {
        Ok(s) => s,
        Err(e) => {
            state.set_status(ConnectionStatus::Error(format!(
                "cannot open UDP socket: {e}"
            )));
            return;
        }
    };
    if let Err(e) = sock.set_read_timeout(Some(Duration::from_millis(200))) {
        state.set_status(ConnectionStatus::Error(format!("socket error: {e}")));
        return;
    }

    state.set_status(ConnectionStatus::Connecting(format!("Wi-Fi {}", addr.ip())));
    let _ = tx.try_send(AudioMsg::Reset);

    let mut buf = [0u8; 2048];
    let mut last_hello: Option<Instant> = None;
    let mut last_rx: Option<Instant> = None;
    let mut connected = false;

    while should_run(state, stop) {
        if last_hello.map_or(true, |t| t.elapsed() >= Duration::from_secs(1)) {
            let _ = sock.send_to(HELLO, addr);
            last_hello = Some(Instant::now());
        }

        match sock.recv_from(&mut buf) {
            Ok((n, src)) => {
                if src.ip() == addr.ip() {
                    last_rx = Some(Instant::now());
                    if !connected {
                        connected = true;
                        state.set_status(ConnectionStatus::Connected(format!(
                            "Wi-Fi {}",
                            addr.ip()
                        )));
                    }
                    forward_frame(state, tx, &buf[..n]);
                }
            }
            Err(e)
            if matches!(
                e.kind(),
                        io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
            ) => {}
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }

        if connected && last_rx.map_or(true, |t| t.elapsed() > PHONE_TIMEOUT) {
            connected = false;
            state.set_level(0.0);
            let _ = tx.try_send(AudioMsg::Reset);
            state.set_status(ConnectionStatus::Connecting(format!(
                "Wi-Fi {}: waiting for phone audio…",
                addr.ip()
            )));
        }
    }

    state.set_level(0.0);
    // Tell the phone to stop (sent twice; UDP is lossy).
    let _ = sock.send_to(BYE, addr);
    let _ = sock.send_to(BYE, addr);
}

// ===========================================================================
// USB (TCP through `adb forward`)
// ===========================================================================

pub fn adb_forward(port: u16) -> Result<()> {
    let spec = format!("tcp:{port}");
    let out = Command::new("adb")
    .args(["forward", &spec, &spec])
    .output()
    .context("cannot run `adb` (install android-tools and enable USB debugging)")?;
    if !out.status.success() {
        bail!(
            "adb forward failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub fn adb_forward_remove(port: u16) {
    let _ = Command::new("adb")
    .args(["forward", "--remove", &format!("tcp:{port}")])
    .output();
}

/// Fill `buf` completely. Returns Ok(false) if asked to stop in the meantime.
fn read_full(
    stream: &mut TcpStream,
    buf: &mut [u8],
    state: &SharedState,
    stop: &AtomicBool,
) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        if !should_run(state, stop) {
            return Ok(false);
        }
        match stream.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "phone closed connection",
                ))
            }
            Ok(n) => filled += n,
            Err(e)
            if matches!(
                e.kind(),
                        io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
            ) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

fn read_usb_stream(
    stream: &mut TcpStream,
    state: &SharedState,
    tx: &AudioSender,
    stop: &AtomicBool,
    port: u16,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;

    let mut connected = false;
    loop {
        let mut len_buf = [0u8; 2];
        if !read_full(stream, &mut len_buf, state, stop)? {
            return Ok(());
        }
        let len = u16::from_be_bytes(len_buf) as usize;
        if len <= HEADER_LEN || len > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad frame length",
            ));
        }
        let mut frame = vec![0u8; len];
        if !read_full(stream, &mut frame, state, stop)? {
            return Ok(());
        }
        if !connected {
            connected = true;
            state.set_status(ConnectionStatus::Connected(format!("USB (adb tcp:{port})")));
        }
        forward_frame(state, tx, &frame);
    }
}

fn run_usb(state: &SharedState, tx: &AudioSender, stop: &AtomicBool, port: u16) {
    let local = SocketAddr::from((Ipv4Addr::LOCALHOST, port));

    while should_run(state, stop) {
        state.set_status(ConnectionStatus::Connecting(format!(
            "USB: adb forward tcp:{port}"
        )));
        if let Err(e) = adb_forward(port) {
            state.set_status(ConnectionStatus::Error(format!("{e:#}")));
            sleep_interruptible(state, stop, Duration::from_secs(2));
            continue;
        }

        state.set_status(ConnectionStatus::Connecting(
            "USB: waiting for the phone app…".into(),
        ));
        // adb accepts the TCP connection even when the phone app is not
        // listening, then closes it; that is handled as "retry" below.
        if let Ok(mut stream) = TcpStream::connect_timeout(&local, Duration::from_secs(1)) {
            let _ = tx.try_send(AudioMsg::Reset);
            let _ = read_usb_stream(&mut stream, state, tx, stop, port);
            state.set_level(0.0);
        }

        if should_run(state, stop) {
            state.set_status(ConnectionStatus::Connecting(
                "USB: waiting for the phone app…".into(),
            ));
            sleep_interruptible(state, stop, Duration::from_secs(1));
        }
    }

    state.set_level(0.0);
    adb_forward_remove(port);
}

// ===========================================================================
// LAN discovery
// ===========================================================================

fn parse_beacon(data: &[u8], default_port: u16) -> Option<(String, u16)> {
    let text = std::str::from_utf8(data).ok()?;
    let rest = text.strip_prefix(BEACON_PREFIX)?;
    let mut parts = rest.split(';');
    let name = parts.next()?.trim();
    let port = parts
    .next()
    .and_then(|p| p.trim().parse::<u16>().ok())
    .unwrap_or(default_port);
    let name = if name.is_empty() {
        "Android phone".to_string()
    } else {
        name.to_string()
    };
    Some((name, port))
}

/// Listen for phone beacons on the discovery port and keep `state.phones()` fresh.
/// Returns an error if the port cannot be bound (manual IP still works then).
pub fn spawn_discovery(state: Arc<SharedState>) -> Result<JoinHandle<()>> {
    let port = state.config.discovery_port;
    let default_audio_port = state.config.audio_port;

    let sock = UdpSocket::bind(("0.0.0.0", port))
    .with_context(|| format!("cannot bind discovery UDP port {port} (already in use?)"))?;
    sock.set_read_timeout(Some(Duration::from_millis(500)))
    .context("discovery socket")?;
    let _ = sock.set_broadcast(true);

    thread::Builder::new()
    .name("phonemic-discovery".into())
    .spawn(move || {
        let mut buf = [0u8; 512];
        while !state.is_shutdown() {
            match sock.recv_from(&mut buf) {
                Ok((n, SocketAddr::V4(src))) => {
                    if let Some((name, audio_port)) =
                        parse_beacon(&buf[..n], default_audio_port)
                        {
                            state.upsert_phone(name, *src.ip(), audio_port);
                        }
                }
                Ok(_) => {}
                Err(e)
                if matches!(
                    e.kind(),
                            io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                ) => {}
                Err(_) => thread::sleep(Duration::from_millis(200)),
            }
            state.prune_phones(Duration::from_secs(5));
        }
    })
    .context("cannot spawn discovery thread")
}
