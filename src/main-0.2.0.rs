#![windows_subsystem = "windows"]

//! EPulse (`epulse`) — Ultra-Lightweight Windows Background System Tray Music Player
//! Author: Edinter
//!
//! Features:
//! - Plays MP3, WAV, FLAC, OGG (Vorbis), M4A and AAC in the background, streaming from disk.
//! - Gapless playback (two tracks are always queued ahead) with optional shuffle and repeat.
//! - In-memory 32x32 RGBA tray icons: green Play badge = playing, red Pause badge = disabled.
//! - Left-click the tray icon to toggle music; right-click for rich context menu.
//! - Global multimedia keyboard keys support (Play/Pause, Next Track, Previous Track).
//! - Automatic detection of new or deleted songs without restarting the app.
//! - Live "Now Playing" track title in the menu (escaped for Win32 &) and hover tooltip.
//! - Volume submenu with Mute toggle and live percentage in the title.
//! - Previous Track (⏮) and Next Track (⏭) navigation (even while Repeat is active).
//! - Clean Settings submenu for "Run on Windows Startup" and "Auto-Play".
//! - Single instance, clean exit (no ghost tray icon), recovers from audio-device changes.
//! - Config lives in `%APPDATA%\EPulse\config.ini` (or alongside exe in portable mode).
//! - Problems are written to `epulse.log` next to the config (capped at 64 KB).

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
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
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentThreadId};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostQuitMessage, PostThreadMessageW, RegisterHotKey,
    TranslateMessage, UnregisterHotKey, MSG,
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
const WM_TRACK_CHANGED: u32 = 0x0400 + 1;

// Win32 Global Media Keys Constants
const WM_HOTKEY: u32 = 0x0312;
const HOTKEY_PLAY_PAUSE: i32 = 101;
const HOTKEY_NEXT_TRACK: i32 = 102;
const HOTKEY_PREV_TRACK: i32 = 103;
const VK_MEDIA_NEXT_TRACK: u32 = 0xB0;
const VK_MEDIA_PREV_TRACK: u32 = 0xB1;
const VK_MEDIA_PLAY_PAUSE: u32 = 0xB3;

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

// ───────────────────────────── Config & Commands ─────────────────────────────

/// Commands sent from the Win32 tray thread to the audio worker thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioCommand {
    SetEnabled(bool),
    NextTrack,
    PrevTrack,
    SetVolume(u8),
    SetMute(bool),
    SetShuffle(bool),
    SetRepeat(bool),
    Rescan,
    Shutdown,
}

/// Messages sent from the audio worker thread back to the Win32 tray thread.
#[derive(Debug, Clone)]
enum UiMessage {
    TrackChanged(String),
}

/// Persistent user configuration (`config.ini`, simple key=value).
#[derive(Debug, Clone)]
struct AppConfig {
    auto_play_on_startup: bool,
    shuffle: bool,
    repeat: bool,
    volume_percent: u8,
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
            repeat: false,
            volume_percent: 75,
            custom_music_dir: None,
        };

        match File::open(Self::config_path()) {
            Ok(file) => {
                for line in BufReader::new(file).lines().map_while(Result::ok) {
                    let trimmed = line.trim_start_matches('\u{feff}').trim();
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
                            "repeat" => cfg.repeat = v == "true" || v == "1",
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
                            _ => {}
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

    fn save(&self) -> std::io::Result<()> {
        let path = Self::config_path();
        let tmp = path.with_extension("ini.tmp");
        {
            let mut file = File::create(&tmp)?;
            writeln!(file, "# {} Configuration", APP_NAME)?;
            writeln!(file, "auto_play_on_startup={}", self.auto_play_on_startup)?;
            writeln!(file, "shuffle={}", self.shuffle)?;
            writeln!(file, "repeat={}", self.repeat)?;
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
                    (22u8, 163u8, 74u8) // #16A34A Emerald
                } else {
                    (185u8, 28u8, 28u8) // #B91C1C Crimson
                };

                let is_symbol = if enabled {
                    x >= 11
                        && x <= 22
                        && (y as f32) >= (10.0 + (x - 11) as f32 * 0.5)
                        && (y as f32) <= (21.0 - (x - 11) as f32 * 0.5)
                } else {
                    // Crisp vertical Pause bars (||) without the cluttered diagonal slash
                    ((x >= 11 && x <= 14) || (x >= 17 && x <= 20)) && (y >= 10 && y <= 21)
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

fn tooltip(playing: bool, title: Option<&str>) -> String {
    let state = if playing { "Playing" } else { "Paused" };
    match title {
        Some(t) => {
            let short: String = if t.chars().count() > 70 {
                let prefix: String = t.chars().take(67).collect();
                format!("{prefix}...")
            } else {
                t.to_string()
            };
            format!("EPulse: {short} ({state})")
        }
        None => format!("EPulse: {state} (Left-click icon to toggle)"),
    }
}

// ───────────────────────────── Playlist ─────────────────────────────

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
    idx: usize,
    shuffle: bool,
    rng: u64,
    last_dir_mtime: Option<SystemTime>,
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
            last_dir_mtime: None,
        }
    }

    fn rebuild(&mut self) {
        self.last_dir_mtime = fs::metadata(&self.dir).and_then(|m| m.modified()).ok();
        self.tracks = scan_audio_files(&self.dir);
        if self.shuffle {
            shuffle(&mut self.tracks, &mut self.rng);
        }
        self.idx = 0;
    }

    /// Automatically detects if files were added or removed from the folder
    fn check_rescan(&mut self) -> bool {
        let current_mtime = fs::metadata(&self.dir).and_then(|m| m.modified()).ok();
        if current_mtime.is_some() && current_mtime != self.last_dir_mtime {
            self.rebuild();
            true
        } else {
            false
        }
    }

    fn next(&mut self) -> Option<PathBuf> {
        if self.idx >= self.tracks.len() {
            self.rebuild();
        }
        let path = self.tracks.get(self.idx)?.clone();
        self.idx += 1;
        Some(path)
    }

    fn rewind(&mut self, n: usize) {
        let len = self.tracks.len();
        if len > 0 {
            self.idx = (self.idx + len - (n % len)) % len;
        }
    }
}

// ───────────────────────────── Audio worker ─────────────────────────────

fn vol_to_gain(percent: u8) -> f32 {
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
    // 512 KB streaming buffer prevents disk I/O jitter from causing audio crackles or pops
    Decoder::new(BufReader::with_capacity(512 * 1024, file)).ok()
}

/// Wraps a rodio source to trigger a callback as soon as the first sample is pulled for playback.
struct TrackNotifier<S> {
    source: S,
    skipped: bool,
    on_start: Option<Box<dyn FnOnce() -> bool + Send + 'static>>,
}

impl<S: rodio::Source> rodio::Source for TrackNotifier<S>
where
    S::Item: rodio::Sample,
{
    #[inline]
    fn current_frame_len(&self) -> Option<usize> {
        self.source.current_frame_len()
    }
    #[inline]
    fn channels(&self) -> u16 {
        self.source.channels()
    }
    #[inline]
    fn sample_rate(&self) -> u32 {
        self.source.sample_rate()
    }
    #[inline]
    fn total_duration(&self) -> Option<Duration> {
        self.source.total_duration()
    }
}

impl<S: Iterator> Iterator for TrackNotifier<S> {
    type Item = S::Item;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(cb) = self.on_start.take() {
            if !cb() {
                self.skipped = true;
            }
        }
        if self.skipped {
            return None;
        }
        self.source.next()
    }
}

struct Worker {
    enabled: Arc<AtomicBool>,
    volume: u8,
    muted: bool,
    repeat: Arc<AtomicBool>,
    /// The track that is actually audible right now (set by the audio callback when it starts).
    now_playing: Arc<Mutex<Option<PathBuf>>>,
    playlist: Playlist,
    audio: Option<Audio>,
    audio_retry_at: Instant,
    queue_retry_at: Instant,
    last_device_check: Instant,
    audio_failure_logged: bool,
    ui_tx: Sender<UiMessage>,
    main_thread_id: u32,
}

impl Worker {
    fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

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
                let repeat = self.repeat.load(Ordering::Acquire);
                if let Ok(mut g) = self.now_playing.lock() {
                    *g = None;
                }
                if let Some(audio) = &self.audio {
                    if repeat {
                        // When repeat is active, the sink contains duplicates of the current song.
                        // Stop the sink and refill to cleanly advance to the next song in the playlist.
                        audio.sink.stop();
                        self.fill_queue();
                        let on = self.is_enabled();
                        if on {
                            audio.sink.play();
                        } else {
                            audio.sink.pause();
                        }
                    } else {
                        audio.sink.skip_one();
                        self.fill_queue();
                    }
                } else {
                    let _ = self.playlist.next();
                }
            }
            AudioCommand::PrevTrack => {
                let repeat = self.repeat.load(Ordering::Acquire);
                if let Ok(mut g) = self.now_playing.lock() {
                    *g = None;
                }
                if self.audio.is_some() {
                    let pending = self.audio.as_ref().map_or(0, |a| a.sink.len());
                    let rewind_count = if repeat { 2 } else { pending + 1 };
                    self.playlist.rewind(rewind_count);
                    if let Some(a) = &self.audio {
                        a.sink.stop();
                    }
                    self.fill_queue();
                    let on = self.is_enabled();
                    if let Some(a) = &self.audio {
                        if on {
                            a.sink.play();
                        } else {
                            a.sink.pause();
                        }
                    }
                } else {
                    self.playlist.rewind(1);
                }
            }
            AudioCommand::SetVolume(vol) => {
                self.volume = vol;
                if !self.muted {
                    if let Some(audio) = &self.audio {
                        audio.sink.set_volume(vol_to_gain(vol));
                    }
                }
            }
            AudioCommand::SetMute(muted) => {
                self.muted = muted;
                if let Some(audio) = &self.audio {
                    if muted {
                        audio.sink.set_volume(0.0);
                    } else {
                        audio.sink.set_volume(vol_to_gain(self.volume));
                    }
                }
            }
            AudioCommand::SetShuffle(on) => {
                self.playlist.shuffle = on;
                self.playlist.rebuild();
            }
            AudioCommand::SetRepeat(on) => {
                self.repeat.store(on, Ordering::Release);
            }
            AudioCommand::Rescan => {
                self.playlist.rebuild();
                self.fill_queue();
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

    fn ensure_audio(&mut self) {
        if self.audio.is_some() || Instant::now() < self.audio_retry_at {
            return;
        }
        let effective_vol = if self.muted { 0 } else { self.volume };
        match open_audio(effective_vol) {
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

    fn check_device(&mut self) {
        if self.last_device_check.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.last_device_check = Instant::now();
        let Some(audio) = &self.audio else {
            return;
        };
        let new_dev = default_device_name();
        if new_dev != audio.device {
            log_line(&format!(
                "default output device changed: {:?} -> {:?}",
                audio.device, new_dev
            ));
            let pending = audio.sink.len();
            let rewind_count = if self.repeat.load(Ordering::Acquire) {
                1.min(pending)
            } else {
                pending
            };
            self.playlist.rewind(rewind_count);
            self.audio = None;
            self.ensure_audio(); // Rebuild immediately if a device is ready
        }
    }

    fn fill_queue(&mut self) {
        if Instant::now() < self.queue_retry_at {
            return;
        }
        let Some(audio) = self.audio.as_ref() else {
            return;
        };
        let mut failures = 0usize;
        while audio.sink.len() < 2 {
            let repeat = self.repeat.load(Ordering::Acquire);
            let path = if repeat {
                let now = self.now_playing.lock().ok().and_then(|g| g.clone());
                match now {
                    Some(p) => Some(p),
                    None => {
                        if audio.sink.len() >= 1 {
                            return;
                        }
                        self.playlist.next()
                    }
                }
            } else {
                self.playlist.next()
            };

            let Some(track_path) = path else {
                self.queue_retry_at = Instant::now() + Duration::from_secs(3);
                return;
            };

            match open_decoder(&track_path) {
                Some(source) => {
                    let ui_tx = self.ui_tx.clone();
                    let main_thread_id = self.main_thread_id;
                    let repeat_flag = Arc::clone(&self.repeat);
                    let now_playing = Arc::clone(&self.now_playing);
                    let queued_with_repeat = repeat;
                    let path_for_cb = track_path.clone();
                    let title = track_path
                        .file_stem()
                        .and_then(|n| n.to_str())
                        .unwrap_or("Unknown Track")
                        .to_string();

                    let notifier = TrackNotifier {
                        source,
                        skipped: false,
                        on_start: Some(Box::new(move || {
                            // If Repeat was turned on after queueing, drop to repeat current song
                            if repeat_flag.load(Ordering::Acquire) && !queued_with_repeat {
                                return false;
                            }
                            if let Ok(mut g) = now_playing.lock() {
                                *g = Some(path_for_cb);
                            }
                            let _ = ui_tx.send(UiMessage::TrackChanged(title));
                            unsafe {
                                PostThreadMessageW(main_thread_id, WM_TRACK_CHANGED, 0, 0);
                            }
                            true
                        })),
                    };

                    audio.sink.append(notifier);
                    failures = 0;
                }
                None => {
                    if repeat {
                        if let Ok(mut g) = self.now_playing.lock() {
                            *g = None;
                        }
                    }
                    failures += 1;
                    if failures >= self.playlist.tracks.len().max(1) {
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
        self.playlist.check_rescan();
        self.fill_queue();
    }

    fn run(mut self, cmd_rx: Receiver<AudioCommand>) {
        loop {
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
        return;
    }

    let mut config = AppConfig::load();
    refresh_startup_path();

    let initial_playing = config.auto_play_on_startup;
    let is_enabled = Arc::new(AtomicBool::new(initial_playing));
    let (cmd_tx, cmd_rx): (Sender<AudioCommand>, Receiver<AudioCommand>) = mpsc::channel();
    let (ui_tx, ui_rx): (Sender<UiMessage>, Receiver<UiMessage>) = mpsc::channel();
    let main_thread_id = unsafe { GetCurrentThreadId() };

    let music_dir = config.music_dir();
    let worker_enabled = Arc::clone(&is_enabled);
    let worker_dir = music_dir.clone();
    let (worker_volume, worker_shuffle, worker_repeat) =
        (config.volume_percent, config.shuffle, config.repeat);

    let worker_repeat_arc = Arc::new(AtomicBool::new(worker_repeat));
    let worker_repeat_clone = Arc::clone(&worker_repeat_arc);
    let now_playing = Arc::new(Mutex::new(None));
    let now_playing_clone = Arc::clone(&now_playing);

    let worker_thread = thread::Builder::new()
        .name("epulse-audio".into())
        .spawn(move || {
            let now = Instant::now();
            Worker {
                enabled: worker_enabled,
                volume: worker_volume,
                muted: false,
                repeat: worker_repeat_clone,
                now_playing: now_playing_clone,
                playlist: Playlist::new(worker_dir, worker_shuffle),
                audio: None,
                audio_retry_at: now,
                queue_retry_at: now,
                last_device_check: now,
                audio_failure_logged: false,
                ui_tx,
                main_thread_id,
            }
            .run(cmd_rx);
        })
        .expect("Failed to spawn EPulse audio worker thread");

    let icon_active = build_status_icon(true);
    let icon_disabled = build_status_icon(false);
    let mut current_title: Option<String> = None;

    // Register optional Global Windows Multimedia Keys (Play/Pause, Next, Previous)
    unsafe {
        RegisterHotKey(std::ptr::null_mut(), HOTKEY_PLAY_PAUSE, 0, VK_MEDIA_PLAY_PAUSE);
        RegisterHotKey(std::ptr::null_mut(), HOTKEY_NEXT_TRACK, 0, VK_MEDIA_NEXT_TRACK);
        RegisterHotKey(std::ptr::null_mut(), HOTKEY_PREV_TRACK, 0, VK_MEDIA_PREV_TRACK);
    }

    // ── Upgraded Tray Menu ──
    let tray_menu = Menu::new();

    // 1. Now Playing Banner (Disabled, unclickable header)
    let item_now_playing = MenuItem::new("🎵 EPulse Music Player", false, None);

    // 2. Play / Pause with dynamic label
    let play_pause_label = if initial_playing {
        "⏸ Pause (Left-Click Icon)"
    } else {
        "▶ Resume Playback (Left-Click Icon)"
    };
    let item_play_pause = CheckMenuItem::new(play_pause_label, true, initial_playing, None);

    // 3. Navigation
    let item_prev_track = MenuItem::new("⏮ Previous Track", true, None);
    let item_next_track = MenuItem::new("⏭ Next Track", true, None);

    // 4. Playback modes
    let item_shuffle = CheckMenuItem::new("🔀 Shuffle", true, config.shuffle, None);
    let item_repeat = CheckMenuItem::new("🔁 Repeat Current Track", true, config.repeat, None);

    // 5. Volume Submenu with live percentage title and Mute toggle
    let nearest_volume = *VOLUME_LEVELS
        .iter()
        .min_by_key(|lvl| (**lvl as i16 - config.volume_percent as i16).abs())
        .unwrap_or(&75);
    let volume_menu = Submenu::new(format!("🔊 Volume ({}%)", config.volume_percent), true);
    let item_mute = CheckMenuItem::new("🔇 Mute", true, false, None);
    let _ = volume_menu.append(&item_mute);
    let _ = volume_menu.append(&PredefinedMenuItem::separator());

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

    // 6. Settings Submenu
    let settings_menu = Submenu::new("⚙ Settings", true);
    let item_auto_play = CheckMenuItem::new(
        "Auto-Play on EPulse Launch",
        true,
        config.auto_play_on_startup,
        None,
    );
    let item_run_on_boot = CheckMenuItem::new(
        "Run EPulse on Windows Startup",
        true,
        is_registered_in_startup(),
        None,
    );
    let _ = settings_menu.append(&item_auto_play);
    let _ = settings_menu.append(&item_run_on_boot);

    // 7. Folder, Rescan and Exit
    let item_open_folder = MenuItem::new("📁 Open Music Folder", true, None);
    let item_rescan = MenuItem::new("🔄 Rescan Music Folder", true, None);
    let item_quit = MenuItem::new("❌ Quit EPulse", true, None);

    let _ = tray_menu.append_items(&[
        &item_now_playing,
        &PredefinedMenuItem::separator(),
        &item_play_pause,
        &item_prev_track,
        &item_next_track,
        &PredefinedMenuItem::separator(),
        &item_shuffle,
        &item_repeat,
        &volume_menu,
        &PredefinedMenuItem::separator(),
        &item_open_folder,
        &item_rescan,
        &settings_menu,
        &PredefinedMenuItem::separator(),
        &item_quit,
    ]);

    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_menu_on_left_click(false)
        .with_tooltip(tooltip(initial_playing, None))
        .with_icon(if initial_playing {
            icon_active.clone()
        } else {
            icon_disabled.clone()
        })
        .build()
        .expect("Failed to create EPulse Windows system tray icon");

    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayIconEvent::receiver();

    let apply_playing = |on: bool, title: Option<&str>| {
        is_enabled.store(on, Ordering::Release);
        item_play_pause.set_checked(on);
        if on {
            item_play_pause.set_text("⏸ Pause (Left-Click Icon)");
        } else {
            item_play_pause.set_text("▶ Resume Playback (Left-Click Icon)");
        }
        let _ = cmd_tx.send(AudioCommand::SetEnabled(on));
        let _ = tray_icon.set_icon(Some(if on {
            icon_active.clone()
        } else {
            icon_disabled.clone()
        }));
        let _ = tray_icon.set_tooltip(Some(tooltip(on, title)));
    };

    // ── Win32 message loop ──
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);

            // Handle Global Multimedia Hotkeys (Keyboards with Play/Pause, Next, Prev)
            if msg.message == WM_HOTKEY {
                match msg.wParam as i32 {
                    HOTKEY_PLAY_PAUSE => {
                        apply_playing(!is_enabled.load(Ordering::Acquire), current_title.as_deref());
                    }
                    HOTKEY_NEXT_TRACK => {
                        let _ = cmd_tx.send(AudioCommand::NextTrack);
                    }
                    HOTKEY_PREV_TRACK => {
                        let _ = cmd_tx.send(AudioCommand::PrevTrack);
                    }
                    _ => {}
                }
            }

            // Handle track title changes from audio worker
            while let Ok(event) = ui_rx.try_recv() {
                match event {
                    UiMessage::TrackChanged(title) => {
                        let display_title = if title.chars().count() > 40 {
                            let truncated: String = title.chars().take(37).collect();
                            format!("{truncated}...")
                        } else {
                            title.clone()
                        };
                        // Escape Win32 accelerator ampersands (& -> &&)
                        let safe_menu_title = display_title.replace('&', "&&");
                        item_now_playing.set_text(format!("🎵 {safe_menu_title}"));
                        current_title = Some(title);
                        let is_on = is_enabled.load(Ordering::Acquire);
                        let _ = tray_icon.set_tooltip(Some(tooltip(is_on, current_title.as_deref())));
                    }
                }
            }

            // Left-Click Toggle
            while let Ok(event) = tray_channel.try_recv() {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    apply_playing(!is_enabled.load(Ordering::Acquire), current_title.as_deref());
                }
            }

            // Menu Items Handling
            while let Ok(event) = menu_channel.try_recv() {
                if event.id == item_play_pause.id() {
                    apply_playing(item_play_pause.is_checked(), current_title.as_deref());
                } else if event.id == item_prev_track.id() {
                    let _ = cmd_tx.send(AudioCommand::PrevTrack);
                } else if event.id == item_next_track.id() {
                    let _ = cmd_tx.send(AudioCommand::NextTrack);
                } else if event.id == item_shuffle.id() {
                    config.shuffle = item_shuffle.is_checked();
                    let _ = config.save();
                    let _ = cmd_tx.send(AudioCommand::SetShuffle(config.shuffle));
                } else if event.id == item_repeat.id() {
                    config.repeat = item_repeat.is_checked();
                    let _ = config.save();
                    let _ = cmd_tx.send(AudioCommand::SetRepeat(config.repeat));
                } else if event.id == item_mute.id() {
                    let is_muted = item_mute.is_checked();
                    let _ = cmd_tx.send(AudioCommand::SetMute(is_muted));
                    if is_muted {
                        volume_menu.set_text("🔇 Volume (Muted)");
                    } else {
                        volume_menu.set_text(format!("🔊 Volume ({}%)", config.volume_percent));
                    }
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
                } else if event.id == item_rescan.id() {
                    let _ = cmd_tx.send(AudioCommand::Rescan);
                } else if event.id == item_quit.id() {
                    let _ = cmd_tx.send(AudioCommand::Shutdown);
                    PostQuitMessage(0);
                } else if let Some((level, _)) =
                    volume_items.iter().find(|(_, item)| event.id == item.id())
                {
                    let level = *level;
                    item_mute.set_checked(false);
                    for (l, item) in &volume_items {
                        item.set_checked(*l == level);
                    }
                    config.volume_percent = level;
                    let _ = config.save();
                    let _ = cmd_tx.send(AudioCommand::SetMute(false));
                    let _ = cmd_tx.send(AudioCommand::SetVolume(level));
                    volume_menu.set_text(format!("🔊 Volume ({level}%)"));
                }
            }
        }
    }

    // Clean shutdown: unregister hotkeys, remove tray icon, flush audio thread
    unsafe {
        UnregisterHotKey(std::ptr::null_mut(), HOTKEY_PLAY_PAUSE);
        UnregisterHotKey(std::ptr::null_mut(), HOTKEY_NEXT_TRACK);
        UnregisterHotKey(std::ptr::null_mut(), HOTKEY_PREV_TRACK);
    }
    let _ = cmd_tx.send(AudioCommand::Shutdown);
    drop(tray_icon);
    let deadline = Instant::now() + Duration::from_secs(1);
    while !worker_thread.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
}
