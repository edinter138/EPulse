# EPulse ⚡

> **Ultra-lightweight, zero-window Windows background system tray music player in pure Rust.**  
> Plays your favorite tracks silently in the background with gapless streaming, zero GUI bloat, and dynamic tray icons.

---

[![Build EPulse Windows Binary](https://github.com/edinter138/EPulse/actions/workflows/build.yml/badge.svg)](https://github.com/edinter138/EPulse/actions)
![Platform](https://img.shields.io/badge/Platform-Windows%2010%20%7C%2011-blue?logo=windows)
![Language](https://img.shields.io/badge/Language-Rust%202021-orange?logo=rust)
![License](https://img.shields.io/badge/License-GPL--3.0-green)
![Binary Size](https://img.shields.io/badge/Binary%20Size-~1.84%20MB-brightgreen)

---

## ✨ Features

* 🪟 **Headless & Zero-Window**: Runs entirely in the Windows System Tray (`#![windows_subsystem = "windows"]`). No Electron, no WebView2, no terminal popup, no window clutter.
* 🎧 **Multi-Format Audio Streaming**: Streams **MP3, FLAC, WAV, OGG (Vorbis), AAC, and M4A** directly from disk via a 512 KB buffer. Uncompressed audio is never dumped into RAM.
* 🟢 / 🔴 **Dynamic In-Memory Tray Badges**: Procedurally generated 32×32 RGBA icons (anti-aliased disc edge):
  * **Emerald Green (`▶`)**: Music is active & playing.
  * **Crimson Red (`||`)**: Music is paused/disabled.
* 🖱️ **Instant One-Click Controls**:
  * **Left-Click Tray Icon**: Instantly pauses or resumes playback.
  * **Right-Click Tray Icon**: Opens the quick context menu.
* 🔀 **Gapless Playback, Shuffle & Repeat**: Two tracks are always queued ahead, so there is no gap between songs. Shuffle uses a built-in zero-dependency `xorshift64` Fisher–Yates; Repeat loops the track you are hearing.
* 🔊 **Quick Volume Submenu**: One-click volume selection (25%, 50%, 75%, 100%) mapped to a quadratic curve matching human ear loudness perception.
* 🚀 **Native Windows Startup Registration**: One-click toggle in the menu writes directly to `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. Self-heals the registry path if you move `epulse.exe`.
* 🎧 **Audio Device Recovery**: Checks the default output device every few seconds. When it changes or disappears (Bluetooth headphones, USB DAC, unplugging), EPulse reopens the stream and replays the tracks that were queued. If no device exists yet (e.g. right after login) it keeps retrying.
* 📂 **Portable or Roamed Config**:
  * **Default**: Stores music & settings in `%APPDATA%\EPulse\`.
  * **Portable Mode**: Place `config.ini` next to `epulse.exe` and EPulse keeps all data alongside the executable.
* 🛡️ **Single-Instance Protection**: Named Win32 kernel mutex prevents duplicate processes and overlapping audio streams.

---

## 📥 Download & Quick Start

### 1. Download Pre-built Binary
You can grab the latest standalone `epulse.exe` directly from the [GitHub Actions tab](https://github.com/edinter138/EPulse/actions):
1. Open the **Releases** page and download `epulse.exe` from the latest `v*` release. (Builds from every push are also available as the `epulse-windows-x64` artifact in the Actions tab; those need a GitHub login and expire after 90 days.)
2. Double-click `epulse.exe` to launch.

> **Note:** the exe is not code-signed, so Windows SmartScreen may warn on first launch (*More info → Run anyway*), and some antivirus tools are suspicious of any unsigned app that can add a `Run` registry entry. The source is here, so you can build it yourself.

### 2. Adding Your Music
1. Right-click the **EPulse** tray icon near your taskbar clock.
2. Click **Open EPulse Music Folder**.
3. Drag and drop your `.mp3`, `.flac`, `.wav`, `.ogg`, `.m4a`, or `.aac` files into the folder.
4. EPulse will immediately stream your tracks!

---

## 🎛️ Upgraded Tray Context Menu

| Menu Item | Action |
| :--- | :--- |
| **🎵 &lt;Track Name&gt;** | Live header displaying currently playing track title |
| **⏸ Pause / ▶ Resume** | State-aware playback toggle (same as left-clicking the tray badge) |
| **⏮ Previous Track** | Rewinds to the previous track in your playlist |
| **⏭ Next Track** | Advances immediately to the next track |
| **🔀 Shuffle** | Toggles random playback order (Fisher-Yates PRNG) |
| **🔁 Repeat Current Track** | Loops the song you are hearing (the prefetched next track is dropped) |
| **🔊 Volume (&lt;N&gt;%) ▶** | Live volume indicator with **🔇 Mute** toggle & presets (25%, 50%, 75%, 100%) |
| **📁 Open Music Folder** | Opens the audio directory in Windows File Explorer |
| **⚙ Settings ▶** | Configure **Auto-Play on Launch** and **Run on Windows Startup** |
| **❌ Quit EPulse** | Stops playback and cleanly destroys the system tray icon |

---

## 🛠️ Building from Source

### Prerequisites
* Windows 10 or 11
* [Rust toolchain](https://rustup.rs/) (Rust 2021 edition)

### Build Commands

```powershell
# Clone the repository
git clone https://github.com/edinter138/EPulse.git
cd EPulse

# Compile the size & performance optimized release executable
cargo build --release
```

The resulting standalone executable will be located at:
```text
target/release/epulse.exe
```

---

## 📊 Comparison with Open-Source Players

| Player | Stack / Architecture | UI / Tray Mode | SLoC | Working Set RAM | Idle CPU | Windows Startup |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **EPulse** | **Rust** (`windows-sys`, `tray-icon`, `rodio`) | **Headless Win32 Tray Only** | **~1035 lines** | **~5 – 12 MB (estimate)** | **~0%** | **Built-in Registry Toggle** |
| [Termusic](https://github.com/tramhao/termusic) | Rust (`ratatui`, `tuirealm`, `mpv`) | Terminal TUI + Client/Server daemon | ~25,000+ | ~18 – 35 MB | ~0.5 – 2.0% | Manual Task Scheduler |
| [musikcube](https://github.com/clangen/musikcube) | C++ (`ncurses`, `boost`, `sqlite3`) | Terminal TUI + audio server | ~45,000+ | ~20 – 40 MB | ~0.3 – 1.5% | Manual shortcut |
| [foobar2000](https://www.foobar2000.org/) | C++ (Win32 GUI + proprietary SDK) | Full desktop window + tray minimize | Proprietary | ~10 – 25 MB | ~0.1 – 0.8% | Command-line `/play` |
| [mpv](https://github.com/mpv-player/mpv) | C (`FFmpeg`, `libass`, Lua) | Window / OSD | ~120,000+ | ~25 – 80 MB | ~0.2 – 1.0% | Custom `.vbs` wrapper |

---

## 🤝 Acknowledgements & Authors

* **Author**: [Edinter](https://github.com/edinter138)
* **AI Pair Programmer**: Developed in collaboration with **Google DeepMind / Gemini**.

---

## 📄 License

This project is licensed under the [GNU General Public License v3.0](LICENSE).
