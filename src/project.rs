//! Project model and its JSON file: which songs live where on the tape, which
//! tracks each song uses, and the simple mix for each track.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Extension of the sidecar project file, saved next to the recording.
pub const PROJECT_SUFFIX: &str = ".mtsplit.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrackSettings {
    /// Whether the song uses this tape track at all. Inactive tracks are
    /// silent in playback and left out of the multitrack export.
    pub active: bool,
    /// Optional per-track start (absolute frame) overriding the song start.
    #[serde(default)]
    pub start: Option<usize>,
    /// Optional per-track end (absolute frame) overriding the song end.
    #[serde(default)]
    pub end: Option<usize>,
    /// Linear gain, 1.0 = unity.
    pub volume: f32,
    /// -1 = left, 0 = center, 1 = right.
    pub pan: f32,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub solo: bool,
}

impl Default for TrackSettings {
    fn default() -> Self {
        Self {
            active: true,
            start: None,
            end: None,
            volume: 1.0,
            pan: 0.0,
            mute: false,
            solo: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Song {
    /// Runtime identity only; reassigned when a project is loaded.
    #[serde(skip)]
    pub id: u64,
    pub title: String,
    /// Song range on the tape, in absolute sample frames, `start < end`.
    pub start: usize,
    pub end: usize,
    /// One entry per tape track.
    pub tracks: Vec<TrackSettings>,
}

impl Song {
    pub fn new(id: u64, title: String, start: usize, end: usize, channels: usize) -> Self {
        Self {
            id,
            title,
            start,
            end,
            tracks: vec![TrackSettings::default(); channels],
        }
    }

    pub fn frames(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Effective audible range of one track within the song: the song range,
    /// narrowed by the track's own start/end overrides. `None` when the track
    /// is inactive (or its range collapsed to nothing).
    pub fn track_range(&self, ch: usize) -> Option<(usize, usize)> {
        let t = self.tracks.get(ch)?;
        if !t.active {
            return None;
        }
        let start = t.start.map_or(self.start, |s| s.clamp(self.start, self.end));
        let end = t.end.map_or(self.end, |e| e.clamp(self.start, self.end));
        (end > start).then_some((start, end))
    }

    pub fn active_channels(&self) -> Vec<usize> {
        (0..self.tracks.len())
            .filter(|&ch| self.tracks[ch].active)
            .collect()
    }

    /// Grows or shrinks the track list to match a recording.
    pub fn fit_channels(&mut self, channels: usize) {
        self.tracks.resize(channels, TrackSettings::default());
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    /// The recording's files in track order (one interleaved file, or several
    /// mono/stereo files): bare names when they sit next to the project
    /// file, otherwise full paths.
    #[serde(default)]
    pub audio_files: Vec<String>,
    /// Older projects stored a single file here; kept so they still load.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub audio_file: String,
    pub sample_rate: u32,
    pub channels: usize,
    pub track_names: Vec<String>,
    pub songs: Vec<Song>,
    /// How "Export mix" writes files; absent in older projects.
    #[serde(default)]
    pub mix_format: Option<crate::export::MixFormat>,
}

impl Project {
    /// The recording's files, whichever field the project used.
    pub fn files(&self) -> Vec<String> {
        if self.audio_files.is_empty() && !self.audio_file.is_empty() {
            vec![self.audio_file.clone()]
        } else {
            self.audio_files.clone()
        }
    }
}

pub fn save(path: &Path, project: &Project) -> Result<(), String> {
    let text = serde_json::to_string_pretty(project)
        .map_err(|e| format!("Cannot serialize project: {e}"))?;
    std::fs::write(path, text).map_err(|e| format!("Cannot write {}: {e}", path.display()))
}

pub fn load(path: &Path) -> Result<Project, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    let mut project: Project =
        serde_json::from_str(&text).map_err(|e| format!("Invalid project file: {e}"))?;
    for (i, song) in project.songs.iter_mut().enumerate() {
        song.id = i as u64;
    }
    Ok(project)
}

/// Default project path: next to a single recording with its stem, or, for
/// a set of files, in their directory named after that directory.
pub fn sidecar_path(audio_paths: &[PathBuf]) -> PathBuf {
    let first = audio_paths.first().cloned().unwrap_or_default();
    if audio_paths.len() > 1 {
        let dir = first.parent().unwrap_or(Path::new("."));
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "tape".to_string());
        dir.join(format!("{name}{PROJECT_SUFFIX}"))
    } else {
        let stem = first
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "tape".to_string());
        first.with_file_name(format!("{stem}{PROJECT_SUFFIX}"))
    }
}

/// Finds the recording a project refers to: as written (absolute or relative
/// to the project file), else by bare file name next to the project file.
pub fn resolve_audio_path(project_path: &Path, audio_file: &str) -> Option<PathBuf> {
    let dir = project_path.parent().unwrap_or(Path::new("."));
    let referenced = PathBuf::from(audio_file);
    let mut candidates = Vec::new();
    if referenced.is_absolute() {
        candidates.push(referenced.clone());
    } else {
        candidates.push(dir.join(&referenced));
    }
    if let Some(name) = referenced.file_name() {
        candidates.push(dir.join(name));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// How the recording is referenced from the project file: bare name when
/// they share a directory, otherwise the full path.
pub fn audio_reference(project_path: &Path, audio_path: &Path) -> String {
    let same_dir = project_path.parent() == audio_path.parent();
    if same_dir {
        audio_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        audio_path.to_string_lossy().into_owned()
    }
}
