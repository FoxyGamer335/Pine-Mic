# Pine Mic for Windows

A Windows port of Pine Mic, written in Rust: use an Android phone as a low-latency microphone for your PC. The wire protocol used to talk to the phone is **unchanged**, so the existing Android app works as-is.

## How it works on Windows

Windows cannot create a virtual audio device without a kernel driver, so instead of PipeWire this port uses **VB-Audio Virtual Cable** (free):

```
Phone --(Wi-Fi UDP / USB adb)--> pine-mic.exe --> "CABLE Input" ==> "CABLE Output" --> Discord / OBS / Zoom ...
           Opus 48 kHz           decoder, RNNoise,   (playback)       (microphone)
                                 gain, mute
```

The program plays the phone's audio into the **CABLE Input** playback device. In other apps, select **CABLE Output** as your microphone.

## Requirements

1. **Rust** (MSVC toolchain): <https://rustup.rs>, then `rustup default stable-x86_64-pc-windows-msvc`.
2. **Visual Studio Build Tools** with the "Desktop development with C++" workload.
3. **CMake** (needed to build libopus): `winget install Kitware.CMake`, then restart your terminal.
4. **VB-Audio Virtual Cable**: <https://vb-audio.com/Cable/>. Reboot Windows after installing it.
5. For USB mode: **Android platform-tools** (`adb.exe`) and USB debugging enabled on the phone.

## Build

```
cargo build --release
```

The executable ends up at `target\release\pine-mic.exe`.

## Usage

**GUI** (default): run `pine-mic.exe`. Closing the window minimizes the app to the system tray (left-click the tray icon to show/hide the window, right-click for the menu).

1. The output device is picked automatically ("Auto (VB-Cable)"). You can choose a different one from the dropdown; the choice is remembered.
2. Pick your phone from the discovered list (or type its IP) and click "Connect Wi-Fi". For USB, click "Connect USB".
3. In Discord, OBS, Zoom, etc., select **CABLE Output (VB-Audio Virtual Cable)** as the microphone.

**CLI** (headless):

```
pine-mic --cli                       # first phone found on the LAN
pine-mic --cli --ip 192.168.1.23     # over Wi-Fi
pine-mic --cli --usb --denoise       # over USB, with noise suppression
pine-mic --list-devices              # list output devices
pine-mic --device "CABLE Input"      # choose the output device (remembered)
pine-mic --help
```

Flags: `--cli --ip --port --discovery-port --usb --denoise --gain --latency-ms --device --list-devices --no-tray`.

Environment variables: `PINE_MIC_ADB` (path to `adb.exe`), `PHONEMIC_DEBUG=1` (audio debug output).

## Windows Firewall

On first launch Windows will ask for network access. Allow **private networks**: UDP 54545 (audio) and UDP 54546 (phone auto-discovery) are used. USB mode does not use the network.

## Android app compatibility

| | |
|---|---|
| Frame | `[seq: u16 BE][opus]`, 48 kHz, mono, 10 ms |
| Wi-Fi | UDP 54545; the PC sends `PMIC_HELLO1` once per second and `PMIC_BYE1` on disconnect |
| USB | TCP 54545 via `adb forward`; each frame is prefixed with `[len: u16 BE]` |
| Discovery | UDP broadcast on 54546: `PHONEMIC1;<name>;<port>` |

## Differences from the Linux version

| Linux | Windows |
|---|---|
| Virtual microphone created via PipeWire / `pactl` | Requires VB-Cable; "Create / Rename / Remove Mic" is replaced by output device selection (`--device`) |
| User-chosen microphone name | Name is set by the driver (CABLE Output) |
| PulseAudio (`libpulse-simple`) | WASAPI via `cpal`, jitter buffer, 48 kHz to device-rate resampler |
| Default buffer 15 ms | 30 ms (WASAPI shared mode is less stable with tiny buffers); adjustable with `--latency-ms` |
| Tray via `ksni` / StatusNotifierItem | `tray-icon`, running in its own thread with its own message loop |
| X11/Wayland workaround (`--x11`, `--wayland`) | Not needed; flags removed |
| `adb` from PATH | `adb.exe` is searched in `PINE_MIC_ADB`, PATH, `ANDROID_HOME`, and the Android Studio SDK folder; launched without a flashing console window |
| n/a | Windows UDP quirk (`ConnectionReset` after ICMP) is handled |

Unchanged: Opus decoding with packet-loss concealment, duplicate/late packet dropping, backlog trimming, RNNoise (`nnnoiseless`), gain 0-8, mute, level meter, phone auto-discovery.

## Project layout

```
Cargo.toml
src/
  main.rs     entry point, CLI arguments, GUI and CLI modes
  state.rs    shared state (atomics, status, phones, window control)
  winutil.rs  all unsafe code: Win32 (window, console, tray message loop)
  audio.rs    Opus, RNNoise, jitter buffer, WASAPI output, device selection
  net.rs      Wi-Fi/USB protocol, auto-discovery
  tray.rs     system tray icon
  gui.rs      egui window
```

## Troubleshooting

- **No sound in the app:** make sure you selected *CABLE Output* as the microphone, and that this program shows "Mic active" and "Connected".
- **"virtual cable not found":** VB-Cable is not installed, or Windows was not rebooted after installing it. Alternatively, pick another output device manually.
- **Crackling or dropouts:** increase the buffer, e.g. `--latency-ms 50`.
- **Phone is not discovered:** check the firewall, make sure the PC and phone are on the same subnet, or enter the IP manually.
- **USB does not connect:** run `adb devices`; the phone must be listed as `device`.

## Not yet verified

This code was written without being compiled or run (the environment it was written in had no network access and no Windows). It has not been checked by a compiler or tested on real hardware. The areas most likely to need fixes:

1. **Crate API versions:** `cpal 0.15`, `tray-icon 0.19`, `eframe 0.29`, `windows-sys 0.59`, `audiopus 0.2`. Small signature mismatches are quick to fix.
2. **Building libopus** through CMake on your machine.
3. **Hiding/showing the window via Win32** (the window is found by its "Pine Mic" title).
4. **The tray running in its own thread** with a dedicated message loop.
5. **Audio:** WASAPI format selection, resampling, and buffer behavior on different sound cards.

If `cargo build` reports errors or the app misbehaves, report the error text or a description of the problem.
