#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod export;
mod mix;
mod player;
mod project;
#[cfg(test)]
mod tests;
mod waveform;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use iced::keyboard;
use iced::widget::{
    Canvas, Space, button, canvas, checkbox, column, container, horizontal_space, pick_list, row,
    scrollable, slider, text, text_input,
};
use iced::{Element, Length, Subscription, Task, Theme};

use audio::{AudioData, format_time, parse_time};
use mix::TrackMix;
use player::LiveMix;
use project::{Project, Song};
use waveform::{MIN_FPP, ViewParams, WaveformProgram};

fn main() -> iced::Result {
    // An optional recording or project file on the command line is opened
    // at startup, as if picked in the Open dialog.
    let initial = std::env::args_os().nth(1).map(PathBuf::from);
    iced::application("Multi Track Split — tape song organizer", App::update, App::view)
        .subscription(App::subscription)
        .theme(|_| Theme::Dark)
        .window_size((1560.0, 960.0))
        .antialiasing(true)
        .run_with(move || {
            let task = match initial {
                Some(path) => Task::done(Message::FileChosen(Some(path))),
                None => Task::none(),
            };
            (App::default(), task)
        })
}

/// A time text field being edited. Edits are kept as a draft string until
/// Enter commits them, so half-typed times never hit the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeField {
    SongStart(u64),
    SongEnd(u64),
    TrackStart(u64, usize),
    TrackEnd(u64, usize),
}

#[derive(Debug, Clone)]
pub enum Message {
    OpenFile,
    FileChosen(Option<PathBuf>),
    Loaded(Result<Arc<AudioData>, String>),
    SaveProject,
    SaveProjectAs,
    ProjectPathChosen(Option<PathBuf>),

    ViewChanged { offset: f64, fpp: f64 },
    ZoomIn,
    ZoomOut,
    ZoomFit,
    ZoomSong,
    VZoomIn,
    VZoomOut,
    VZoomReset,
    WindowResized(f32),

    SetPlayhead(usize),
    PlayPause,
    Stop,
    /// Move the playhead by this fraction of the visible waveform span,
    /// so arrow-key steps scale with the zoom level.
    SeekVisible(f64),
    Tick,
    PlaySong(u64),

    AddSong,
    DeleteSong(u64),
    SelectSong(Option<u64>),
    SongTitle(u64, String),
    SongStartToPlayhead,
    SongEndToPlayhead,
    MoveSongStart(u64, usize),
    MoveSongEnd(u64, usize),

    TrackActive(u64, usize, bool),
    ToggleTrackActive(u64, usize),
    TrackStart(u64, usize, Option<usize>),
    TrackEnd(u64, usize, Option<usize>),
    TrackStartToPlayhead(u64, usize),
    TrackEndToPlayhead(u64, usize),
    TrackVolume(u64, usize, f32),
    TrackPan(u64, usize, f32),
    TrackMute(u64, usize, bool),
    TrackSolo(u64, usize, bool),
    ResetTrackMix(u64, usize),
    TrackName(usize, String),

    TimeDraft(TimeField, String),
    TimeCommit(TimeField),

    ExportMix,
    ExportMixPathChosen(Option<PathBuf>),
    ExportMulti,
    ExportMultiPathChosen(Option<PathBuf>),
    ExportAll,
    ExportAllDirChosen(Option<PathBuf>),
    ExportDone(Result<String, String>),

    DeviceSelected(String),
}

const DEFAULT_DEVICE: &str = "System default";
/// Length of a freshly added song before its end is adjusted.
const NEW_SONG_SECS: f64 = 4.0 * 60.0;
/// Shortest song / track range the UI will accept.
const MIN_RANGE_SECS: f64 = 0.5;

struct App {
    audio: Option<Arc<AudioData>>,
    loading: bool,
    exporting: bool,

    project_path: Option<PathBuf>,
    /// Project loaded from disk, waiting for its recording to finish decoding.
    pending_project: Option<Project>,
    dirty: bool,

    track_names: Vec<String>,
    songs: Vec<Song>,
    next_id: u64,
    selected: Option<u64>,
    draft: Option<(TimeField, String)>,

    view: ViewParams,
    /// Vertical (amplitude) zoom factor, >= 1.
    v_zoom: f32,
    window_width: f32,
    wf_cache: canvas::Cache,

    playhead: usize,
    playing: bool,
    play_anchor: Option<(Instant, usize)>,
    /// Frame at which the current playback stops.
    play_end: usize,
    player: player::Player,
    live_mix: LiveMix,
    devices: Vec<String>,
    selected_device: String,

    status: String,
}

impl Default for App {
    fn default() -> Self {
        Self {
            audio: None,
            loading: false,
            exporting: false,
            project_path: None,
            pending_project: None,
            dirty: false,
            track_names: Vec::new(),
            songs: Vec::new(),
            next_id: 0,
            selected: None,
            draft: None,
            view: ViewParams::default(),
            v_zoom: 1.0,
            window_width: 1560.0,
            wf_cache: canvas::Cache::new(),
            playhead: 0,
            playing: false,
            play_anchor: None,
            play_end: 0,
            player: player::Player::new(),
            live_mix: Arc::new(RwLock::new(Vec::new())),
            devices: {
                let mut d = vec![DEFAULT_DEVICE.to_string()];
                d.extend(player::list_output_devices());
                d
            },
            selected_device: DEFAULT_DEVICE.to_string(),
            status: "Open a multitrack WAV or FLAC file (or a saved project) to get started."
                .to_string(),
        }
    }
}

impl App {
    fn canvas_width(&self) -> f64 {
        (self.window_width - 24.0).max(100.0) as f64
    }

    fn canvas_height(&self) -> f32 {
        let channels = self.audio.as_ref().map_or(2, |a| a.channels()) as f32;
        (waveform::STRIP_H + channels * 52.0).clamp(220.0, 480.0)
    }

    fn sample_rate(&self) -> f64 {
        self.audio
            .as_ref()
            .map(|a| a.sample_rate as f64)
            .unwrap_or(44100.0)
    }

    fn frames(&self) -> usize {
        self.audio.as_ref().map_or(0, |a| a.frames())
    }

    fn min_range(&self) -> usize {
        (MIN_RANGE_SECS * self.sample_rate()) as usize
    }

    fn format_frame(&self, frame: usize) -> String {
        format_time(frame as f64 / self.sample_rate())
    }

    fn song(&self, id: u64) -> Option<&Song> {
        self.songs.iter().find(|s| s.id == id)
    }

    fn song_mut(&mut self, id: u64) -> Option<&mut Song> {
        self.songs.iter_mut().find(|s| s.id == id)
    }

    fn selected_song(&self) -> Option<&Song> {
        self.selected.and_then(|id| self.song(id))
    }

    fn song_number(&self, id: u64) -> usize {
        self.songs.iter().position(|s| s.id == id).map_or(0, |i| i + 1)
    }

    fn clamp_view(&self, offset: f64, fpp: f64) -> ViewParams {
        let frames = self.frames() as f64;
        let width = self.canvas_width();
        let max_fpp = (frames / width).max(MIN_FPP) * 1.1;
        let fpp = fpp.clamp(MIN_FPP, max_fpp.max(MIN_FPP));
        let max_offset = (frames - width * fpp * 0.5).max(0.0);
        ViewParams {
            offset: offset.clamp(0.0, max_offset.max(0.0)),
            fpp,
        }
    }

    fn zoom_fit(&mut self) {
        if let Some(audio) = &self.audio {
            let fpp = (audio.frames() as f64 / self.canvas_width()).max(MIN_FPP);
            self.view = ViewParams { offset: 0.0, fpp };
            self.wf_cache.clear();
        }
    }

    /// Fits the selected song into the view with a little margin either side.
    fn zoom_song(&mut self) {
        if let Some(song) = self.selected_song() {
            let span = song.frames().max(1) as f64 * 1.1;
            let fpp = (span / self.canvas_width()).max(MIN_FPP);
            let offset = song.start as f64 - span * 0.05 / 1.1;
            self.view = self.clamp_view(offset, fpp);
            self.wf_cache.clear();
        }
    }

    fn zoom_by(&mut self, factor: f64) {
        // Keep the playhead anchored if visible, otherwise the view center.
        let width = self.canvas_width();
        let center_x = {
            let px = self.view.x_of(self.playhead as f64) as f64;
            if px >= 0.0 && px <= width {
                px
            } else {
                width / 2.0
            }
        };
        let anchor = self.view.offset + center_x * self.view.fpp;
        let fpp = self.view.fpp * factor;
        let offset = anchor - center_x * fpp;
        self.view = self.clamp_view(offset, fpp);
        self.wf_cache.clear();
    }

    /// Scrolls the view so `pos` is visible, keeping the zoom level.
    fn scroll_to(&mut self, pos: usize) {
        let width = self.canvas_width();
        let x = (pos as f64 - self.view.offset) / self.view.fpp;
        if x < 0.0 || x > width {
            self.view = self.clamp_view(pos as f64 - 0.1 * width * self.view.fpp, self.view.fpp);
            self.wf_cache.clear();
        }
    }

    // ----- playback -------------------------------------------------------

    /// What to play: the selected song with its mix, or the whole tape flat.
    fn play_plan(&self) -> (usize, usize, Vec<TrackMix>) {
        match self.selected_song() {
            Some(song) => (song.start, song.end.min(self.frames()), mix::song_mixes(song)),
            None => {
                let audio = self.audio.as_ref();
                let channels = audio.map_or(0, |a| a.channels());
                (0, self.frames(), mix::flat_mixes(channels, self.frames()))
            }
        }
    }

    fn start_playback(&mut self) {
        let Some(audio) = self.audio.clone() else {
            return;
        };
        let (range_start, range_end, mixes) = self.play_plan();
        let start = if self.playhead >= range_start && self.playhead < range_end {
            self.playhead
        } else {
            range_start
        };
        if let Ok(mut m) = self.live_mix.write() {
            *m = mixes;
        }
        self.player
            .play(audio, self.live_mix.clone(), start, range_end);
        self.playhead = start;
        self.playing = true;
        self.play_anchor = Some((Instant::now(), start));
        self.play_end = range_end;
    }

    fn stop_playback(&mut self) {
        self.player.stop();
        self.playing = false;
        self.play_anchor = None;
    }

    /// Pushes the current mixer settings to the running playback.
    fn refresh_live_mix(&self) {
        if self.playing {
            let (_, _, mixes) = self.play_plan();
            if let Ok(mut m) = self.live_mix.write() {
                *m = mixes;
            }
        }
    }

    // ----- song editing ---------------------------------------------------

    /// Housekeeping after any change to songs or mixes.
    fn song_changed(&mut self) {
        self.songs.sort_by_key(|s| s.start);
        self.dirty = true;
        self.wf_cache.clear();
        self.refresh_live_mix();
    }

    fn set_song_start(&mut self, id: u64, frame: usize) {
        let min_range = self.min_range();
        if let Some(song) = self.song_mut(id) {
            song.start = frame.min(song.end.saturating_sub(min_range));
        }
        self.song_changed();
    }

    fn set_song_end(&mut self, id: u64, frame: usize) {
        let min_range = self.min_range();
        let frames = self.frames();
        if let Some(song) = self.song_mut(id) {
            song.end = frame.min(frames).max(song.start + min_range);
        }
        self.song_changed();
    }

    /// Sets a track's start override; `None` clears it. The value is kept
    /// inside the song and short of the track's end so the range never
    /// collapses.
    fn set_track_start(&mut self, id: u64, ch: usize, value: Option<usize>) {
        if let Some(song) = self.song_mut(id) {
            let (s_start, s_end) = (song.start, song.end);
            if let Some(track) = song.tracks.get_mut(ch) {
                let limit = track
                    .end
                    .map_or(s_end, |e| e.clamp(s_start, s_end))
                    .saturating_sub(1);
                track.start = value.filter(|&f| f > s_start).map(|f| f.min(limit));
            }
        }
        self.song_changed();
    }

    fn set_track_end(&mut self, id: u64, ch: usize, value: Option<usize>) {
        if let Some(song) = self.song_mut(id) {
            let (s_start, s_end) = (song.start, song.end);
            if let Some(track) = song.tracks.get_mut(ch) {
                let limit = track.start.map_or(s_start, |s| s.clamp(s_start, s_end)) + 1;
                track.end = value.filter(|&f| f < s_end).map(|f| f.max(limit));
            }
        }
        self.song_changed();
    }

    fn select_song(&mut self, id: Option<u64>) {
        if self.selected == id {
            return;
        }
        self.stop_playback();
        self.selected = id;
        self.draft = None;
        if let Some(start) = self.selected_song().map(|s| s.start) {
            self.playhead = start;
            self.scroll_to(start);
        }
        self.wf_cache.clear();
    }

    fn add_song(&mut self) {
        let frames = self.frames();
        let min_range = self.min_range();
        if frames == 0 {
            return;
        }
        let start = self.playhead.min(frames.saturating_sub(min_range));
        let end = (start + (NEW_SONG_SECS * self.sample_rate()) as usize).min(frames);
        let channels = self.audio.as_ref().map_or(0, |a| a.channels());
        let id = self.next_id;
        self.next_id += 1;
        let title = format!("Song {}", self.songs.len() + 1);
        self.songs.push(Song::new(id, title, start, end, channels));
        self.song_changed();
        self.select_song(Some(id));
        self.status = format!(
            "Added song at {}. Drag its flags in the strip, or press I / O to set start / end at the playhead.",
            self.format_frame(start)
        );
    }

    // ----- project I/O ----------------------------------------------------

    fn build_project(&self, project_path: &Path) -> Option<Project> {
        let audio = self.audio.as_ref()?;
        Some(Project {
            audio_file: project::audio_reference(project_path, &audio.path),
            sample_rate: audio.sample_rate,
            channels: audio.channels(),
            track_names: self.track_names.clone(),
            songs: self.songs.clone(),
        })
    }

    fn save_project_to(&mut self, path: PathBuf) {
        let Some(project) = self.build_project(&path) else {
            return;
        };
        match project::save(&path, &project) {
            Ok(()) => {
                self.project_path = Some(path.clone());
                self.dirty = false;
                self.status = format!("Saved project {} ({} songs).", path.display(), self.songs.len());
            }
            Err(e) => self.status = e,
        }
    }

    /// Applies a loaded project to the loaded recording.
    fn apply_project(&mut self, project: Project) {
        let channels = self.audio.as_ref().map_or(0, |a| a.channels());
        let frames = self.frames();
        let mut names = project.track_names;
        names.resize_with(channels, || String::new());
        for (i, n) in names.iter_mut().enumerate() {
            if n.is_empty() {
                *n = format!("Track {}", i + 1);
            }
        }
        self.track_names = names;
        self.songs = project.songs;
        for (i, song) in self.songs.iter_mut().enumerate() {
            song.id = i as u64;
            song.fit_channels(channels);
            song.end = song.end.min(frames);
            song.start = song.start.min(song.end);
        }
        self.songs.retain(|s| s.end > s.start);
        self.next_id = self.songs.len() as u64;
        self.selected = None;
        self.select_song(self.songs.first().map(|s| s.id));
        self.dirty = false;
        self.wf_cache.clear();
    }

    fn commit_time(&mut self, field: TimeField) {
        let Some((f, draft)) = self.draft.take() else {
            return;
        };
        if f != field {
            return;
        }
        let text = draft.trim();
        let is_override = matches!(field, TimeField::TrackStart(..) | TimeField::TrackEnd(..));
        let frame = if text.is_empty() && is_override {
            None
        } else {
            match parse_time(text) {
                Some(secs) => Some(self.audio.as_ref().map_or(0, |a| a.frame_of_secs(secs))),
                None => {
                    self.status = format!("\"{text}\" is not a time. Use mm:ss.mmm or seconds.");
                    return;
                }
            }
        };
        match (field, frame) {
            (TimeField::SongStart(id), Some(f)) => self.set_song_start(id, f),
            (TimeField::SongEnd(id), Some(f)) => self.set_song_end(id, f),
            (TimeField::TrackStart(id, ch), f) => self.set_track_start(id, ch, f),
            (TimeField::TrackEnd(id, ch), f) => self.set_track_end(id, ch, f),
            _ => {}
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::OpenFile => {
                if self.loading {
                    return Task::none();
                }
                return Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .add_filter("Recording or project (WAV/FLAC/JSON)", &["wav", "flac", "json"])
                            .add_filter("Project", &["json"])
                            .pick_file()
                            .await
                            .map(|h| h.path().to_path_buf())
                    },
                    Message::FileChosen,
                );
            }
            Message::FileChosen(Some(path)) => {
                let is_project = path
                    .extension()
                    .map(|e| e.eq_ignore_ascii_case("json"))
                    .unwrap_or(false);
                let audio_path = if is_project {
                    match project::load(&path) {
                        Ok(p) => {
                            let Some(audio_path) = project::resolve_audio_path(&path, &p.audio_file)
                            else {
                                self.status = format!(
                                    "Recording \"{}\" referenced by the project was not found next to it.",
                                    p.audio_file
                                );
                                return Task::none();
                            };
                            self.pending_project = Some(p);
                            self.project_path = Some(path);
                            audio_path
                        }
                        Err(e) => {
                            self.status = e;
                            return Task::none();
                        }
                    }
                } else {
                    // A project saved next to the recording is picked up automatically.
                    let sidecar = project::sidecar_path(&path);
                    self.pending_project = None;
                    self.project_path = None;
                    if sidecar.is_file() {
                        match project::load(&sidecar) {
                            Ok(p) => {
                                self.pending_project = Some(p);
                                self.project_path = Some(sidecar);
                            }
                            Err(e) => self.status = format!("Ignoring {}: {e}", sidecar.display()),
                        }
                    }
                    path
                };
                self.stop_playback();
                self.loading = true;
                self.status = format!("Loading {}…", audio_path.display());
                return Task::perform(
                    async move { audio::load(&audio_path).map(Arc::new) },
                    Message::Loaded,
                );
            }
            Message::FileChosen(None) => {}
            Message::Loaded(Ok(audio)) => {
                self.loading = false;
                self.stop_playback();
                let info = format!(
                    "Loaded {} — {} tracks, {} Hz, {} bit, {}",
                    audio.file_name(),
                    audio.channels(),
                    audio.sample_rate,
                    audio.bits_per_sample,
                    format_time(audio.duration_secs()),
                );
                let channels = audio.channels();
                self.audio = Some(audio);
                self.playhead = 0;
                self.selected = None;
                self.draft = None;
                match self.pending_project.take() {
                    Some(p) => {
                        let mismatch = p.channels != channels;
                        self.apply_project(p);
                        self.status = format!(
                            "{info} — {} songs from {}{}",
                            self.songs.len(),
                            self.project_path
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default(),
                            if mismatch {
                                ". Warning: the project was made for a different track count."
                            } else {
                                ""
                            }
                        );
                    }
                    None => {
                        self.track_names = project::default_track_names(channels);
                        self.songs.clear();
                        self.next_id = 0;
                        self.dirty = false;
                        self.status = format!("{info}. Press N to add a song at the playhead.");
                    }
                }
                self.zoom_fit();
            }
            Message::Loaded(Err(e)) => {
                self.loading = false;
                self.pending_project = None;
                self.status = format!("Load failed: {e}");
            }

            Message::SaveProject => {
                if self.audio.is_none() {
                    return Task::none();
                }
                match self.project_path.clone() {
                    Some(path) => self.save_project_to(path),
                    None => return self.update(Message::SaveProjectAs),
                }
            }
            Message::SaveProjectAs => {
                let Some(audio) = &self.audio else {
                    return Task::none();
                };
                let default = self
                    .project_path
                    .clone()
                    .unwrap_or_else(|| project::sidecar_path(&audio.path));
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new().add_filter("Project", &["json"]);
                        if let Some(name) = default.file_name() {
                            dialog = dialog.set_file_name(name.to_string_lossy());
                        }
                        if let Some(dir) = default.parent() {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.save_file().await.map(|h| h.path().to_path_buf())
                    },
                    Message::ProjectPathChosen,
                );
            }
            Message::ProjectPathChosen(Some(path)) => self.save_project_to(path),
            Message::ProjectPathChosen(None) => {}

            Message::ViewChanged { offset, fpp } => {
                self.view = self.clamp_view(offset, fpp);
                self.wf_cache.clear();
            }
            Message::ZoomIn => self.zoom_by(1.0 / 1.6),
            Message::ZoomOut => self.zoom_by(1.6),
            Message::ZoomFit => self.zoom_fit(),
            Message::ZoomSong => self.zoom_song(),
            Message::VZoomIn => {
                self.v_zoom = (self.v_zoom * 1.5).min(200.0);
                self.wf_cache.clear();
            }
            Message::VZoomOut => {
                self.v_zoom = (self.v_zoom / 1.5).max(1.0);
                self.wf_cache.clear();
            }
            Message::VZoomReset => {
                self.v_zoom = 1.0;
                self.wf_cache.clear();
            }
            Message::WindowResized(w) => {
                self.window_width = w;
                self.wf_cache.clear();
            }

            Message::SetPlayhead(frame) => {
                self.playhead = frame;
                if self.playing {
                    self.start_playback();
                }
            }
            Message::PlayPause => {
                if self.playing {
                    self.stop_playback();
                } else {
                    self.start_playback();
                }
            }
            Message::Stop => {
                self.stop_playback();
                if let Some(start) = self.selected_song().map(|s| s.start) {
                    self.playhead = start;
                }
            }
            Message::SeekVisible(fraction) => {
                if self.audio.is_some() {
                    let delta = fraction * self.canvas_width() * self.view.fpp;
                    let new = (self.playhead as f64 + delta).clamp(0.0, self.frames() as f64);
                    self.playhead = new as usize;
                    // Keep the playhead in view when it steps past an edge.
                    let x = (self.playhead as f64 - self.view.offset) / self.view.fpp;
                    if x < 0.0 || x > self.canvas_width() {
                        self.view = self.clamp_view(self.view.offset + delta, self.view.fpp);
                        self.wf_cache.clear();
                    }
                    if self.playing {
                        self.start_playback();
                    }
                }
            }
            Message::Tick => {
                if let (Some(audio), Some((start, frame))) = (&self.audio, self.play_anchor) {
                    let elapsed = start.elapsed().as_secs_f64();
                    let pos = frame + (elapsed * audio.sample_rate as f64) as usize;
                    if pos >= self.play_end {
                        self.playhead = self.play_end;
                        self.stop_playback();
                    } else {
                        self.playhead = pos;
                        self.scroll_to(pos);
                    }
                }
            }
            Message::PlaySong(id) => {
                self.select_song(Some(id));
                if let Some(start) = self.song(id).map(|s| s.start) {
                    self.playhead = start;
                    self.start_playback();
                }
            }

            Message::AddSong => self.add_song(),
            Message::DeleteSong(id) => {
                if self.selected == Some(id) {
                    self.stop_playback();
                    self.selected = None;
                }
                self.songs.retain(|s| s.id != id);
                self.draft = None;
                self.song_changed();
            }
            Message::SelectSong(id) => self.select_song(id),
            Message::SongTitle(id, title) => {
                if let Some(song) = self.song_mut(id) {
                    song.title = title;
                }
                self.dirty = true;
                self.wf_cache.clear();
            }
            Message::SongStartToPlayhead => {
                if let Some(id) = self.selected {
                    self.set_song_start(id, self.playhead);
                }
            }
            Message::SongEndToPlayhead => {
                if let Some(id) = self.selected {
                    self.set_song_end(id, self.playhead);
                }
            }
            Message::MoveSongStart(id, frame) => self.set_song_start(id, frame),
            Message::MoveSongEnd(id, frame) => self.set_song_end(id, frame),

            Message::TrackActive(id, ch, active) => {
                if let Some(t) = self.song_mut(id).and_then(|s| s.tracks.get_mut(ch)) {
                    t.active = active;
                }
                self.song_changed();
            }
            Message::ToggleTrackActive(id, ch) => {
                if let Some(t) = self.song_mut(id).and_then(|s| s.tracks.get_mut(ch)) {
                    t.active = !t.active;
                }
                self.song_changed();
            }
            Message::TrackStart(id, ch, value) => self.set_track_start(id, ch, value),
            Message::TrackEnd(id, ch, value) => self.set_track_end(id, ch, value),
            Message::TrackStartToPlayhead(id, ch) => {
                self.set_track_start(id, ch, Some(self.playhead));
            }
            Message::TrackEndToPlayhead(id, ch) => {
                self.set_track_end(id, ch, Some(self.playhead));
            }
            Message::TrackVolume(id, ch, v) => {
                if let Some(t) = self.song_mut(id).and_then(|s| s.tracks.get_mut(ch)) {
                    t.volume = v;
                }
                self.dirty = true;
                self.refresh_live_mix();
            }
            Message::TrackPan(id, ch, p) => {
                if let Some(t) = self.song_mut(id).and_then(|s| s.tracks.get_mut(ch)) {
                    t.pan = p;
                }
                self.dirty = true;
                self.refresh_live_mix();
            }
            Message::TrackMute(id, ch, m) => {
                if let Some(t) = self.song_mut(id).and_then(|s| s.tracks.get_mut(ch)) {
                    t.mute = m;
                }
                self.dirty = true;
                self.refresh_live_mix();
            }
            Message::TrackSolo(id, ch, s) => {
                if let Some(t) = self.song_mut(id).and_then(|s| s.tracks.get_mut(ch)) {
                    t.solo = s;
                }
                self.dirty = true;
                self.refresh_live_mix();
            }
            Message::ResetTrackMix(id, ch) => {
                if let Some(t) = self.song_mut(id).and_then(|s| s.tracks.get_mut(ch)) {
                    t.volume = 1.0;
                    t.pan = 0.0;
                    t.mute = false;
                    t.solo = false;
                }
                self.dirty = true;
                self.refresh_live_mix();
            }
            Message::TrackName(ch, name) => {
                if let Some(n) = self.track_names.get_mut(ch) {
                    *n = name;
                }
                self.dirty = true;
                self.wf_cache.clear();
            }

            Message::TimeDraft(field, text) => self.draft = Some((field, text)),
            Message::TimeCommit(field) => self.commit_time(field),

            Message::ExportMix | Message::ExportMulti => {
                let Some(song) = self.selected_song() else {
                    return Task::none();
                };
                if self.exporting {
                    return Task::none();
                }
                let is_mix = matches!(message, Message::ExportMix);
                let stem = export::song_file_stem(self.song_number(song.id), song);
                let name = if is_mix {
                    format!("{stem} (mix).flac")
                } else if song.active_channels().len() > export::FLAC_MAX_CHANNELS {
                    format!("{stem}.wav")
                } else {
                    format!("{stem}.flac")
                };
                let dir = self
                    .audio
                    .as_ref()
                    .and_then(|a| a.path.parent().map(|p| p.to_path_buf()));
                let dialog = async move {
                    let mut dialog = rfd::AsyncFileDialog::new()
                        .add_filter("FLAC", &["flac"])
                        .add_filter("WAV", &["wav"])
                        .set_file_name(name);
                    if let Some(dir) = dir {
                        dialog = dialog.set_directory(dir);
                    }
                    dialog.save_file().await.map(|h| h.path().to_path_buf())
                };
                return if is_mix {
                    Task::perform(dialog, Message::ExportMixPathChosen)
                } else {
                    Task::perform(dialog, Message::ExportMultiPathChosen)
                };
            }
            Message::ExportMixPathChosen(Some(ref path)) | Message::ExportMultiPathChosen(Some(ref path)) => {
                let is_mix = matches!(message, Message::ExportMixPathChosen(_));
                let path = path.clone();
                let (Some(audio), Some(song)) = (self.audio.clone(), self.selected_song().cloned())
                else {
                    return Task::none();
                };
                if audio.path == path {
                    self.status = "Refusing to overwrite the loaded recording — pick another name."
                        .to_string();
                    return Task::none();
                }
                self.exporting = true;
                self.status = format!("Exporting {}…", path.display());
                return Task::perform(
                    async move {
                        if is_mix {
                            export::export_mixdown(&audio, &song, &path)
                        } else {
                            export::export_multitrack(&audio, &song, &path)
                        }
                    },
                    Message::ExportDone,
                );
            }
            Message::ExportMixPathChosen(None) | Message::ExportMultiPathChosen(None) => {}
            Message::ExportAll => {
                if self.audio.is_none() || self.songs.is_empty() || self.exporting {
                    return Task::none();
                }
                let dir = self
                    .audio
                    .as_ref()
                    .and_then(|a| a.path.parent().map(|p| p.to_path_buf()));
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new();
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.pick_folder().await.map(|h| h.path().to_path_buf())
                    },
                    Message::ExportAllDirChosen,
                );
            }
            Message::ExportAllDirChosen(Some(dir)) => {
                let Some(audio) = self.audio.clone() else {
                    return Task::none();
                };
                let songs = self.songs.clone();
                self.exporting = true;
                self.status = format!("Exporting {} songs as multitrack FLAC…", songs.len());
                return Task::perform(
                    async move {
                        export::export_all_multitrack(&audio, &songs, &dir, export::Format::Flac)
                    },
                    Message::ExportDone,
                );
            }
            Message::ExportAllDirChosen(None) => {}
            Message::ExportDone(result) => {
                self.exporting = false;
                self.status = match result {
                    Ok(msg) => msg,
                    Err(e) => format!("Export failed: {e}"),
                };
            }

            Message::DeviceSelected(name) => {
                self.stop_playback();
                self.selected_device = name.clone();
                self.player.set_device(if name == DEFAULT_DEVICE {
                    None
                } else {
                    Some(name)
                });
            }
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        // Keyboard shortcuts only fire when no widget consumed the key, so
        // typing a title or a time never triggers them.
        let events = iced::event::listen_with(|event, status, _id| match event {
            iced::Event::Window(iced::window::Event::Resized(size)) => {
                Some(Message::WindowResized(size.width))
            }
            iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
                if matches!(status, iced::event::Status::Ignored) =>
            {
                key_message(key, modifiers)
            }
            _ => None,
        });

        let mut subs = vec![events];
        if self.playing {
            subs.push(iced::time::every(Duration::from_millis(33)).map(|_| Message::Tick));
        }
        Subscription::batch(subs)
    }

    // ----- view -----------------------------------------------------------

    /// A time text field with draft editing; empty shows the placeholder.
    fn time_input<'a>(
        &'a self,
        field: TimeField,
        value: Option<usize>,
        placeholder: &'a str,
        width: f32,
    ) -> Element<'a, Message> {
        let shown = match &self.draft {
            Some((f, text)) if *f == field => text.clone(),
            _ => value.map(|v| self.format_frame(v)).unwrap_or_default(),
        };
        text_input(placeholder, &shown)
            .on_input(move |s| Message::TimeDraft(field, s))
            .on_submit(Message::TimeCommit(field))
            .size(13)
            .width(width)
            .into()
    }

    fn view(&self) -> Element<'_, Message> {
        let has_audio = self.audio.is_some();
        let selected = self.selected_song();
        let file_label = match &self.audio {
            Some(a) => format!(
                "{}{}",
                a.file_name(),
                if self.dirty { "  •" } else { "" }
            ),
            None => "No file loaded".to_string(),
        };

        let toolbar = row![
            button(text("Open…")).on_press(Message::OpenFile),
            button(text("Save project")).on_press_maybe(has_audio.then_some(Message::SaveProject)),
            button(text("Save as…")).on_press_maybe(has_audio.then_some(Message::SaveProjectAs)),
            text(file_label).size(14),
            horizontal_space(),
            button(text(if self.playing { "Pause" } else { "Play" }))
                .on_press_maybe(has_audio.then_some(Message::PlayPause)),
            button(text("Stop")).on_press_maybe(has_audio.then_some(Message::Stop)),
            button(text("+ Song at playhead (N)"))
                .on_press_maybe(has_audio.then_some(Message::AddSong)),
            button(text("Start = playhead (I)"))
                .on_press_maybe(selected.map(|_| Message::SongStartToPlayhead)),
            button(text("End = playhead (O)"))
                .on_press_maybe(selected.map(|_| Message::SongEndToPlayhead)),
            horizontal_space(),
            button(text("Export mix…"))
                .on_press_maybe((selected.is_some() && !self.exporting).then_some(Message::ExportMix)),
            button(text("Export multitrack…"))
                .on_press_maybe((selected.is_some() && !self.exporting).then_some(Message::ExportMulti)),
            button(text("Export all songs…")).on_press_maybe(
                (has_audio && !self.songs.is_empty() && !self.exporting).then_some(Message::ExportAll)
            ),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let zoom_bar = row![
            text(format!(
                "Playhead: {}  /  {}",
                self.format_frame(self.playhead),
                self.audio
                    .as_ref()
                    .map(|a| format_time(a.duration_secs()))
                    .unwrap_or_else(|| "--:--".to_string())
            ))
            .size(13),
            horizontal_space(),
            text("Zoom").size(13),
            button(text("-")).on_press(Message::ZoomOut),
            button(text("+")).on_press(Message::ZoomIn),
            button(text("Fit")).on_press(Message::ZoomFit),
            button(text("Song")).on_press_maybe(selected.map(|_| Message::ZoomSong)),
            text(format!("Amp ×{:.1}", self.v_zoom)).size(13),
            button(text("-")).on_press(Message::VZoomOut),
            button(text("+")).on_press(Message::VZoomIn),
            button(text("1:1")).on_press(Message::VZoomReset),
            horizontal_space(),
            text("Output device").size(13),
            pick_list(
                self.devices.clone(),
                Some(self.selected_device.clone()),
                Message::DeviceSelected,
            )
            .text_size(13)
            .width(260),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let canvas_h = self.canvas_height();
        let waveform_view: Element<'_, Message> = if let Some(audio) = &self.audio {
            Canvas::new(WaveformProgram {
                audio,
                songs: &self.songs,
                selected: self.selected,
                track_names: &self.track_names,
                view: self.view,
                v_zoom: self.v_zoom,
                playhead: self.playhead,
                cache: &self.wf_cache,
            })
            .width(Length::Fill)
            .height(Length::Fixed(canvas_h))
            .into()
        } else {
            container(
                text(if self.loading {
                    "Loading…"
                } else {
                    "Open a multitrack WAV or FLAC file to see its tracks here."
                })
                .size(16),
            )
            .center_x(Length::Fill)
            .center_y(Length::Fixed(canvas_h))
            .into()
        };

        // Horizontal scrollbar for the waveform: maps the view offset onto
        // the scrollable range at the current zoom level.
        let scroll_bar: Element<'_, Message> = if let Some(audio) = &self.audio {
            let frames = audio.frames() as f64;
            let visible = self.canvas_width() * self.view.fpp;
            let max_offset = (frames - visible).max(0.0);
            let fpp = self.view.fpp;
            slider(
                0.0..=max_offset.max(1.0),
                self.view.offset.min(max_offset.max(1.0)),
                move |offset| Message::ViewChanged { offset, fpp },
            )
            .step(self.view.fpp.max(1.0))
            .width(Length::Fill)
            .into()
        } else {
            Space::with_height(0).into()
        };

        let help = text(
            "Strip: drag song flags · click a band to select · Lanes: click = playhead · drag a track's edge = per-track start/end · \
             double-click a lane = toggle track · Wheel / +/-: zoom · Shift+wheel: pan · Space: play/pause · ←/→: step · N: new song · I/O: song start/end",
        )
        .size(12);

        let songs_pane = self.songs_pane();
        let mixer_pane = self.mixer_pane();

        let lower = row![
            container(songs_pane).width(Length::FillPortion(2)),
            container(mixer_pane).width(Length::FillPortion(3)),
        ]
        .spacing(16)
        .height(Length::Fill);

        column![
            toolbar,
            zoom_bar,
            waveform_view,
            scroll_bar,
            help,
            lower,
            text(&self.status).size(13),
        ]
        .spacing(8)
        .padding(12)
        .into()
    }

    fn songs_pane(&self) -> Element<'_, Message> {
        let header = row![
            text(format!("Songs ({})", self.songs.len())).size(15),
            horizontal_space(),
            text("Start / End · Enter to apply").size(12),
        ]
        .align_y(iced::Alignment::Center);

        let rows: Vec<Element<'_, Message>> = self
            .songs
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let is_selected = Some(s.id) == self.selected;
                let used = s.active_channels().len();
                let select = button(text(format!("{:02}", i + 1)).size(13))
                    .style(if is_selected {
                        button::primary
                    } else {
                        button::secondary
                    })
                    .on_press(Message::SelectSong(Some(s.id)));
                row![
                    select,
                    text_input("Title", &s.title)
                        .size(13)
                        .on_input({
                            let id = s.id;
                            move |t| Message::SongTitle(id, t)
                        })
                        .width(Length::Fill),
                    self.time_input(TimeField::SongStart(s.id), Some(s.start), "start", 92.0),
                    self.time_input(TimeField::SongEnd(s.id), Some(s.end), "end", 92.0),
                    text(format!("{}/{}", used, s.tracks.len())).size(12).width(34),
                    button(text("Play").size(13)).on_press(Message::PlaySong(s.id)),
                    button(text("Delete").size(13))
                        .style(button::danger)
                        .on_press(Message::DeleteSong(s.id)),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center)
                .into()
            })
            .collect();

        let list: Element<'_, Message> = if rows.is_empty() {
            container(
                text(if self.audio.is_some() {
                    "No songs yet. Move the playhead to where a song starts and press N (or the + Song button)."
                } else {
                    ""
                })
                .size(13),
            )
            .padding(8)
            .into()
        } else {
            scrollable(
                iced::widget::Column::with_children(rows)
                    .spacing(4)
                    .padding(iced::Padding::from([0, 8])),
            )
            .height(Length::Fill)
            .into()
        };

        column![header, list].spacing(8).into()
    }

    fn mixer_pane(&self) -> Element<'_, Message> {
        let Some(song) = self.selected_song() else {
            return container(
                text(if self.songs.is_empty() {
                    "The mixer for the selected song appears here."
                } else {
                    "Select a song to edit which tracks it uses and their mix."
                })
                .size(13),
            )
            .padding(8)
            .into();
        };
        let id = song.id;
        let sr = self.sample_rate();

        let header = row![
            text(format!(
                "Song {:02}{} — {} to {} ({})",
                self.song_number(id),
                if song.title.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", song.title)
                },
                self.format_frame(song.start),
                self.format_frame(song.end),
                format_time(song.frames() as f64 / sr),
            ))
            .size(15),
            horizontal_space(),
            text("Track in/out: blank = song boundary · PH = playhead").size(12),
        ]
        .align_y(iced::Alignment::Center);

        let columns = row![
            text("Use").size(11).width(44),
            text("Track").size(11).width(120),
            text("In").size(11).width(158),
            text("Out").size(11).width(158),
            text("Volume").size(11).width(196),
            text("Pan").size(11).width(160),
            text("").size(11).width(90),
        ]
        .spacing(6);

        let rows: Vec<Element<'_, Message>> = song
            .tracks
            .iter()
            .enumerate()
            .map(|(ch, t)| {
                let name = self
                    .track_names
                    .get(ch)
                    .cloned()
                    .unwrap_or_default();
                let active = t.active;
                let range_note = match song.track_range(ch) {
                    Some((s, e)) if active => {
                        format!("{}", format_time((e - s) as f64 / sr))
                    }
                    _ => "—".to_string(),
                };
                row![
                    checkbox("", active)
                        .on_toggle(move |v| Message::TrackActive(id, ch, v))
                        .size(18)
                        .width(44),
                    text_input("Name", &name)
                        .size(13)
                        .on_input(move |n| Message::TrackName(ch, n))
                        .width(120),
                    row![
                        self.time_input(TimeField::TrackStart(id, ch), t.start, "song start", 92.0),
                        button(text("PH").size(11))
                            .on_press_maybe(active.then_some(Message::TrackStartToPlayhead(id, ch))),
                        button(text("×").size(11))
                            .style(button::secondary)
                            .on_press_maybe(t.start.map(|_| Message::TrackStart(id, ch, None))),
                    ]
                    .spacing(2)
                    .width(158)
                    .align_y(iced::Alignment::Center),
                    row![
                        self.time_input(TimeField::TrackEnd(id, ch), t.end, "song end", 92.0),
                        button(text("PH").size(11))
                            .on_press_maybe(active.then_some(Message::TrackEndToPlayhead(id, ch))),
                        button(text("×").size(11))
                            .style(button::secondary)
                            .on_press_maybe(t.end.map(|_| Message::TrackEnd(id, ch, None))),
                    ]
                    .spacing(2)
                    .width(158)
                    .align_y(iced::Alignment::Center),
                    row![
                        slider(0.0..=1.5, t.volume, move |v| Message::TrackVolume(id, ch, v))
                            .step(0.01_f32)
                            .width(120),
                        text(mix::volume_to_db_label(t.volume)).size(12).width(70),
                    ]
                    .spacing(6)
                    .width(196)
                    .align_y(iced::Alignment::Center),
                    row![
                        slider(-1.0..=1.0, t.pan, move |p| Message::TrackPan(id, ch, p))
                            .step(0.01_f32)
                            .width(110),
                        text(mix::pan_label(t.pan)).size(12).width(44),
                    ]
                    .spacing(6)
                    .width(160)
                    .align_y(iced::Alignment::Center),
                    button(text("M").size(12))
                        .style(if t.mute { button::danger } else { button::secondary })
                        .on_press(Message::TrackMute(id, ch, !t.mute)),
                    button(text("S").size(12))
                        .style(if t.solo { button::success } else { button::secondary })
                        .on_press(Message::TrackSolo(id, ch, !t.solo)),
                    button(text("Reset").size(11))
                        .style(button::text)
                        .on_press(Message::ResetTrackMix(id, ch)),
                    text(range_note).size(12).style(move |theme: &Theme| text::Style {
                        color: if active {
                            None
                        } else {
                            Some(theme.extended_palette().background.strong.color)
                        },
                    }),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center)
                .into()
            })
            .collect();

        let list = scrollable(
            iced::widget::Column::with_children(rows)
                .spacing(4)
                .padding(iced::Padding::from([0, 8])),
        )
        .height(Length::Fill);

        column![header, columns, list].spacing(8).into()
    }
}

fn key_message(key: keyboard::Key, mods: keyboard::Modifiers) -> Option<Message> {
    use keyboard::key::Named;
    match key.as_ref() {
        keyboard::Key::Named(Named::Space) => Some(Message::PlayPause),
        // Hardware media keys; most keyboards send the combined
        // play/pause key, but some have separate play and pause keys.
        keyboard::Key::Named(Named::MediaPlayPause | Named::MediaPlay | Named::MediaPause) => {
            Some(Message::PlayPause)
        }
        keyboard::Key::Named(Named::MediaStop) => Some(Message::Stop),
        keyboard::Key::Named(Named::ArrowLeft) => Some(Message::SeekVisible(-0.05)),
        keyboard::Key::Named(Named::ArrowRight) => Some(Message::SeekVisible(0.05)),
        keyboard::Key::Character("n") | keyboard::Key::Character("N") => Some(Message::AddSong),
        keyboard::Key::Character("i") | keyboard::Key::Character("I") => {
            Some(Message::SongStartToPlayhead)
        }
        keyboard::Key::Character("o") | keyboard::Key::Character("O") => {
            Some(Message::SongEndToPlayhead)
        }
        // The key is the logical (layout-dependent) character, so with
        // shift held the +/- keys may report their shifted symbols:
        // "?" (Nordic +), "*" (German +), "_" (shifted -).
        keyboard::Key::Character(c @ ("+" | "=" | "?" | "*")) => {
            if mods.shift() {
                Some(Message::VZoomIn)
            } else if c == "+" || c == "=" {
                Some(Message::ZoomIn)
            } else {
                None
            }
        }
        keyboard::Key::Character("-" | "_") => {
            if mods.shift() {
                Some(Message::VZoomOut)
            } else {
                Some(Message::ZoomOut)
            }
        }
        _ => None,
    }
}
