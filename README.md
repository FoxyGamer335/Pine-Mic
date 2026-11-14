# Pine Mic

Use your Android phone as a low-latency virtual microphone on Linux over Wi-Fi or USB.

This project was developed and tested on **CachyOS** (Arch Linux). The entire codebase was built with the assistance of **Claude Sonnet 5.5** and **Gemini 4 Argon**.

---

## ✨ Features

* **Dual Transport (Wi-Fi & USB):** Stream low-latency audio over Wi-Fi (UDP port `54545` with automatic LAN discovery on UDP port `54546`) or over a USB cable via `adb forward` (TCP port `54545` with `TCP_NODELAY`).
* **High-Quality Opus Audio:** Captures 48 kHz 16-bit mono PCM on Android in 20 ms chunks and encodes it at 96 kbps using the platform `MediaCodec` Opus encoder, complete with Packet Loss Concealment (PLC) and automatic backlog trimming on Linux.
* **PipeWire Virtual Microphone:** Dynamically creates a stereo virtual microphone (`PhoneVirtualMic` backed by `PhoneMicSink`) via `pactl` so WebRTC, Discord, and OBS link both Left and Right channels reliably. Can be saved permanently across reboots in `~/.config/pipewire/pipewire-pulse.conf.d/99-phone-mic.conf`.
* **Real-Time DSP & Controls:** Built-in RNNoise neural noise suppression (`nnnoiseless`), linear gain control (`0.0` – `8.0`), mute toggle, live RMS level meter, and packet loss statistics.
* **Clean Raw Capture on Android:** Allows selecting specific physical microphones (Bottom/Primary, Top/Earpiece, Back/Camera, Wired, USB, or Bluetooth) and `AudioSource` modes (`VOICE_RECOGNITION`, `CAMCORDER`, `UNPROCESSED`, `MIC`) on the fly while force-disabling Android's muffled call DSP effects (`NoiseSuppressor`, `AcousticEchoCanceler`, `AutomaticGainControl`).
* **GUI, System Tray & Headless CLI:** Run the desktop app with an `egui` interface and `ksni` StatusNotifier system tray icon, or run completely headless in `--cli` mode.

---

## 📦 Downloads & Runtime Requirements

### 🐧 Linux Binary (`phone-mic`)
A standalone binary for Linux (compiled on CachyOS / Arch Linux).

**Runtime Dependencies:**
To make the virtual microphone and audio streaming work correctly, ensure you have the following packages installed on your Linux distribution:
* **Audio Server:** `pipewire`, `pipewire-pulse`, and `libpulse` / `pulseaudio-utils` (required for `pactl` commands).
* **Codecs & Libraries:** `opus`.
* **USB Mode (Optional):** `android-tools` (`adb`) if you plan to connect your phone via USB cable.

**Quick install commands:**
* **CachyOS / Arch Linux:**
  ```bash
  sudo pacman -S --needed pipewire pipewire-pulse libpulse opus android-tools
  ```
* **Ubuntu / Debian:**
  ```bash
  sudo apt update
  sudo apt install -y pipewire pipewire-pulse pulseaudio-utils libopus0 adb
  ```
* **Fedora:**
  ```bash
  sudo dnf install -y pipewire pipewire-pulseaudio pulseaudio-utils opus android-tools
  ```

---

### 📱 Android App (`app-debug.apk`)
The companion Android application (`dev.pinemic`), built with zero third-party dependencies.

**Requirements:**
* **Android Version:** Android 10 (API 29) or higher (required for platform `MediaCodec` Opus encoding).
* **Permissions:**
  * `RECORD_AUDIO` — mandatory to capture microphone input.
  * `FOREGROUND_SERVICE` / `FOREGROUND_SERVICE_MICROPHONE` — keeps the stream active in the background when the screen is turned off.
  * `POST_NOTIFICATIONS` (Android 13+) — displays the ongoing foreground service notification with a quick "Stop" action.
* **Connection:** Wi-Fi (UDP port `54545` + broadcast discovery on UDP `54546`) or USB with USB Debugging enabled (`adb forward` on TCP port `54545`).

---

## 🛠️ Building from Source

### 1. Build the Linux Client

#### Install Build Dependencies

**CachyOS / Arch Linux:**
```bash
sudo pacman -S --needed base-devel rustup pkgconf cmake opus libpulse pipewire pipewire-pulse android-tools
```

**Ubuntu / Debian:**
```bash
sudo apt update
sudo apt install -y build-essential pkg-config cmake curl libopus-dev libpulse-dev pulseaudio-utils pipewire pipewire-pulse adb
```

**Fedora:**
```bash
sudo dnf install -y @development-tools pkgconf-pkg-config cmake opus-devel pulseaudio-libs-devel pulseaudio-utils pipewire pipewire-pulseaudio android-tools
```

#### Set Up Rust & Compile
```bash
rustup default stable
git clone [https://github.com/FoxyGamer335/Pine-Mic.git](https://github.com/FoxyGamer335/Pine-Mic.git)
cd Pine-Mic
cargo build --release
```

The compiled binary will be located at `./target/release/phone-mic`.

---

### 2. Build the Android APK

#### Prerequisites
* **JDK 17** (`jdk17-openjdk`)
* **Android SDK** (`compileSdk = 35`, `targetSdk = 34`, `minSdk = 29`)

#### Compile with Gradle Wrapper
```bash
cd android
export JAVA_HOME=/usr/lib/jvm/java-17-openjdk
export ANDROID_HOME=$HOME/Android/Sdk   # or your local android-sdk path
chmod +x ./gradlew
./gradlew assembleDebug
```

The signed debug APK will be generated at:
`android/app/build/outputs/apk/debug/app-debug.apk`

---

## 🚀 Usage & CLI Options

Run `./target/release/phone-mic` with no arguments to launch the **GUI** with system tray support.

```text
Usage: phone-mic [OPTIONS]
```

| Option | Description | Default |
| :--- | :--- | :--- |
| `--cli` | Run headless (no GUI, no tray, no Wayland/X11 initialization) | `false` |
| `--ip <IP>` | Phone IP address for Wi-Fi (UDP). In `--cli` mode without `--ip`/`--usb`, connects to the first discovered phone | Auto-discovery |
| `--usb` | Connect over USB via `adb forward tcp:<port> tcp:<port>` | `false` |
| `--port <PORT>` | Audio port (UDP for Wi-Fi, TCP for USB) | `54545` |
| `--discovery-port <PORT>` | UDP port for phone LAN auto-discovery | `54546` |
| `--denoise` | Enable RNNoise noise suppression on startup | `false` |
| `--gain <GAIN>` | Linear gain applied to the microphone signal (`0.0` – `8.0`) | `1.0` |
| `--latency-ms <MS>` | PulseAudio playback buffer target (`tlength`) in milliseconds | `15` |
| `--create-mic <NAME>` | Permanently create the virtual microphone with the given name on startup | — |
| `--remove-mic` | Permanently remove the virtual microphone and exit | `false` |
| `--no-tray` | GUI only: disable the system tray icon (closing the window quits the app) | `false` |
| `--x11` | GUI only: force X11/XWayland (enabled by default so close-to-tray works on KDE Wayland) | `true` |
| `--wayland` | GUI only: force native Wayland | `false` |

### Environment Variables (Linux)
* `PHONEMIC_F32=1` — Use `float32le` sample format instead of the default `s16le` when opening the PulseAudio playback stream.
* `PHONEMIC_NOATTR=1` — Let the sound server use default buffer attributes instead of custom `tlength`/`maxlength`.
* `PHONEMIC_DEBUG=1` — Print per-second frame counts, peak audio levels, and stream reopen errors to `stderr`.

---

## 📡 Wire Protocol

* **Audio Frame (`Phone -> Linux`):** 48 kHz mono Opus packet prefixed with a 16-bit big-endian sequence number:
  ```text
  [ seq: u16 BE ][ opus packet bytes ... ]
  ```
* **Wi-Fi Transport (UDP `:54545`):** One frame per datagram. The Linux client sends `PMIC_HELLO1` every 1 second to `phone_ip:54545` as a keep-alive; the phone streams to the source address of the `HELLO` and stops ~3 seconds after the last `HELLO` or upon receiving `PMIC_BYE1`.
* **USB Transport (TCP `:54545`):** Forwarded via `adb forward tcp:54545 tcp:54545`. Each frame is prefixed with its 16-bit big-endian byte length:
  ```text
  [ len: u16 BE ][ seq: u16 BE ][ opus packet bytes ... ]
  ```
* **LAN Discovery (UDP Broadcast `:54546`):** The Android app broadcasts a beacon once per second:
  ```text
  PHONEMIC1;<device model>;<audio port>
  ```
