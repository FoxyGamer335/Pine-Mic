# Pine-Mic
App that lets you use your phone as a microphone
This project was developed and tested on **CachyOS** (Arch Linux). The entire codebase was built with the assistance of **Claude Sonnet 5.5** and **Gemini 4 Argon**.

# Prerequisites & Building (Linux)

## 1. Install Dependencies

You need the C build toolchain, `pkg-config`, `opus` and `libpulse` development headers, `pactl` (`libpulse` / `pulseaudio-utils`) with `pipewire-pulse` for virtual microphone management, and `adb` (`android-tools`) for USB mode.

**CachyOS / Arch Linux:**
```bash
sudo pacman -S --needed base-devel rustup pkgconf cmake opus libpulse pipewire pipewire-pulse android-tools
```
### Ubuntu / Debian:
```bash
sudo apt update
sudo apt install -y build-essential pkg-config cmake curl libopus-dev libpulse-dev pulseaudio-utils pipewire pipewire-pulse adb
```
### Fedora:

```bash
sudo dnf install -y @development-tools pkgconf-pkg-config cmake opus-devel pulseaudio-libs-devel pulseaudio-utils pipewire pipewire-pulseaudio android-tools
```
## 2. Set Up Rust

If you installed rustup via your package manager, enable the stable toolchain:
```bash
rustup default stable
```
If your distribution does not package rustup, install it via the official script:
```bash
curl --proto '=https' --tlsv1.2 -sSf [https://sh.rustup.rs](https://sh.rustup.rs) | sh
```
## 3. Compile

Clone the repository and build the release binary:
```bash
git clone [https://github.com/FoxyGamer335/Pine-Mic.git](https://github.com/FoxyGamer335/Pine-Mic.git)
cd Pine-Mic
cargo build --release
```
Once compiled, the executable will be located in `target/release/`

# Android App Prerequisites & Building

To build the Android application located in the `android/` folder, you will need:

1. **Java 17 (JDK 17)** (e.g., Eclipse Temurin).
2. **Android SDK** (including platform-tools and build-tools). Ensure that the `ANDROID_HOME` environment variable points to your SDK path.

#### Compilation

Navigate into the `android/` directory, grant execution permissions to Gradle, and build the APK:

```bash
cd android
chmod +x gradlew
./gradlew assembleDebug
```
Once built, the debug APK will be available at: `android/app/build/outputs/apk/debug/app-debug.apk`
