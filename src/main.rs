use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::Duration;

use anyhow::Context;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MediaKeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use lofty::prelude::*;
use lofty::probe::Probe;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
    Frame, Terminal,
};
use ratatui_image::{picker::Picker, protocol::StatefulProtocol, StatefulImage};
use serde_json::{json, Value};
use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig};

const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "ogg", "wav", "m4a", "aac", "opus", "wma", "aiff", "alac",
];

fn get_config_dir() -> PathBuf {
    if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        PathBuf::from(config_home).join("tuiplay")
    } else if let Ok(home) = env::var("HOME") {
        PathBuf::from(home).join(".config/tuiplay")
    } else {
        PathBuf::from(".tuiplay")
    }
}

fn pseudo_random_index(len: usize) -> usize {
    if len <= 1 {
        return 0;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    (nanos as usize) % len
}

#[derive(Debug)]
enum AppAction {
    TogglePause,
    Next,
    Prev,
    Stop,
}

#[derive(PartialEq, Clone, Copy)]
enum ViewMode {
    Files,
    Queue,
    Zen,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum LoopMode {
    Off,
    Track,
    Queue,
}

impl LoopMode {
    fn next(self) -> Self {
        match self {
            LoopMode::Off => LoopMode::Track,
            LoopMode::Track => LoopMode::Queue,
            LoopMode::Queue => LoopMode::Off,
        }
    }

    fn label(self) -> &'static str {
        match self {
            LoopMode::Off => "Off",
            LoopMode::Track => "Track",
            LoopMode::Queue => "Queue",
        }
    }
}

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    is_dir: bool,
}

impl Entry {
    fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.to_string_lossy().to_string())
    }
}

// ---------------------------------------------------------------------------
// Theme & Colors
// ---------------------------------------------------------------------------

fn parse_hex_color(hex: &str) -> Color {
    let hex = hex.trim().trim_start_matches('#');
    let full_hex = if hex.len() == 3 {
        hex.chars().map(|c| format!("{c}{c}")).collect::<String>()
    } else {
        hex.to_string()
    };

    if full_hex.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&full_hex[0..2], 16),
            u8::from_str_radix(&full_hex[2..4], 16),
            u8::from_str_radix(&full_hex[4..6], 16),
        ) {
            return Color::Rgb(r, g, b);
        }
    }
    Color::Reset
}

#[derive(Clone, Copy)]
struct Theme {
    border: Color,
    title: Color,
    dir: Color,
    playing: Color,
    text: Color,
    highlight_bg: Color,
    highlight_fg: Color,
    volume: Color,
    yes: Color,
    no: Color,
    selector: Color,
    paused: Color,
    search: Color,
}

impl Theme {
    fn status_color(&self, has_track: bool, paused: bool) -> Color {
        if !has_track {
            self.no
        } else if paused {
            self.paused
        } else {
            self.playing
        }
    }

    fn load_or_default() -> (Self, Option<String>) {
        let local_path = PathBuf::from("theme.json");
        let config_path = get_config_dir().join("theme.json");

        let target_path = if local_path.exists() {
            Some(local_path)
        } else {
            Some(config_path).filter(|p| p.exists())
        };

        if let Some(path) = target_path {
            match fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<Value>(&content) {
                    Ok(v) => {
                        let get_color = |key: &str, default: &str| -> Color {
                            v.get(key)
                                .and_then(|val| val.as_str())
                                .map(parse_hex_color)
                                .unwrap_or_else(|| parse_hex_color(default))
                        };

                        let theme = Self {
                            border: get_color("border", "#cba6f7"),
                            title: get_color("title", "#f5c2e7"),
                            dir: get_color("dir", "#89b4fa"),
                            playing: get_color("playing", "#a6e3a1"),
                            text: get_color("text", "#cdd6f4"),
                            highlight_bg: get_color("highlight_bg", "#313244"),
                            highlight_fg: get_color("highlight_fg", "#cba6f7"),
                            volume: get_color("volume", "#fab387"),
                            yes: get_color("yes", "#a6e3a1"),
                            no: get_color("no", "#f38ba8"),
                            selector: get_color("selector", "#f9e2af"),
                            paused: get_color("paused", "#f9e2af"),
                            search: get_color("search", "#74c7ec"),
                        };
                        return (theme, None);
                    }
                    Err(e) => {
                        return (
                            Self::default(),
                            Some(format!("theme.json invalid: {e}; using defaults")),
                        );
                    }
                },
                Err(e) => {
                    return (
                        Self::default(),
                        Some(format!("Could not read theme file: {e}")),
                    );
                }
            }
        }

        (Self::default(), None)
    }

    fn default() -> Self {
        Self {
            border: parse_hex_color("#cba6f7"),
            title: parse_hex_color("#f5c2e7"),
            dir: parse_hex_color("#89b4fa"),
            playing: parse_hex_color("#a6e3a1"),
            text: parse_hex_color("#cdd6f4"),
            highlight_bg: parse_hex_color("#313244"),
            highlight_fg: parse_hex_color("#cba6f7"),
            volume: parse_hex_color("#fab387"),
            yes: parse_hex_color("#a6e3a1"),
            no: parse_hex_color("#f38ba8"),
            selector: parse_hex_color("#f9e2af"),
            paused: parse_hex_color("#f9e2af"),
            search: parse_hex_color("#74c7ec"),
        }
    }
}

// ---------------------------------------------------------------------------
// Artwork Helper
// ---------------------------------------------------------------------------

fn extract_album_art(path: &Path) -> Option<image::DynamicImage> {
    let tagged_file = Probe::open(path).ok()?.read().ok()?;
    let tag = tagged_file.primary_tag().or_else(|| tagged_file.first_tag())?;
    let picture = tag.pictures().first()?;
    image::load_from_memory(picture.data()).ok()
}

// ---------------------------------------------------------------------------
// Decoupled Threaded Mpv IPC Controller
// ---------------------------------------------------------------------------

enum MpvEvent {
    PropertyChange { name: String, data: Value },
}

struct MpvState {
    time_pos: f64,
    duration: f64,
    volume: f64,
    paused: bool,
    idle: bool,
}

struct Mpv {
    child: Child,
    writer: UnixStream,
    event_rx: Receiver<MpvEvent>,
    socket_path: PathBuf,
    state: MpvState,
}

impl Mpv {
    fn spawn() -> anyhow::Result<Self> {
        let socket_path =
            env::temp_dir().join(format!("tuiplay-mpv-{}.sock", std::process::id()));
        let _ = fs::remove_file(&socket_path);

        let mut cmd = Command::new("mpv");
        cmd.arg("--idle=yes")
            .arg("--no-video")
            .arg("--no-terminal")
            .arg("--no-config")
            .arg("--loop-file=no")
            .arg("--loop-playlist=no")
            .arg("--really-quiet")
            .arg(format!("--input-ipc-server={}", socket_path.display()));

        let candidate_mpris_paths = [
            PathBuf::from("/usr/lib/mpv/mpris.so"),
            PathBuf::from("/usr/lib64/mpv/mpris.so"),
            PathBuf::from("/usr/local/lib/mpv/mpris.so"),
            get_config_dir().join("scripts/mpris.so"),
        ];

        if let Some(mpris_plugin) = candidate_mpris_paths.iter().find(|p| p.exists()) {
            cmd.arg(format!("--script={}", mpris_plugin.display()));
        }

        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start mpv -- is it installed?")?;

        let mut connected = None;
        for _ in 0..50 {
            if let Ok(s) = UnixStream::connect(&socket_path) {
                connected = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let writer = connected.context("mpv did not open its IPC socket in time")?;
        let reader_stream = writer.try_clone()?;

        let (event_tx, event_rx) = channel();

        thread::spawn(move || {
            let mut reader = BufReader::new(reader_stream);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
                        if v.get("event").and_then(|e| e.as_str()) == Some("property-change") {
                            if let Some(name) = v.get("name").and_then(|n| n.as_str()).map(String::from) {
                                if let Some(data) = v.get("data").cloned() {
                                    let _ = event_tx.send(MpvEvent::PropertyChange { name, data });
                                }
                            }
                        }
                    }
                }
                line.clear();
            }
        });

        let mut mpv = Self {
            child,
            writer,
            event_rx,
            socket_path,
            state: MpvState {
                time_pos: 0.0,
                duration: 0.0,
                volume: 100.0,
                paused: false,
                idle: true,
            },
        };

        mpv.observe_properties();
        Ok(mpv)
    }

    fn observe_properties(&mut self) {
        let cmds = [
            json!({ "command": ["observe_property", 1, "time-pos"] }),
            json!({ "command": ["observe_property", 2, "duration"] }),
            json!({ "command": ["observe_property", 3, "volume"] }),
            json!({ "command": ["observe_property", 4, "pause"] }),
            json!({ "command": ["observe_property", 5, "idle-active"] }),
        ];
        for cmd in cmds {
            let _ = self.send_cmd(cmd);
        }
    }

    fn send_cmd(&mut self, payload: Value) -> anyhow::Result<()> {
        let mut msg = payload.to_string();
        msg.push('\n');
        self.writer.write_all(msg.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    fn poll_events(&mut self) {
        while let Ok(evt) = self.event_rx.try_recv() {
            match evt {
                MpvEvent::PropertyChange { name, data } => match name.as_str() {
                    "time-pos" => {
                        if let Some(val) = data.as_f64() {
                            self.state.time_pos = val;
                        }
                    }
                    "duration" => {
                        if let Some(val) = data.as_f64() {
                            self.state.duration = val;
                        }
                    }
                    "volume" => {
                        if let Some(val) = data.as_f64() {
                            self.state.volume = val;
                        }
                    }
                    "pause" => {
                        if let Some(val) = data.as_bool() {
                            self.state.paused = val;
                        }
                    }
                    "idle-active" => {
                        if let Some(val) = data.as_bool() {
                            self.state.idle = val;
                        }
                    }
                    _ => {}
                },
            }
        }
    }

    fn play(&mut self, path: &Path) -> anyhow::Result<()> {
        let path_str = path.to_string_lossy().to_string();
        self.send_cmd(json!({ "command": ["loadfile", path_str, "replace"] }))?;
        self.state.idle = false;
        Ok(())
    }

    fn toggle_pause(&mut self) {
        let _ = self.send_cmd(json!({ "command": ["cycle", "pause"] }));
    }

    fn stop(&mut self) {
        let _ = self.send_cmd(json!({ "command": ["stop"] }));
        self.state.idle = true;
    }

    fn set_volume(&mut self, vol: f64) {
        let vol = vol.clamp(0.0, 100.0);
        self.state.volume = vol;
        let _ = self.send_cmd(json!({ "command": ["set_property", "volume", vol] }));
    }

    fn seek(&mut self, seconds: f64) {
        let _ = self.send_cmd(json!({ "command": ["seek", seconds, "relative"] }));
    }

    fn is_dead(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        let _ = self.send_cmd(json!({ "command": ["quit"] }));
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.socket_path);
    }
}

// ---------------------------------------------------------------------------
// App State
// ---------------------------------------------------------------------------

struct App {
    current_dir: PathBuf,
    entries: Vec<Entry>,
    filtered_indices: Vec<usize>,
    selected_file: usize,

    queue: Vec<PathBuf>,
    selected_queue: usize,

    mode: ViewMode,
    loop_mode: LoopMode,
    is_shuffled: bool,

    mpv: Mpv,
    current: Option<PathBuf>,
    status: String,
    should_quit: bool,
    show_quit_popup: bool,
    show_help_popup: bool,

    theme: Theme,

    picker: Picker,
    current_artwork: Option<StatefulProtocol>,
    art_cache: HashMap<PathBuf, Option<image::DynamicImage>>,

    controls: Option<MediaControls>,
    action_rx: Receiver<AppAction>,

    search_query: String,
    is_searching: bool,
    pending_seek: f64,
}

impl App {
    fn new(start_dir: PathBuf) -> anyhow::Result<Self> {
        let mpv = Mpv::spawn()?;
        let (theme, theme_status) = Theme::load_or_default();
        let (tx, rx) = channel();

        let controls = Self::init_media_controls(tx);
        let picker = Picker::from_query_stdio()
            .unwrap_or_else(|_| Picker::from_fontsize((8, 12)));

        let mut app = Self {
            current_dir: start_dir,
            entries: Vec::new(),
            filtered_indices: Vec::new(),
            selected_file: 0,
            queue: Vec::new(),
            selected_queue: 0,
            mode: ViewMode::Files,
            loop_mode: LoopMode::Off,
            is_shuffled: false,
            mpv,
            current: None,
            status: theme_status.unwrap_or_default(),
            should_quit: false,
            show_quit_popup: false,
            show_help_popup: false,
            theme,
            picker,
            current_artwork: None,
            art_cache: HashMap::new(),
            controls,
            action_rx: rx,
            search_query: String::new(),
            is_searching: false,
            pending_seek: 0.0,
        };

        app.refresh_entries();
        app.load_state();
        Ok(app)
    }

    fn reload_theme(&mut self) {
        let (theme, status) = Theme::load_or_default();
        self.theme = theme;
        self.status = status.unwrap_or_else(|| "Theme reloaded successfully".to_string());
    }

    fn init_media_controls(tx: Sender<AppAction>) -> Option<MediaControls> {
        let config = PlatformConfig {
            dbus_name: "tuiplay",
            display_name: "tuiplay",
            hwnd: None,
        };

        let mut controls = MediaControls::new(config).ok()?;

        controls
            .attach(move |event| match event {
                MediaControlEvent::Play
                | MediaControlEvent::Pause
                | MediaControlEvent::Toggle => {
                    let _ = tx.send(AppAction::TogglePause);
                }
                MediaControlEvent::Next => {
                    let _ = tx.send(AppAction::Next);
                }
                MediaControlEvent::Previous => {
                    let _ = tx.send(AppAction::Prev);
                }
                MediaControlEvent::Stop => {
                    let _ = tx.send(AppAction::Stop);
                }
                _ => {}
            })
            .ok()?;

        Some(controls)
    }

    fn save_state(&self) {
        let dir = get_config_dir();
        if fs::create_dir_all(&dir).is_ok() {
            let state_file = dir.join("state.json");
            let data = json!({
                "current_dir": self.current_dir,
                "queue": self.queue,
                "volume": self.mpv.state.volume,
                "mode": match self.mode {
                    ViewMode::Files => 1,
                    ViewMode::Queue => 2,
                    ViewMode::Zen => 3,
                },
                "loop_mode": match self.loop_mode {
                    LoopMode::Off => 0,
                    LoopMode::Track => 1,
                    LoopMode::Queue => 2,
                },
                "is_shuffled": self.is_shuffled,
            });
            let _ = fs::write(state_file, data.to_string());
        }
    }

    fn load_state(&mut self) {
        let state_file = get_config_dir().join("state.json");

        if state_file.exists() {
            if let Ok(content) = fs::read_to_string(state_file) {
                if let Ok(v) = serde_json::from_str::<Value>(&content) {
                    if let Some(dir_str) = v.get("current_dir").and_then(|s| s.as_str()) {
                        let dir = PathBuf::from(dir_str);
                        if dir.is_dir() {
                            self.current_dir = dir;
                            self.refresh_entries();
                        }
                    }
                    if let Some(q) = v.get("queue").and_then(|arr| arr.as_array()) {
                        self.queue = q
                            .iter()
                            .filter_map(|val| val.as_str().map(PathBuf::from))
                            .collect();
                    }
                    if let Some(vol) = v.get("volume").and_then(|val| val.as_f64()) {
                        self.mpv.set_volume(vol);
                    }
                    if let Some(m) = v.get("mode").and_then(|val| val.as_u64()) {
                        self.mode = match m {
                            2 => ViewMode::Queue,
                            3 => ViewMode::Zen,
                            _ => ViewMode::Files,
                        };
                    }
                    if let Some(l) = v.get("loop_mode").and_then(|val| val.as_u64()) {
                        self.loop_mode = match l {
                            1 => LoopMode::Track,
                            2 => LoopMode::Queue,
                            _ => LoopMode::Off,
                        };
                    }
                    if let Some(s) = v.get("is_shuffled").and_then(|val| val.as_bool()) {
                        self.is_shuffled = s;
                    }
                }
            }
        }
        self.clamp_selection();
    }

    fn check_mpv_health(&mut self) {
        if self.mpv.is_dead() {
            self.status = "MPV died! Respawning process...".to_string();
            if let Ok(mut new_mpv) = Mpv::spawn() {
                let old_vol = self.mpv.state.volume;
                new_mpv.set_volume(old_vol);
                self.mpv = new_mpv;
                if let Some(ref current) = self.current.clone() {
                    let _ = self.mpv.play(current);
                }
                self.status = "MPV process respawned.".to_string();
            }
        }
    }

    fn process_pending_seek(&mut self) {
        if self.pending_seek != 0.0 {
            if self.current.is_some() {
                self.mpv.seek(self.pending_seek);
            }
            self.pending_seek = 0.0;
        }
    }

    fn sync_mpris(&mut self) {
        if let Some(ref mut controls) = self.controls {
            let is_paused = self.mpv.state.paused;
            let pos_secs = self.mpv.state.time_pos;
            let progress = Some(MediaPosition(Duration::from_secs_f64(pos_secs)));

            if self.current.is_none() || self.mpv.state.idle {
                let _ = controls.set_playback(MediaPlayback::Stopped);
            } else if is_paused {
                let _ = controls.set_playback(MediaPlayback::Paused { progress });
            } else {
                let _ = controls.set_playback(MediaPlayback::Playing { progress });
            }
        }
    }

    fn update_search_filter(&mut self) {
        if self.search_query.is_empty() {
            self.filtered_indices = (0..self.entries.len()).collect();
        } else {
            let q = self.search_query.to_lowercase();
            self.filtered_indices = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.name().to_lowercase().contains(&q))
                .map(|(idx, _)| idx)
                .collect();
        }
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        if self.filtered_indices.is_empty() {
            self.selected_file = 0;
        } else if self.selected_file >= self.filtered_indices.len() {
            self.selected_file = self.filtered_indices.len() - 1;
        }

        if self.queue.is_empty() {
            self.selected_queue = 0;
        } else if self.selected_queue >= self.queue.len() {
            self.selected_queue = self.queue.len() - 1;
        }
    }

    fn is_audio(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|ext| AUDIO_EXTS.contains(&ext.to_lowercase().as_str()))
            .unwrap_or(false)
    }

    fn is_hidden(path: &Path) -> bool {
        path.file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.starts_with('.'))
            .unwrap_or(false)
    }

    fn refresh_entries(&mut self) {
        let mut dirs = Vec::new();
        let mut files = Vec::new();

        if let Ok(read) = fs::read_dir(&self.current_dir) {
            for e in read.flatten() {
                let path = e.path();
                if Self::is_hidden(&path) {
                    continue;
                }
                if path.is_dir() {
                    dirs.push(Entry { path, is_dir: true });
                } else if Self::is_audio(&path) {
                    files.push(Entry { path, is_dir: false });
                }
            }
        }

        dirs.sort_by_key(|e| e.name().to_lowercase());
        files.sort_by_key(|e| e.name().to_lowercase());

        self.entries = dirs;
        self.entries.extend(files);
        self.update_search_filter();
    }

    fn move_up(&mut self) {
        match self.mode {
            ViewMode::Files => {
                if self.selected_file > 0 {
                    self.selected_file -= 1;
                }
            }
            ViewMode::Queue => {
                if self.selected_queue > 0 {
                    self.selected_queue -= 1;
                }
            }
            ViewMode::Zen => {}
        }
    }

    fn move_down(&mut self) {
        match self.mode {
            ViewMode::Files => {
                if !self.filtered_indices.is_empty()
                    && self.selected_file + 1 < self.filtered_indices.len()
                {
                    self.selected_file += 1;
                }
            }
            ViewMode::Queue => {
                if !self.queue.is_empty() && self.selected_queue + 1 < self.queue.len() {
                    self.selected_queue += 1;
                }
            }
            ViewMode::Zen => {}
        }
    }

    fn queue_move_up(&mut self) {
        if self.mode == ViewMode::Queue && self.selected_queue > 0 {
            self.queue.swap(self.selected_queue, self.selected_queue - 1);
            self.selected_queue -= 1;
        }
    }

    fn queue_move_down(&mut self) {
        if self.mode == ViewMode::Queue
            && !self.queue.is_empty()
            && self.selected_queue + 1 < self.queue.len()
        {
            self.queue.swap(self.selected_queue, self.selected_queue + 1);
            self.selected_queue += 1;
        }
    }

    fn add_selected_to_queue(&mut self) {
        if self.mode == ViewMode::Files {
            if let Some(&entry_idx) = self.filtered_indices.get(self.selected_file) {
                if let Some(entry) = self.entries.get(entry_idx) {
                    if !entry.is_dir {
                        self.queue.push(entry.path.clone());
                        self.status = format!("Added to queue: {}", entry.name());
                        self.move_down();
                    }
                }
            }
        }
    }

    fn remove_from_queue(&mut self) {
        if self.mode == ViewMode::Queue && !self.queue.is_empty() {
            let removed = self.queue.remove(self.selected_queue);
            self.clamp_selection();
            if let Some(name) = removed.file_name() {
                self.status = format!("Removed from queue: {}", name.to_string_lossy());
            }
        }
    }

    fn enter_selected(&mut self) {
        match self.mode {
            ViewMode::Files => {
                if let Some(&entry_idx) = self.filtered_indices.get(self.selected_file) {
                    if let Some(entry) = self.entries.get(entry_idx).cloned() {
                        if entry.is_dir {
                            self.current_dir = entry.path;
                            self.search_query.clear();
                            self.is_searching = false;
                            self.refresh_entries();
                        } else {
                            self.play(&entry.path);
                        }
                    }
                }
            }
            ViewMode::Queue => {
                if let Some(path) = self.queue.get(self.selected_queue).cloned() {
                    self.play(&path);
                }
            }
            ViewMode::Zen => {}
        }
    }

    fn go_parent(&mut self) {
        if self.mode == ViewMode::Files {
            if let Some(parent) = self.current_dir.parent().map(|p| p.to_path_buf()) {
                let old_dir = self.current_dir.clone();
                self.current_dir = parent;
                self.search_query.clear();
                self.is_searching = false;
                self.refresh_entries();

                if let Some(pos) = self.filtered_indices.iter().position(|&idx| self.entries[idx].path == old_dir) {
                    self.selected_file = pos;
                }
            }
        }
    }

    fn update_media_metadata(&mut self, path: &Path) {
        if let Some(ref mut controls) = self.controls {
            let title = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Unknown Track".to_string());

            let album = path
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().to_string());

            let _ = controls.set_metadata(MediaMetadata {
                title: Some(&title),
                album: album.as_deref(),
                ..Default::default()
            });

            let _ = controls.set_playback(MediaPlayback::Playing {
                progress: Some(MediaPosition(Duration::from_secs(0))),
            });
        }
    }

    fn play(&mut self, path: &Path) {
        match self.mpv.play(path) {
            Ok(()) => {
                self.current = Some(path.to_path_buf());
                self.status.clear();
                self.update_media_metadata(path);

                let img_opt = self
                    .art_cache
                    .entry(path.to_path_buf())
                    .or_insert_with(|| extract_album_art(path))
                    .clone();

                if let Some(img) = img_opt {
                    self.current_artwork = Some(self.picker.new_resize_protocol(img));
                } else {
                    self.current_artwork = None;
                }
            }
            Err(e) => {
                self.status = format!("Error: {e}");
            }
        }
    }

    fn play_next(&mut self) {
        if self.loop_mode == LoopMode::Track {
            if let Some(current) = self.current.clone() {
                self.play(&current);
                return;
            }
        }

        if !self.queue.is_empty() {
            let next_idx = if self.is_shuffled {
                pseudo_random_index(self.queue.len())
            } else {
                0
            };

            let next_track = self.queue.remove(next_idx);
            if self.loop_mode == LoopMode::Queue {
                self.queue.push(next_track.clone());
            }

            self.clamp_selection();
            self.play(&next_track);
            return;
        }

        if let Some(current) = self.current.clone() {
            if let Some(pos) = self.entries.iter().position(|e| e.path == current) {
                if self.is_shuffled {
                    let audio_files: Vec<_> = self.entries.iter().filter(|e| !e.is_dir).collect();
                    if !audio_files.is_empty() {
                        let pick = audio_files[pseudo_random_index(audio_files.len())];
                        let path = pick.path.clone();
                        if let Some(new_pos) = self.filtered_indices.iter().position(|&idx| self.entries[idx].path == path) {
                            self.selected_file = new_pos;
                        }
                        self.play(&path);
                        return;
                    }
                } else if let Some(next) = self.entries[pos + 1..].iter().find(|e| !e.is_dir) {
                    let path = next.path.clone();
                    if let Some(new_pos) = self.filtered_indices.iter().position(|&idx| self.entries[idx].path == path) {
                        self.selected_file = new_pos;
                    }
                    self.play(&path);
                    return;
                } else if self.loop_mode == LoopMode::Queue {
                    if let Some(first) = self.entries.iter().find(|e| !e.is_dir) {
                        let path = first.path.clone();
                        if let Some(new_pos) = self.filtered_indices.iter().position(|&idx| self.entries[idx].path == path) {
                            self.selected_file = new_pos;
                        }
                        self.play(&path);
                        return;
                    }
                }
            }
        }

        self.current = None;
        self.current_artwork = None;
        self.status = "Playback finished".to_string();
        self.sync_mpris();
    }

    fn play_prev(&mut self) {
        let pos = self.mpv.state.time_pos;
        if pos > 3.0 {
            self.mpv.seek(-pos);
            return;
        }

        if let Some(current) = self.current.clone() {
            if let Some(pos) = self.entries.iter().position(|e| e.path == current) {
                if pos > 0 {
                    if let Some(prev) = self.entries[..pos].iter().rev().find(|e| !e.is_dir) {
                        let path = prev.path.clone();
                        if let Some(new_pos) = self.filtered_indices.iter().position(|&idx| self.entries[idx].path == path) {
                            self.selected_file = new_pos;
                        }
                        self.play(&path);
                        return;
                    }
                }
            }
        }
        self.status = "Top of list".to_string();
    }

    fn toggle_pause(&mut self) {
        self.mpv.toggle_pause();
        self.sync_mpris();
    }

    fn stop(&mut self) {
        self.mpv.stop();
        self.current = None;
        self.current_artwork = None;
        self.sync_mpris();
    }

    fn volume_up(&mut self) {
        let v = self.mpv.state.volume;
        self.mpv.set_volume(v + 10.0);
    }

    fn volume_down(&mut self) {
        let v = self.mpv.state.volume;
        self.mpv.set_volume(v - 10.0);
    }

    fn seek_forward(&mut self) {
        if self.current.is_some() {
            self.pending_seek += 5.0;
        }
    }

    fn seek_backward(&mut self) {
        if self.current.is_some() {
            self.pending_seek -= 5.0;
        }
    }

    fn process_external_actions(&mut self) {
        while let Ok(action) = self.action_rx.try_recv() {
            match action {
                AppAction::TogglePause => self.toggle_pause(),
                AppAction::Next => self.play_next(),
                AppAction::Prev => self.play_prev(),
                AppAction::Stop => self.stop(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_time(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let minutes = total / 60;
    let seconds = total % 60;
    format!("{minutes:02}:{seconds:02}")
}

fn make_progress_bar(percent: f64, width: usize) -> String {
    let width = width.saturating_sub(2);
    let filled = ((percent / 100.0) * width as f64).round() as usize;
    let filled = filled.clamp(0, width);
    let empty = width.saturating_sub(filled);

    format!("[{}{}]", "━".repeat(filled), "─".repeat(empty))
}

fn centered_rect(width: u16, height: u16, r: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(height),
            Constraint::Fill(1),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(width),
            Constraint::Fill(1),
        ])
        .split(vertical[1])[1]
}

// ---------------------------------------------------------------------------
// UI Drawing
// ---------------------------------------------------------------------------

fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(4),
            Constraint::Length(3),
        ])
        .split(f.area());

    draw_header(f, app, chunks[0]);

    match app.mode {
        ViewMode::Files => draw_file_list(f, app, chunks[1]),
        ViewMode::Queue => draw_queue_list(f, app, chunks[1]),
        ViewMode::Zen => draw_zen_mode(f, app, chunks[1]),
    }

    if app.mode != ViewMode::Zen {
        draw_player_status(f, app, chunks[2]);
    } else {
        f.render_widget(Block::default(), chunks[2]);
    }

    draw_controls_bar(f, app, chunks[3]);

    if app.show_quit_popup {
        draw_quit_popup(f, app);
    }

    if app.show_help_popup {
        draw_help_popup(f, app);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let mode_str = match app.mode {
        ViewMode::Files => "1: Files",
        ViewMode::Queue => "2: Queue",
        ViewMode::Zen => "3: Zen",
    };

    let title_text = Line::from(vec![
        Span::styled(" View: ", Style::default().fg(app.theme.title)),
        Span::styled(
            format!("[{mode_str}]"),
            Style::default()
                .fg(app.theme.playing)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" | "),
        Span::styled(app.current_dir.display().to_string(), Style::default().fg(app.theme.text)),
    ]);

    let header = Paragraph::new(title_text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.border))
            .title(Span::styled(" tuiplay ", Style::default().fg(app.theme.title))),
    );
    f.render_widget(header, area);
}

fn draw_file_list(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .filtered_indices
        .iter()
        .map(|&idx| {
            let e = &app.entries[idx];
            let is_current = app.current.as_ref() == Some(&e.path);
            let label = if e.is_dir {
                format!("  📁 {}/", e.name())
            } else if is_current {
                format!("  ▶  {}", e.name())
            } else {
                format!("     {}", e.name())
            };

            let style = if is_current {
                Style::default()
                    .fg(app.theme.playing)
                    .add_modifier(Modifier::BOLD)
            } else if e.is_dir {
                Style::default().fg(app.theme.dir)
            } else {
                Style::default().fg(app.theme.text)
            };

            ListItem::new(Line::from(Span::styled(label, style)))
        })
        .collect();

    let title = if app.is_searching {
        format!(" Directory Files (Filtering: {}) [Esc: Clear] ", app.search_query)
    } else {
        format!(
            " Directory Files ({}/{}) (Press '/' to search | 'a' to queue) ",
            app.filtered_indices.len(),
            app.entries.len()
        )
    };

    let border_color = if app.is_searching { app.theme.search } else { app.theme.border };
    let title_color = if app.is_searching { app.theme.search } else { app.theme.title };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(Span::styled(title, Style::default().fg(title_color))),
        )
        .highlight_style(
            Style::default()
                .bg(app.theme.highlight_bg)
                .fg(app.theme.highlight_fg)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    let mut state = ListState::default();
    if !app.filtered_indices.is_empty() {
        state.select(Some(app.selected_file));
    }

    f.render_stateful_widget(list, area, &mut state);

    if !app.filtered_indices.is_empty() {
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"))
            .track_symbol(Some("│"))
            .thumb_symbol("█")
            .style(Style::default().fg(app.theme.border));

        let mut scrollbar_state = ScrollbarState::new(app.filtered_indices.len().saturating_sub(1))
            .position(app.selected_file);

        f.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }
}

fn draw_queue_list(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .queue
        .iter()
        .enumerate()
        .map(|(idx, path)| {
            let is_current = app.current.as_ref() == Some(path);
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string_lossy().to_string());

            let label = if is_current {
                format!(" {:2}. ▶ {}", idx + 1, name)
            } else {
                format!(" {:2}.   {}", idx + 1, name)
            };

            let style = if is_current {
                Style::default()
                    .fg(app.theme.playing)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(app.theme.text)
            };

            ListItem::new(Line::from(Span::styled(label, style)))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.border))
                .title(Span::styled(
                    format!(" Queue ({}) [Shift+J/K: move | d: remove] ", app.queue.len()),
                    Style::default().fg(app.theme.title),
                )),
        )
        .highlight_style(
            Style::default()
                .bg(app.theme.highlight_bg)
                .fg(app.theme.selector)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    let mut state = ListState::default();
    if !app.queue.is_empty() {
        state.select(Some(app.selected_queue));
    }

    f.render_stateful_widget(list, area, &mut state);

    if !app.queue.is_empty() {
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"))
            .track_symbol(Some("│"))
            .thumb_symbol("█")
            .style(Style::default().fg(app.theme.border));

        let mut scrollbar_state =
            ScrollbarState::new(app.queue.len().saturating_sub(1)).position(app.selected_queue);

        f.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }
}

fn draw_zen_mode(f: &mut Frame, app: &mut App, area: Rect) {
    let zen_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    let art_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border))
        .title(Span::styled(" Artwork ", Style::default().fg(app.theme.title)));

    let inner_art_area = art_block.inner(zen_layout[0]);
    f.render_widget(art_block, zen_layout[0]);

    if let Some(ref mut protocol) = app.current_artwork {
        let image_widget = StatefulImage::new(None);
        f.render_stateful_widget(image_widget, inner_art_area, protocol);
    } else {
        let art_ascii = vec![
            "  ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄  ",
            " █                     █ ",
            " █     ▄▄▄███▄▄▄       █ ",
            " █   ▄███████████▄     █ ",
            " █  █████▀▀ ▀▀█████    █ ",
            " █  ████   ⊙   ████    █ ",
            " █  █████▄▄ ▄▄█████    █ ",
            " █   ▀███████████▀     █ ",
            " █     ▀▀▀███▀▀▀       █ ",
            " █                     █ ",
            "  ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀  ",
        ];

        let art_lines: Vec<Line> = art_ascii
            .into_iter()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(app.theme.title))))
            .collect();

        let album_art_widget = Paragraph::new(art_lines).alignment(Alignment::Center);
        f.render_widget(album_art_widget, inner_art_area);
    }

    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(100)])
        .split(zen_layout[1]);

    let pos = app.mpv.state.time_pos;
    let duration = app.mpv.state.duration;
    let vol_pct = app.mpv.state.volume.round() as i32;

    let track_name = app
        .current
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "No Track Playing".to_string());

    let album_or_folder = app
        .current
        .as_ref()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown Album".to_string());

    let play_state = if app.current.is_none() {
        "⏹ STOPPED"
    } else if app.mpv.state.paused {
        "⏸ PAUSED"
    } else {
        "▶ PLAYING"
    };

    let pos_str = format_time(pos);
    let dur_str = format_time(duration);
    let pct = if duration > 0.0 { (pos / duration) * 100.0 } else { 0.0 };

    let bar_width = (right_chunks[0].width as usize).saturating_sub(18).max(10);
    let progress_bar = make_progress_bar(pct, bar_width);

    let meta_lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(" Track:  ", Style::default().fg(app.theme.title)),
            Span::styled(
                track_name,
                Style::default()
                    .fg(app.theme.playing)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(" Album:  ", Style::default().fg(app.theme.title)),
            Span::styled(album_or_folder, Style::default().fg(app.theme.text)),
        ]),
        Line::from(vec![
            Span::styled(" Status: ", Style::default().fg(app.theme.title)),
            Span::styled(
                play_state,
                Style::default()
                    .fg(app.theme.status_color(app.current.is_some(), app.mpv.state.paused))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("   [Vol: {vol_pct}%]"), Style::default().fg(app.theme.volume)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(" Timeline:", Style::default().fg(app.theme.title)),
        ]),
        Line::from(vec![
            Span::styled(format!(" {pos_str} "), Style::default().fg(app.theme.text)),
            Span::styled(progress_bar, Style::default().fg(app.theme.playing)),
            Span::styled(format!(" {dur_str}"), Style::default().fg(app.theme.text)),
        ]),
    ];

    let metadata_widget = Paragraph::new(meta_lines)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.border))
                .title(Span::styled(" Now Playing (Zen Mode) ", Style::default().fg(app.theme.title))),
        );

    f.render_widget(metadata_widget, right_chunks[0]);
}

fn draw_player_status(f: &mut Frame, app: &mut App, area: Rect) {
    if app.current.is_none() {
        let empty_msg = Paragraph::new(" No track selected").block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.border))
                .title(Span::styled(" Now Playing ", Style::default().fg(app.theme.title))),
        );
        f.render_widget(empty_msg, area);
        return;
    }

    let pos = app.mpv.state.time_pos;
    let duration = app.mpv.state.duration;

    let pos_str = format_time(pos);
    let dur_str = format_time(duration);

    let pct = if duration > 0.0 {
        (pos / duration) * 100.0
    } else {
        0.0
    };

    let track_name = app
        .current
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown".to_string());

    let vol_pct = app.mpv.state.volume.round() as i32;

    let line1 = Line::from(vec![
        Span::styled(
            format!(" ♪ {track_name}"),
            Style::default()
                .fg(app.theme.status_color(app.current.is_some(), app.mpv.state.paused))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("   [Vol: {vol_pct}%]"), Style::default().fg(app.theme.volume)),
    ]);

    let bar_width = (area.width as usize).saturating_sub(20).max(10);
    let bar = make_progress_bar(pct, bar_width);
    let line2 = Line::from(format!(" {pos_str} {bar} {dur_str}"));

    let paragraph = Paragraph::new(vec![line1, line2]).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.border))
            .title(Span::styled(" Now Playing ", Style::default().fg(app.theme.title))),
    );

    f.render_widget(paragraph, area);
}

fn draw_controls_bar(f: &mut Frame, app: &mut App, area: Rect) {
    let play_state = if app.current.is_none() {
        "STOPPED"
    } else if app.mpv.state.paused {
        "PAUSED"
    } else {
        "PLAYING"
    };

    let status_msg = if app.status.is_empty() {
        String::new()
    } else {
        format!(" | {}", app.status)
    };

    let dim_style = Style::default().fg(app.theme.text);
    let key_style = Style::default().fg(app.theme.title).add_modifier(Modifier::BOLD);

    let controls_text = Line::from(vec![
        Span::styled(
            format!("[{play_state}]{status_msg}  "),
            Style::default().fg(app.theme.status_color(app.current.is_some(), app.mpv.state.paused)),
        ),
        Span::styled("[1/2/3]", key_style),
        Span::styled(" Views  |  ", dim_style),
        Span::styled("[?]", key_style),
        Span::styled(" Keybinds  |  ", dim_style),
        Span::styled("[/]", key_style),
        Span::styled(" Filter  |  ", dim_style),
        Span::styled("[space]", key_style),
        Span::styled(" Toggle  |  ", dim_style),
        Span::styled("[q]", key_style),
        Span::styled(" Quit", dim_style),
    ]);

    let bar = Paragraph::new(controls_text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.border))
            .title(Span::styled(" Controls ", Style::default().fg(app.theme.title))),
    );
    f.render_widget(bar, area);
}

fn draw_help_popup(f: &mut Frame, app: &App) {
    let popup_area = centered_rect(60, 20, f.area());
    f.render_widget(Clear, popup_area);

    let popup_block = Block::default()
        .title(Span::styled(" Keybindings Help (Press ? or Esc to close) ", Style::default().fg(app.theme.title)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border));

    let key_style = Style::default().fg(app.theme.selector).add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(app.theme.text);
    let section_style = Style::default().fg(app.theme.playing).add_modifier(Modifier::BOLD | Modifier::UNDERLINED);

    let content = vec![
        Line::from(Span::styled(" Navigation & Views", section_style)),
        Line::from(vec![Span::styled("  j / k or ↓ / ↑  ", key_style), Span::styled("Move selection up/down", desc_style)]),
        Line::from(vec![Span::styled("  l / Enter      ", key_style), Span::styled("Open folder / play track", desc_style)]),
        Line::from(vec![Span::styled("  h              ", key_style), Span::styled("Go to parent directory", desc_style)]),
        Line::from(vec![Span::styled("  1 / 2 / 3      ", key_style), Span::styled("Switch to Files / Queue / Zen mode", desc_style)]),
        Line::from(vec![Span::styled("  /              ", key_style), Span::styled("Filter/search current folder", desc_style)]),
        Line::from(""),
        Line::from(Span::styled(" Playback & Queue", section_style)),
        Line::from(vec![Span::styled("  Space          ", key_style), Span::styled("Toggle play / pause", desc_style)]),
        Line::from(vec![Span::styled("  n / p          ", key_style), Span::styled("Play next / previous track", desc_style)]),
        Line::from(vec![Span::styled("  s              ", key_style), Span::styled("Stop playback", desc_style)]),
        Line::from(vec![Span::styled("  + / -          ", key_style), Span::styled("Increase / decrease volume", desc_style)]),
        Line::from(vec![Span::styled("  ← / →          ", key_style), Span::styled("Seek backward / forward 5s", desc_style)]),
        Line::from(vec![Span::styled("  a              ", key_style), Span::styled("Add selected track to queue", desc_style)]),
        Line::from(vec![Span::styled("  d              ", key_style), Span::styled("Remove selected track from queue", desc_style)]),
        Line::from(vec![Span::styled("  Shift + J/K    ", key_style), Span::styled("Move item up/down inside queue", desc_style)]),
        Line::from(""),
        Line::from(Span::styled(" Settings & Misc", section_style)),
        Line::from(vec![Span::styled("  r              ", key_style), Span::styled("Cycle loop mode (Off/Track/Queue)", desc_style)]),
        Line::from(vec![Span::styled("  z              ", key_style), Span::styled("Toggle shuffle", desc_style)]),
        Line::from(vec![Span::styled("  t              ", key_style), Span::styled("Reload theme configuration", desc_style)]),
        Line::from(vec![Span::styled("  ?              ", key_style), Span::styled("Toggle this help screen", desc_style)]),
        Line::from(vec![Span::styled("  q / Esc        ", key_style), Span::styled("Quit application", desc_style)]),
    ];

    let popup_paragraph = Paragraph::new(content)
        .block(popup_block);

    f.render_widget(popup_paragraph, popup_area);
}

fn draw_quit_popup(f: &mut Frame, app: &App) {
    let popup_area = centered_rect(44, 7, f.area());
    f.render_widget(Clear, popup_area);

    let popup_block = Block::default()
        .title(Span::styled(" Quit Confirmation ", Style::default().fg(app.theme.title)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border));

    let content = vec![
        Line::from(""),
        Line::from(Span::styled(
            "Are you sure you want to quit?",
            Style::default().fg(app.theme.text).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(" [Y] Yes ", Style::default().fg(app.theme.yes).add_modifier(Modifier::BOLD)),
            Span::raw("    "),
            Span::styled(" [N] No ", Style::default().fg(app.theme.no).add_modifier(Modifier::BOLD)),
        ]),
    ];

    let popup_paragraph = Paragraph::new(content)
        .alignment(Alignment::Center)
        .block(popup_block);

    f.render_widget(popup_paragraph, popup_area);
}

// ---------------------------------------------------------------------------
// Main Loop & Event Handling
// ---------------------------------------------------------------------------

fn main() -> anyhow::Result<()> {
    let start_dir = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::var("HOME")
                .ok()
                .map(|home| PathBuf::from(home).join("Music"))
                .filter(|path| path.is_dir())
                .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        });

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(start_dir)?;
    let result = run(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> anyhow::Result<()> {
    loop {
        app.mpv.poll_events();
        app.sync_mpris();
        app.check_mpv_health();
        app.process_pending_seek();

        terminal.draw(|f| draw(f, app))?;

        if event::poll(Duration::from_millis(30))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key(app, key);
                }
            }
        }

        app.process_external_actions();

        if app.current.is_some() && app.mpv.state.idle {
            app.play_next();
        }

        if app.should_quit {
            app.save_state();
            break;
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) {
    if app.show_quit_popup {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.should_quit = true;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                app.show_quit_popup = false;
            }
            _ => {}
        }
        return;
    }

    if app.show_help_popup {
        match key.code {
            KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                app.show_help_popup = false;
            }
            _ => {}
        }
        return;
    }

    if app.is_searching {
        match key.code {
            KeyCode::Esc => {
                app.is_searching = false;
                app.search_query.clear();
                app.update_search_filter();
            }
            KeyCode::Enter => {
                app.is_searching = false;
            }
            KeyCode::Backspace => {
                app.search_query.pop();
                app.update_search_filter();
            }
            KeyCode::Char(c) => {
                app.search_query.push(c);
                app.update_search_filter();
            }
            _ => {}
        }
        return;
    }

    let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);

    match key.code {
        KeyCode::Char('q') => app.show_quit_popup = true,
        KeyCode::Esc => {
            if app.is_searching {
                app.is_searching = false;
                app.search_query.clear();
                app.update_search_filter();
            } else {
                app.show_quit_popup = true;
            }
        }

        KeyCode::Char('?') => app.show_help_popup = true,

        KeyCode::Media(media_event) => match media_event {
            MediaKeyCode::Play | MediaKeyCode::Pause | MediaKeyCode::PlayPause => {
                app.toggle_pause();
            }
            MediaKeyCode::TrackNext => app.play_next(),
            MediaKeyCode::TrackPrevious => app.play_prev(),
            MediaKeyCode::Stop => app.stop(),
            _ => {}
        },

        KeyCode::Char('1') => app.mode = ViewMode::Files,
        KeyCode::Char('2') => app.mode = ViewMode::Queue,
        KeyCode::Char('3') => app.mode = ViewMode::Zen,

        KeyCode::Char('r') => {
            app.loop_mode = app.loop_mode.next();
            app.status = format!("Loop mode: {}", app.loop_mode.label());
        }
        KeyCode::Char('z') => {
            app.is_shuffled = !app.is_shuffled;
            app.status = format!("Shuffle: {}", if app.is_shuffled { "On" } else { "Off" });
        }

        KeyCode::Char('t') | KeyCode::Char('T') => app.reload_theme(),

        KeyCode::Char('/') if app.mode == ViewMode::Files => {
            app.is_searching = true;
            app.search_query.clear();
            app.update_search_filter();
        }

        KeyCode::Char('J') => app.queue_move_down(),
        KeyCode::Char('K') => app.queue_move_up(),

        KeyCode::Char('a') => app.add_selected_to_queue(),
        KeyCode::Char('d') => app.remove_from_queue(),

        KeyCode::Char('j') | KeyCode::Down => app.move_down(),
        KeyCode::Char('k') | KeyCode::Up => app.move_up(),
        KeyCode::Char('l') | KeyCode::Enter => app.enter_selected(),
        KeyCode::Char('h') => app.go_parent(),

        KeyCode::Right => {
            if has_shift {
                app.play_next();
            } else {
                app.seek_forward();
            }
        }
        KeyCode::Left => {
            if has_shift {
                app.play_prev();
            } else {
                app.seek_backward();
            }
        }

        KeyCode::Char(' ') => app.toggle_pause(),
        KeyCode::Char('s') => app.stop(),
        KeyCode::Char('n') => app.play_next(),
        KeyCode::Char('p') => app.play_prev(),
        KeyCode::Char('+') | KeyCode::Char('=') => app.volume_up(),
        KeyCode::Char('-') | KeyCode::Char('_') => app.volume_down(),
        _ => {}
    }
}
