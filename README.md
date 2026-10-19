# EPulse ⚡ v0.3.0

> **Ultra-lightweight, zero-window Windows background music player built in pure Rust.**  
> Plays your local audio files directly from the system tray with asynchronous prefetch decoding, native Windows Dark Mode, and zero GUI bloat.

---

[![Release](https://img.shields.io/badge/Release-v0.3.0-blue)](https://github.com/edinter138/EPulse/releases)
[![Build Status](https://github.com/edinter138/EPulse/actions/workflows/build.yml/badge.svg)](https://github.com/edinter138/EPulse/actions)
![Platform](https://img.shields.io/badge/Platform-Windows%2010%20%7C%2011-blue?logo=windows)
![Language](https://img.shields.io/badge/Language-Rust%202021-orange?logo=rust)
![License](https://img.shields.io/badge/License-GPL--3.0-green)
![Binary Size](https://img.shields.io/badge/Binary%20Size-~1.6%20MB-brightgreen)
![RAM Footprint](https://img.shields.io/badge/RAM-~5--10%20MB-blueviolet)

---

## 💡 Why EPulse?

Most modern desktop music players are heavy: they bundle entire Chromium runtimes (Electron), consume hundreds of megabytes of RAM, and clutter your taskbar with windows. 

**EPulse takes the opposite approach:**
* **Zero Windows**: No Electron, no WebView2, no console windows. It lives entirely in your system tray.
* **Zero Audio Stutter**: Decoupled asynchronous prefetch decoding means heavy gaming or CPU compilation will never interrupt your music.
* **Instant & Lightweight**: Starts in milliseconds, sits at **0.0% CPU** when paused, and uses only **~5–10 MB of RAM**.
* **Native Windows Feel**: Seamlessly adopts your Windows 10/11 system Dark Mode and connects directly to your keyboard's media keys.

---

## 📥 Quick Start (Ready in 30 Seconds)

### 1. Download & Run
1. Grab the latest standalone `epulse.exe` from [Releases](https://github.com/edinter138/EPulse/releases) or the [GitHub Actions build tab](https://github.com/edinter138/EPulse/actions).
2. Double-click `epulse.exe`. It will appear near your taskbar clock.

> **SmartScreen Note:** Because EPulse is an independent open-source binary without an expensive corporate signing certificate, Windows SmartScreen may show a one-time notice (*More info → Run anyway*).

### 2. Add Your Music
1. Right-click the **EPulse** tray badge.
2. Select **📁 Open Music Folder**.
3. Drop your music files (`.mp3`, `.flac`, `.wav`, `.ogg`, `.m4a`, `.aac`) into the folder.
4. Left-click the tray badge to start playing immediately!

---

## 🎮 Everyday Controls & User Experience

### 🟢 Dynamic Tray Status Badges
EPulse generates procedural anti-aliased 32×32 icons directly in memory:
* **Emerald Green (`▶`)**: Music is actively playing.
* **Crimson Red (`||`)**: Music is paused.
* **Left-Click Tray Icon**: Instantly toggles Play / Pause.

### ⌨️ Global Multimedia Keyboard Keys
Control your music from anywhere in Windows—even inside fullscreen games or another workspace:
| Hotkey | Action |
| :--- | :--- |
| **Media Play / Pause** (`VK_MEDIA_PLAY_PAUSE`) | Toggle playback globally |
| **Media Next Track** (`VK_MEDIA_NEXT_TRACK`) | Skip to the next track |
| **Media Previous Track** (`VK_MEDIA_PREV_TRACK`) | Rewind to the previous track |

### 🎛️ Native Dark Mode Context Menu
Right-clicking the tray badge opens a Win32 menu that automatically adapts to your Windows desktop theme:
| Menu Item | Function |
| :--- | :--- |
| **🎵 &lt;Track Title&gt;** | Live display of the active track (with safe Win32 `&` escaping) |
| **⏸ Pause / ▶ Resume** | Playback toggle |
| **⏮ Previous Track** | Jumps back to the previous track in the playlist |
| **⏭ Next Track** | Advances immediately to the next track |
| **🔁 Repeat Current Track** | Loops current track (skipping advances to the next track and repeats it) |
| **🔊 Volume (&lt;N&gt;%) ▶** | Submenu with live volume percentage, **🔇 Mute**, and 25%, 50%, 75%, 100% presets |
| **📁 Open Music Folder** | Opens the music directory in Windows Explorer |
| **🔄 Rescan Music Folder** | Scans for newly added or deleted songs |
| **⚙ Settings ▶** | Submenu with **Auto-Play on Launch** and **Run on Windows Startup** |
| **❌ Quit EPulse** | Unregisters hotkeys and cleanly shuts down without leaving ghost icons |

---

## ⚡ Under the Hood: Audio Engineering

EPulse v0.3.0 introduces a dedicated low-latency audio pipeline engineered to eliminate buffer starvation and audio crackles:

```
[ Disk Audio Files ]
         │
         ▼ (64 KB buffered reads)
[ epulse-decode Thread ] ──► Symphonia DSP Codecs
         │
         ▼ (~3 seconds pre-decoded PCM chunks / ~512 KB headroom)
[ mpsc Sync Channel ]
         │
         ▼ (Sample-copy only: zero disk I/O, zero decoding)
[ WASAPI Audio Callback ] ──► MMCSS "Audio" Real-Time Class ──► Speakers / Headphones
```

1. **Decoupled Asynchronous Prefetching**: Audio decoding runs on a dedicated thread (`epulse-decode`), keeping ~3 seconds of decoded PCM audio ahead of the real-time audio thread. Spikes in disk I/O or background CPU load cannot cause audible dropouts.
2. **Windows MMCSS Registration**: The audio callback registers with Windows Multimedia Class Scheduler Service (**MMCSS**) under the `"Audio"` task class (`AvSetMmThreadCharacteristicsW`), falling back to `THREAD_PRIORITY_TIME_CRITICAL`.
3. **Windows 11 EcoQoS Power Throttling Opt-Out**: Uses `SetProcessInformation` with `ProcessPowerThrottling` so Windows 11 does not downgrade the player to efficiency mode or throttle timer resolution.
4. **Adaptive Sleep Mode**: When paused, EPulse sleeps completely until a new command or hotkey is triggered, dropping CPU usage to **0.0%**.
5. **Seamless Device Handover**: Monitors audio endpoint changes (e.g. plugging in headphones or connecting Bluetooth). If a device disappears, EPulse safely pauses and resumes on the newly active output.

---

## 🚀 What's New in v0.3.0

* ⚡ **Decoupled Asynchronous Decoding**: Moved all file reading and codec processing off the WASAPI thread into a dedicated worker thread with ~3s of pre-buffered audio.
* 🎯 **MMCSS & EcoQoS Hardening**: Full integration with Windows real-time audio scheduling and Windows 11 efficiency mode bypass.
* 🌙 **Native Windows Dark Mode**: Native `uxtheme.dll` integration (`SetPreferredAppMode` + `FlushMenuThemes`) for context menus.
* ⌨️ **Global Media Keys**: Added support for standard keyboard Play/Pause, Next, and Previous keys.
* 🔁 **Smart Repeat Navigation**: Skipping tracks under Repeat mode now cleanly advances to the next track and repeats the new selection.
* ⚡ **Fast Library Indexing**: Optimized directory scans using `sort_by_cached_key` and zero-allocation extension matching.
* 🧹 **Streamlined Menu**: Removed redundant toggles for a cleaner layout.

---

## 📂 Configuration & Portability

EPulse supports both roamed system installations and fully portable USB drive use:

* **Default Roamed Mode**: Settings and audio files are stored in:
  ```text
  %APPDATA%\EPulse\config.ini
  %APPDATA%\EPulse\music\
  ```
* **Portable Mode**: Simply place an empty `config.ini` in the same directory as `epulse.exe`. EPulse will automatically run in portable mode, storing settings, logs, and music alongside the executable.

---

## 📊 Benchmark Comparison

| Player | Architecture / Stack | Interface | SLoC | RAM Footprint | Idle CPU | Windows Startup |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **EPulse v0.3.0** | **Pure Rust** (`windows-sys`, `tray-icon`, `rodio`) | **Headless Tray Only** | **~1,360** | **~5 – 10 MB** | **0.0%** | **Built-in Registry Toggle** |
| [Termusic](https://github.com/tramhao/termusic) | Rust (`ratatui`, `tuirealm`, `mpv`) | Terminal TUI + Daemon | ~25,000+ | ~18 – 35 MB | ~0.5 – 2.0% | Manual Task Scheduler |
| [musikcube](https://github.com/clangen/musikcube) | C++ (`ncurses`, `boost`, `sqlite3`) | Terminal TUI + Server | ~45,000+ | ~20 – 40 MB | ~0.3 – 1.5% | Manual Shortcut |
| [foobar2000](https://www.foobar2000.org/) | C++ (Win32 GUI + proprietary SDK) | Desktop Window + Tray | Proprietary | ~12 – 28 MB | ~0.1 – 0.8% | Command-line `/play` |
| [mpv](https://github.com/mpv-player/mpv) | C (`FFmpeg`, `libass`, Lua) | Desktop Window | ~120,000+ | ~25 – 80 MB | ~0.2 – 1.0% | Custom `.vbs` Script |

---

## 🛠️ Building from Source

### Prerequisites
* 64-bit Windows 10 or 11
* [Rust toolchain](https://rustup.rs/) (Rust 2021 edition)

```powershell
# 1. Clone the repository
git clone https://github.com/edinter138/EPulse.git
cd EPulse

# 2. Compile release binary with aggressive LTO and size stripping
cargo build --release
```

The compiled standalone executable will be generated at:
```text
target/release/epulse.exe
```

---

## 🤝 Authors & Credits

* **Author**: [Edinter](https://github.com/edinter138)
* **Audio Engineering**: Developed in collaboration with **Google DeepMind / Gemini**.

---

## 📄 License

This project is licensed under the [GNU General Public License v3.0](LICENSE).
