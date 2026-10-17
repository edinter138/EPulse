#![windows_subsystem = "windows"]

//! EPulse (`epulse`) — Ultra-Lightweight Windows Background System Tray Music Player
//! Author: Edinter
//!
//! Features:
//! - Plays MP3, WAV, FLAC, OGG (Vorbis), M4A and AAC in the background, streaming from disk.
//! - Gapless playback (two tracks are always queued ahead) with optional shuffle.
//! - In-memory 32x32 RGBA tray icons: green Play badge = playing, red Pause badge = disabled.
//! - Left-click the tray icon (or use the menu) to disable/enable music.
//! - Volume submenu, "Run on Windows Startup" (HKCU\...\Run) and "Auto-Play on Startup".
//! - Single instance, clean exit (no ghost tray icon), recovers from audio-device changes.
//! - Config lives in `%APPDATA%\EPulse\config.ini`. If a `config.ini` already sits next to
//!   `epulse.exe`, EPulse runs in portable mode and keeps everything next to the exe instead.
//! - Problems are written to `epulse.log` next to the config (capped at 64 KB).

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rodio::cpal::traits::{DeviceTrait, HostTrait};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostQuitMessage, TranslateMessage, MSG,
};
use winreg::enums::*;
use winreg::RegKey;

const APP_NAME: &str = "EPulse";
const APP_REGISTRY_NAME: &str = "EPulse";
const REGISTRY_RUN_PATH: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
// Symphonia has no Opus decoder, so `.opus` is intentionally not listed.
const SUPPORTED_EXTENSIONS: &[&str] = &["mp3", "wav", "flac", "ogg", "m4a", "aac"];
const VOLUME_LEVELS: [u8; 4] = [25, 50, 75, 100];
const MAX_SCAN_DEPTH: u32 = 16;

// ───────────────────────────── Logging ─────────────────────────────

/// Appends a line to `epulse.log` (best effort). The file is deleted once it passes 64 KB.
fn log_line(msg: &str) {
    let path = AppConfig::data_dir().join("epulse.log");
    if fs::metadata(&path).map(|m| m.len() > 64 * 1024).unwrap_or(false) {
        let _ = fs::remove_file(&path);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{secs}] {msg}");
    }
}

// ───────────────────────────── Single instance ─────────────────────────────

/// Returns `false` if another EPulse process already owns the named mutex.
/// The handle is intentionally never closed: it must live as long as the process.
fn acquire_single_instance() -> bool {
    let name: Vec<u16> = "Local\\EPulse_SingleInstance\0".encode_utf16().collect();
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        !handle.is_null() && GetLastError() != ERROR_ALREADY_EXISTS
    }
}

// ───────────────────────────── Config ─────────────────────────────

/// Commands sent from the Win32 tray thread to the audio worker thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioCommand {
    SetEnabled(bool),
    NextTrack,
    SetVolume(u8),
    SetShuffle(bool),
    Shutdown,
}

/// Persistent user configuration (`config.ini`, simple key=value).
#[derive(Debug, Clone)]
struct AppConfig {
    /// `true`: music starts playing on every launch. `false`: EPulse starts disabled.
    auto_play_on_startup: bool,
    shuffle: bool,
    volume_percent: u8,
    /// Only set when the user configured a custom folder. It is kept as-is even if the folder
    /// is currently unavailable (e.g. an external drive that is not mounted yet at boot).
    custom_music_dir: Option<PathBuf>,
}

impl AppConfig {
    fn exe_dir() -> PathBuf {
        env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Portable mode if `config.ini` already sits next to the exe, otherwise `%APPDATA%\EPulse`.
    fn data_dir() -> PathBuf {
        static DIR: OnceLock<PathBuf> = OnceLock::new();
        DIR.get_or_init(|| {
            let exe_dir = Self::exe_dir();
            if exe_dir.join("config.ini").exists() {
                return exe_dir;
            }
            if let Some(appdata) = env::var_os("APPDATA") {
                let dir = PathBuf::from(appdata).join(APP_NAME);
                if fs::create_dir_all(&dir).is_ok() {
                    return dir;
                }
            }
            exe_dir
        })
        .clone()
    }

    fn config_path() -> PathBuf {
        Self::data_dir().join("config.ini")
    }

    /// Default music folder; created lazily, only when it is actually used.
    fn default_music_dir() -> PathBuf {
        let dir = Self::data_dir().join("music");
        let _ = fs::create_dir_all(&dir);
        dir
    }

    fn music_dir(&self) -> PathBuf {
        match &self.custom_music_dir {
            Some(dir) => dir.clone(),
            None => Self::default_music_dir(),
        }
    }

    fn load() -> Self {
        let mut cfg = Self {
            auto_play_on_startup: true,
            shuffle: false,
            volume_percent: 75,
            custom_music_dir: None,
        };

        match File::open(Self::config_path()) {
            Ok(file) => {
                for line in BufReader::new(file).lines().map_while(Result::ok) {
                    let trimmed = line.trim_start_matches('\u{feff}').trim(); // tolerate a BOM
                    if trimmed.is_empty() || trimmed.starts_with('#') {
                        continue;
                    }
                    if let Some((key, val)) = trimmed.split_once('=') {
                        let v = val.trim();
                        match key.trim() {
                            "auto_play_on_startup" => {
                                cfg.auto_play_on_startup = v == "true" || v == "1"
                            }
                            "shuffle" => cfg.shuffle = v == "true" || v == "1",
                            "volume_percent" => {
                                if let Ok(vol) = v.parse::<u8>() {
                                    cfg.volume_percent = vol.min(100);
                                }
                            }
                            "music_dir" => {
                                if !v.is_empty() {
                                    cfg.custom_music_dir = Some(PathBuf::from(v));
                                }
                            }
                            _ => {} // unknown / legacy keys (e.g. music_enabled) are ignored
                        }
                    }
                }
            }
            Err(_) => {
                let _ = cfg.save();
            }
        }
        cfg
    }

    /// Writes to a temp file and renames it, so a crash can never leave an empty config.
    fn save(&self) -> std::io::Result<()> {
        let path = Self::config_path();
        let tmp = path.with_extension("ini.tmp");
        {
            let mut file = File::create(&tmp)?;
            writeln!(file, "# {} Configuration", APP_NAME)?;
            writeln!(file, "auto_play_on_startup={}", self.auto_play_on_startup)?;
            writeln!(file, "shuffle={}", self.shuffle)?;
            writeln!(file, "volume_percent={}", self.volume_percent)?;
            if let Some(dir) = &self.custom_music_dir {
                writeln!(file, "music_dir={}", dir.display())?;
            }
        }
        fs::rename(&tmp, &path)
    }
}

// ───────────────────────────── Windows startup (registry) ─────────────────────────────

fn startup_command() -> std::io::Result<String> {
    // `--autostart` is no longer needed by the app but is kept so old entries stay identical.
    let exe_path = env::current_exe()?;
    Ok(format!("\"{}\" --autostart", exe_path.display()))
}

fn is_registered_in_startup() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    match hkcu.open_subkey_with_flags(REGISTRY_RUN_PATH, KEY_READ) {
        Ok(run_key) => run_key.get_value::<String, _>(APP_REGISTRY_NAME).is_ok(),
        Err(_) => false,
    }
}

fn set_registered_in_startup(enable: bool) -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (run_key, _) = hkcu.create_subkey(REGISTRY_RUN_PATH)?;
    if enable {
        run_key.set_value(APP_REGISTRY_NAME, &startup_command()?)?;
    } else {
        let _ = run_key.delete_value(APP_REGISTRY_NAME);
    }
    Ok(())
}

/// If EPulse is registered for startup but the exe was moved, point the entry at the new path.
fn refresh_startup_path() {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(run_key) = hkcu.open_subkey_with_flags(REGISTRY_RUN_PATH, KEY_READ | KEY_WRITE) {
        if let (Ok(current), Ok(expected)) = (
            run_key.get_value::<String, _>(APP_REGISTRY_NAME),
            startup_command(),
        ) {
            if current != expected {
                let _ = run_key.set_value(APP_REGISTRY_NAME, &expected);
            }
        }
    }
}

// ───────────────────────────── Tray icon ─────────────────────────────

/// Procedurally renders a 32x32 RGBA icon:
/// enabled = green disc + white Play triangle, disabled = red disc + white Pause bars + slash.
fn build_status_icon(enabled: bool) -> Icon {
    const SIZE: u32 = 32;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let center = 15.5_f32;
    let radius = 14.0_f32;

    for y in 0..SIZE {
        for x in 0..SIZE {
            let idx = ((y * SIZE + x) * 4) as usize;
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();

            if dist <= radius {
                let alpha = if dist > radius - 1.2 {
                    ((radius - dist) / 1.2 * 255.0).clamp(0.0, 255.0) as u8
                } else {
                    255
                };

                let (mut r, mut g, mut b) = if enabled {
                    (22u8, 163u8, 74u8) // #16A34A
                } else {
                    (185u8, 28u8, 28u8) // #B91C1C
                };

                let is_symbol = if enabled {
                    x >= 11
                        && x <= 22
                        && (y as f32) >= (10.0 + (x - 11) as f32 * 0.5)
                        && (y as f32) <= (21.0 - (x - 11) as f32 * 0.5)
                } else {
                    let in_bars =
                        ((x >= 11 && x <= 14) || (x >= 17 && x <= 20)) && (y >= 10 && y <= 21);
                    let in_slash = (x as i32 - y as i32).abs() <= 1 && dist <= 10.5;
                    in_bars || in_slash
                };

                if is_symbol {
                    r = 255;
                    g = 255;
                    b = 255;
                }

                rgba[idx] = r;
                rgba[idx + 1] = g;
                rgba[idx + 2] = b;
                rgba[idx + 3] = alpha;
            }
        }
    }

    Icon::from_rgba(rgba, SIZE, SIZE).expect("Failed to construct EPulse tray icon")
}

fn tooltip(playing: bool) -> &'static str {
    if playing {
        "EPulse: Playing (Left-click icon to disable)"
    } else {
        "EPulse: Disabled (Left-click icon to enable)"
    }
}

// ───────────────────────────── Playlist ─────────────────────────────

/// Recursively collects audio files. Uses `file_type()` (does not follow symlinks/junctions),
/// so link loops cannot cause unbounded recursion; depth is capped as a second safety net.
fn scan_into(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            scan_into(&path, depth + 1, out);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if SUPPORTED_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) {
                out.push(path);
            }
        }
    }
}

fn scan_audio_files(dir: &Path) -> Vec<PathBuf> {
    let mut tracks = Vec::new();
    scan_into(dir, 0, &mut tracks);
    tracks.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    tracks
}

fn next_rand(state: &mut u64) -> u64 {
    // xorshift64: plenty for shuffling, no extra dependency
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn shuffle<T>(items: &mut [T], state: &mut u64) {
    for i in (1..items.len()).rev() {
        let j = (next_rand(state) % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

struct Playlist {
    dir: PathBuf,
    tracks: Vec<PathBuf>,
    /// Index of the next track to hand out.
    idx: usize,
    shuffle: bool,
    rng: u64,
}

impl Playlist {
    fn new(dir: PathBuf, shuffle: bool) -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1;
        Self {
            dir,
            tracks: Vec::new(),
            idx: 0,
            shuffle,
            rng: seed,
        }
    }

    fn rebuild(&mut self) {
        self.tracks = scan_audio_files(&self.dir);
        if self.shuffle {
            shuffle(&mut self.tracks, &mut self.rng);
        }
        self.idx = 0;
    }

    /// Next track. At the end of a cycle the folder is rescanned (and reshuffled), so newly
    /// added songs are picked up without restarting EPulse.
    fn next(&mut self) -> Option<PathBuf> {
        if self.idx >= self.tracks.len() {
            self.rebuild();
        }
        let path = self.tracks.get(self.idx)?.clone();
        self.idx += 1;
        Some(path)
    }

    /// Steps back `n` tracks (used to replay queued tracks after the audio device changed).
    fn rewind(&mut self, n: usize) {
        let len = self.tracks.len();
        if len > 0 {
            self.idx = (self.idx + len - (n % len)) % len;
        }
    }
}

// ───────────────────────────── Audio worker ─────────────────────────────

fn vol_to_gain(percent: u8) -> f32 {
    // Squared curve: closer to perceived loudness than a linear amplitude scale.
    let v = (percent as f32 / 100.0).clamp(0.0, 1.0);
    v * v
}

fn default_device_name() -> Option<String> {
    rodio::cpal::default_host()
        .default_output_device()?
        .name()
        .ok()
}

struct Audio {
    _stream: OutputStream,
    _handle: OutputStreamHandle,
    sink: Sink,
    device: Option<String>,
}

fn open_audio(volume: u8) -> Option<Audio> {
    let device = default_device_name();
    let (stream, handle) = OutputStream::try_default().ok()?;
    let sink = Sink::try_new(&handle).ok()?;
    sink.set_volume(vol_to_gain(volume));
    Some(Audio {
        _stream: stream,
        _handle: handle,
        sink,
        device,
    })
}

fn open_decoder(path: &Path) -> Option<Decoder<BufReader<File>>> {
    let file = File::open(path).ok()?;
    Decoder::new(BufReader::with_capacity(16 * 1024, file)).ok()
}

struct Worker {
    enabled: Arc<AtomicBool>,
    volume: u8,
    playlist: Playlist,
    audio: Option<Audio>,
    audio_retry_at: Instant,
    queue_retry_at: Instant,
    last_device_check: Instant,
    audio_failure_logged: bool,
}

impl Worker {
    fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Returns `false` when the worker should exit.
    fn handle(&mut self, cmd: AudioCommand) -> bool {
        match cmd {
            AudioCommand::SetEnabled(on) => {
                if let Some(audio) = &self.audio {
                    if on {
                        audio.sink.play();
                    } else {
                        audio.sink.pause();
                    }
                }
            }
            AudioCommand::NextTrack => {
                if let Some(audio) = &self.audio {
                    audio.sink.skip_one(); // skips only the current track, keeps the prefetched one
                    self.fill_queue();
                } else {
                    let _ = self.playlist.next();
                }
            }
            AudioCommand::SetVolume(vol) => {
                self.volume = vol;
                if let Some(audio) = &self.audio {
                    audio.sink.set_volume(vol_to_gain(vol));
                }
            }
            AudioCommand::SetShuffle(on) => {
                self.playlist.shuffle = on;
                self.playlist.rebuild();
            }
            AudioCommand::Shutdown => {
                if let Some(audio) = &self.audio {
                    audio.sink.stop();
                }
                return false;
            }
        }
        true
    }

    /// (Re)opens the output stream when needed. Retries every 2 s, which covers audio services
    /// that are not ready yet right after Windows login.
    fn ensure_audio(&mut self) {
        if self.audio.is_some() || Instant::now() < self.audio_retry_at {
            return;
        }
        match open_audio(self.volume) {
            Some(audio) => {
                log_line(&format!("audio output opened: {:?}", audio.device));
                self.audio_failure_logged = false;
                self.audio = Some(audio);
            }
            None => {
                if !self.audio_failure_logged {
                    log_line("no audio output available, retrying every 2s");
                    self.audio_failure_logged = true;
                }
                self.audio_retry_at = Instant::now() + Duration::from_secs(2);
            }
        }
    }

    /// Detects a changed/removed default output device (headphones, Bluetooth, USB DAC) and
    /// rebuilds the stream. Tracks that were queued but not finished are replayed.
    fn check_device(&mut self) {
        if self.last_device_check.elapsed() < Duration::from_secs(3) {
            return;
        }
        self.last_device_check = Instant::now();
        let Some(audio) = &self.audio else {
            return;
        };
        let current = default_device_name();
        if current != audio.device {
            log_line(&format!(
                "default output device changed: {:?} -> {:?}",
                audio.device, current
            ));
            let pending = audio.sink.len();
            self.playlist.rewind(pending);
            self.audio = None; // reopened by ensure_audio()
        }
    }

    /// Keeps two tracks queued so there is no gap between songs.
    fn fill_queue(&mut self) {
        if Instant::now() < self.queue_retry_at {
            return;
        }
        let Some(audio) = self.audio.as_ref() else {
            return;
        };
        let mut failures = 0usize;
        while audio.sink.len() < 2 {
            let Some(path) = self.playlist.next() else {
                // Empty or unavailable folder: look again in a few seconds.
                self.queue_retry_at = Instant::now() + Duration::from_secs(3);
                return;
            };
            match open_decoder(&path) {
                Some(source) => {
                    audio.sink.append(source);
                    failures = 0;
                }
                None => {
                    failures += 1;
                    if failures >= self.playlist.tracks.len().max(1) {
                        // Every track failed to open/decode: back off instead of spinning.
                        self.queue_retry_at = Instant::now() + Duration::from_secs(3);
                        return;
                    }
                }
            }
        }
    }

    fn tick(&mut self) {
        if !self.is_enabled() {
            return;
        }
        self.ensure_audio();
        self.check_device();
        self.fill_queue();
    }

    fn run(mut self, cmd_rx: Receiver<AudioCommand>) {
        loop {
            // Wake instantly on a command, otherwise every 250 ms to top up the queue.
            match cmd_rx.recv_timeout(Duration::from_millis(250)) {
                Ok(cmd) => {
                    if !self.handle(cmd) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while let Ok(cmd) = cmd_rx.try_recv() {
                if !self.handle(cmd) {
                    return;
                }
            }
            self.tick();
        }
    }
}

// ───────────────────────────── main ─────────────────────────────

fn main() {
    std::panic::set_hook(Box::new(|info| log_line(&format!("panic: {info}"))));

    if !acquire_single_instance() {
        return; // EPulse is already running
    }

    let mut config = AppConfig::load();
    refresh_startup_path();

    // Simple, predictable rule: auto-play on => every launch starts playing; off => starts disabled.
    let initial_playing = config.auto_play_on_startup;
    let is_enabled = Arc::new(AtomicBool::new(initial_playing));
    let (cmd_tx, cmd_rx): (Sender<AudioCommand>, Receiver<AudioCommand>) = mpsc::channel();

    let music_dir = config.music_dir();
    // The Worker holds a cpal stream (not `Send`), so it is built inside the audio thread.
    let worker_enabled = Arc::clone(&is_enabled);
    let worker_dir = music_dir.clone();
    let (worker_volume, worker_shuffle) = (config.volume_percent, config.shuffle);
    let worker_thread = thread::Builder::new()
        .name("epulse-audio".into())
        .spawn(move || {
            let now = Instant::now();
            Worker {
                enabled: worker_enabled,
                volume: worker_volume,
                playlist: Playlist::new(worker_dir, worker_shuffle),
                audio: None,
                audio_retry_at: now,
                queue_retry_at: now,
                last_device_check: now,
                audio_failure_logged: false,
            }
            .run(cmd_rx);
        })
        .expect("Failed to spawn EPulse audio worker thread");

    let icon_active = build_status_icon(true);
    let icon_disabled = build_status_icon(false);

    // ── Tray menu ──
    let tray_menu = Menu::new();
    let item_enable_music =
        CheckMenuItem::new("Enable EPulse Music (Left-Click Icon)", true, initial_playing, None);
    let item_next_track = MenuItem::new("Skip to Next Track", true, None);
    let item_shuffle = CheckMenuItem::new("Shuffle", true, config.shuffle, None);

    let nearest_volume = *VOLUME_LEVELS
        .iter()
        .min_by_key(|lvl| (**lvl as i16 - config.volume_percent as i16).abs())
        .unwrap_or(&75);
    let volume_menu = Submenu::new("Volume", true);
    let volume_items: Vec<(u8, CheckMenuItem)> = VOLUME_LEVELS
        .iter()
        .map(|&lvl| {
            (
                lvl,
                CheckMenuItem::new(format!("{lvl}%"), true, lvl == nearest_volume, None),
            )
        })
        .collect();
    for (_, item) in &volume_items {
        let _ = volume_menu.append(item);
    }

    let item_auto_play =
        CheckMenuItem::new("Auto-Play Music on EPulse Startup", true, config.auto_play_on_startup, None);
    let item_run_on_boot =
        CheckMenuItem::new("Run EPulse on Windows Startup", true, is_registered_in_startup(), None);
    let item_open_folder = MenuItem::new("Open EPulse Music Folder", true, None);
    let item_quit = MenuItem::new("Quit EPulse", true, None);

    let _ = tray_menu.append_items(&[
        &item_enable_music,
        &item_next_track,
        &PredefinedMenuItem::separator(),
        &item_shuffle,
        &volume_menu,
        &PredefinedMenuItem::separator(),
        &item_auto_play,
        &item_run_on_boot,
        &PredefinedMenuItem::separator(),
        &item_open_folder,
        &PredefinedMenuItem::separator(),
        &item_quit,
    ]);

    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_menu_on_left_click(false) // left-click toggles music, right-click opens the menu
        .with_tooltip(tooltip(initial_playing))
        .with_icon(if initial_playing {
            icon_active.clone()
        } else {
            icon_disabled.clone()
        })
        .build()
        .expect("Failed to create EPulse Windows system tray icon");

    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayIconEvent::receiver();

    // Keeps icon, tooltip, menu state, shared flag and audio thread in sync.
    let apply_playing = |on: bool| {
        is_enabled.store(on, Ordering::Release);
        item_enable_music.set_checked(on);
        let _ = cmd_tx.send(AudioCommand::SetEnabled(on));
        let _ = tray_icon.set_icon(Some(if on {
            icon_active.clone()
        } else {
            icon_disabled.clone()
        }));
        let _ = tray_icon.set_tooltip(Some(tooltip(on)));
    };

    // ── Win32 message loop (blocks in the kernel, ~0% CPU when idle) ──
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);

            while let Ok(event) = tray_channel.try_recv() {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    apply_playing(!is_enabled.load(Ordering::Acquire));
                }
            }

            while let Ok(event) = menu_channel.try_recv() {
                if event.id == item_enable_music.id() {
                    apply_playing(item_enable_music.is_checked());
                } else if event.id == item_next_track.id() {
                    let _ = cmd_tx.send(AudioCommand::NextTrack);
                } else if event.id == item_shuffle.id() {
                    config.shuffle = item_shuffle.is_checked();
                    let _ = config.save();
                    let _ = cmd_tx.send(AudioCommand::SetShuffle(config.shuffle));
                } else if event.id == item_auto_play.id() {
                    config.auto_play_on_startup = item_auto_play.is_checked();
                    let _ = config.save();
                } else if event.id == item_run_on_boot.id() {
                    let enable_boot = item_run_on_boot.is_checked();
                    if set_registered_in_startup(enable_boot).is_err() {
                        item_run_on_boot.set_checked(!enable_boot);
                    }
                } else if event.id == item_open_folder.id() {
                    let _ = fs::create_dir_all(&music_dir);
                    let explorer = env::var_os("SystemRoot")
                        .map(|root| PathBuf::from(root).join("explorer.exe"))
                        .unwrap_or_else(|| PathBuf::from("explorer.exe"));
                    let _ = Command::new(explorer).arg(&music_dir).spawn();
                } else if event.id == item_quit.id() {
                    let _ = cmd_tx.send(AudioCommand::Shutdown);
                    PostQuitMessage(0); // ends the loop so the tray icon is dropped properly
                } else if let Some((level, _)) =
                    volume_items.iter().find(|(_, item)| event.id == item.id())
                {
                    let level = *level;
                    for (l, item) in &volume_items {
                        item.set_checked(*l == level);
                    }
                    config.volume_percent = level;
                    let _ = config.save();
                    let _ = cmd_tx.send(AudioCommand::SetVolume(level));
                }
            }
        }
    }

    // Clean shutdown: remove the tray icon, then give the audio thread up to 1 s to finish.
    let _ = cmd_tx.send(AudioCommand::Shutdown);
    drop(tray_icon);
    let deadline = Instant::now() + Duration::from_secs(1);
    while !worker_thread.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
}
