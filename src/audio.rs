use std::path::{Path, PathBuf};

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Number of frames aggregated into one waveform peak bin.
pub const PEAK_BIN: usize = 512;

/// A decoded multitrack recording, held entirely in memory. It comes either
/// from one interleaved multichannel file or from several mono/stereo files
/// laid out as consecutive tracks.
pub struct AudioData {
    /// The (first) source file; its directory is where exports and the
    /// project sidecar default to.
    pub path: PathBuf,
    /// Every source file, in track order.
    pub sources: Vec<PathBuf>,
    /// Channel count of each source file, in the same order as `sources`.
    pub source_channels: Vec<usize>,
    /// Planar samples, `tracks[channel][frame]`, normalized to [-1.0, 1.0].
    /// Planar storage makes per-track work (mixing, exporting a subset of
    /// tracks) a plain slice walk.
    pub tracks: Vec<Vec<f32>>,
    pub sample_rate: u32,
    pub bits_per_sample: u32,
    /// Per channel: (min, max) per PEAK_BIN frames.
    pub peaks: Vec<Vec<(f32, f32)>>,
}

impl std::fmt::Debug for AudioData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioData")
            .field("path", &self.path)
            .field("channels", &self.channels())
            .field("sample_rate", &self.sample_rate)
            .field("frames", &self.frames())
            .finish()
    }
}

impl AudioData {
    pub fn channels(&self) -> usize {
        self.tracks.len()
    }

    pub fn frames(&self) -> usize {
        self.tracks.first().map_or(0, |t| t.len())
    }

    pub fn duration_secs(&self) -> f64 {
        self.frames() as f64 / self.sample_rate as f64
    }

    pub fn is_multi_file(&self) -> bool {
        self.sources.len() > 1
    }

    /// Display name: the file name, or "N files in <dir>" for a set.
    pub fn file_name(&self) -> String {
        if self.is_multi_file() {
            let dir = self
                .path
                .parent()
                .and_then(|d| d.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            format!("{} files in {dir}", self.sources.len())
        } else {
            self.path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        }
    }

    /// Track names to start from: "Track n" for one multichannel file, the
    /// file stems for a set of files (with L/R for stereo ones).
    pub fn default_track_names(&self) -> Vec<String> {
        if !self.is_multi_file() {
            return (1..=self.channels()).map(|n| format!("Track {n}")).collect();
        }
        let mut names = Vec::with_capacity(self.channels());
        for (path, &n) in self.sources.iter().zip(&self.source_channels) {
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            match n {
                1 => names.push(stem),
                2 => {
                    names.push(format!("{stem} L"));
                    names.push(format!("{stem} R"));
                }
                _ => names.extend((1..=n).map(|i| format!("{stem} {i}"))),
            }
        }
        names
    }

    pub fn frame_of_secs(&self, secs: f64) -> usize {
        ((secs.max(0.0) * self.sample_rate as f64).round() as usize).min(self.frames())
    }
}

/// Convenience for a single file (used by the tests).
#[cfg_attr(not(test), allow(dead_code))]
pub fn load(path: &Path) -> Result<AudioData, String> {
    load_many(&[path.to_path_buf()])
}

/// Loads one interleaved multichannel file, or several mono/stereo files
/// that become consecutive tracks. The files are assumed to start at the
/// same instant; shorter ones are padded with silence to the longest. Files
/// are ordered by name (numbers compared numerically, so "track 2" sorts
/// before "track 10"), whatever order they were picked in.
pub fn load_many(paths: &[PathBuf]) -> Result<AudioData, String> {
    if paths.is_empty() {
        return Err("No file given".to_string());
    }
    let mut paths = paths.to_vec();
    if paths.len() > 1 {
        paths.sort_by_cached_key(|p| natural_key(&p.file_name().unwrap_or_default().to_string_lossy()));
    }

    let mut tracks: Vec<Vec<f32>> = Vec::new();
    let mut source_channels = Vec::with_capacity(paths.len());
    let mut sample_rate = 0u32;
    let mut bits_per_sample = 0u32;
    for path in &paths {
        let decoded = decode_file(path).map_err(|e| {
            if paths.len() > 1 {
                format!("{}: {e}", path.display())
            } else {
                e
            }
        })?;
        if sample_rate == 0 {
            sample_rate = decoded.sample_rate;
        } else if decoded.sample_rate != sample_rate {
            return Err(format!(
                "{} is {} Hz but the first file is {sample_rate} Hz; all files must share a sample rate",
                path.display(),
                decoded.sample_rate
            ));
        }
        bits_per_sample = bits_per_sample.max(decoded.bits_per_sample);
        source_channels.push(decoded.tracks.len());
        tracks.extend(decoded.tracks);
    }

    // Pad to the longest file so every track has the same frame count.
    let frames = tracks.iter().map(|t| t.len()).max().unwrap_or(0);
    for t in &mut tracks {
        t.resize(frames, 0.0);
        t.shrink_to_fit();
    }

    let peaks = tracks.iter().map(|t| compute_peaks(t)).collect();

    Ok(AudioData {
        path: paths[0].clone(),
        sources: paths,
        source_channels,
        tracks,
        sample_rate,
        bits_per_sample,
        peaks,
    })
}

/// Sort key that compares digit runs numerically.
fn natural_key(name: &str) -> Vec<(u64, String)> {
    let lower = name.to_lowercase();
    let mut key = Vec::new();
    let mut chars = lower.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            let mut n = 0u64;
            while let Some(&d) = chars.peek() {
                if !d.is_ascii_digit() {
                    break;
                }
                n = n.saturating_mul(10).saturating_add(d as u64 - '0' as u64);
                chars.next();
            }
            key.push((n, String::new()));
        } else {
            let mut text = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    break;
                }
                text.push(d);
                chars.next();
            }
            key.push((u64::MAX, text));
        }
    }
    key
}

struct Decoded {
    tracks: Vec<Vec<f32>>,
    sample_rate: u32,
    bits_per_sample: u32,
}

fn decode_file(path: &Path) -> Result<Decoded, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Unsupported format: {e}"))?;

    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| "No audio track found".to_string())?;
    let track_id = track.id;
    let params = track.codec_params.clone();

    let sample_rate = params
        .sample_rate
        .ok_or_else(|| "Unknown sample rate".to_string())?;
    let channels = params
        .channels
        .ok_or_else(|| "Unknown channel layout".to_string())?
        .count();
    let bits_per_sample = params.bits_per_sample.unwrap_or(16);

    let mut decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| format!("Cannot create decoder: {e}"))?;

    // Presize from the container's frame count when known so the per-track
    // vectors do not go through repeated doubling on multi-GB recordings.
    let capacity = params
        .n_frames
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0);
    let mut tracks: Vec<Vec<f32>> = (0..channels).map(|_| Vec::with_capacity(capacity)).collect();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(format!("Read error: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                if sample_buf.is_none() {
                    sample_buf = Some(SampleBuffer::new(
                        decoded.capacity() as u64,
                        *decoded.spec(),
                    ));
                }
                let buf = sample_buf.as_mut().unwrap();
                buf.copy_interleaved_ref(decoded);
                for frame in buf.samples().chunks_exact(channels) {
                    for (track, &s) in tracks.iter_mut().zip(frame) {
                        track.push(s);
                    }
                }
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("Decode error: {e}")),
        }
    }

    if tracks.first().is_none_or(|t| t.is_empty()) {
        return Err("File contains no audio".to_string());
    }

    Ok(Decoded {
        tracks,
        sample_rate,
        bits_per_sample,
    })
}

fn compute_peaks(track: &[f32]) -> Vec<(f32, f32)> {
    track
        .chunks(PEAK_BIN)
        .map(|bin| {
            bin.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &s| {
                (lo.min(s), hi.max(s))
            })
        })
        .collect()
}

/// `mm:ss.mmm`, or `h:mm:ss.mmm` when there is an hour part.
pub fn format_time(secs: f64) -> String {
    let secs = secs.max(0.0);
    let h = (secs / 3600.0) as u64;
    let m = ((secs / 60.0) as u64) % 60;
    let s = (secs as u64) % 60;
    let ms = ((secs - secs.floor()) * 1000.0).round() as u64;
    let (s, ms) = if ms >= 1000 { (s + 1, 0) } else { (s, ms) };
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{ms:03}")
    } else {
        format!("{m:02}:{s:02}.{ms:03}")
    }
}

/// Parses `[[h:]mm:]ss[.fraction]` into seconds. Plain seconds (`95.5`) work too.
pub fn parse_time(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut total = 0.0;
    for (i, part) in parts.iter().enumerate() {
        let is_last = i == parts.len() - 1;
        let v: f64 = if is_last {
            part.trim().parse().ok()?
        } else {
            part.trim().parse::<u64>().ok()? as f64
        };
        if v < 0.0 || !v.is_finite() {
            return None;
        }
        total = total * 60.0 + v;
    }
    Some(total)
}
