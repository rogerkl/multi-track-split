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

/// A decoded multitrack recording, held entirely in memory.
pub struct AudioData {
    pub path: PathBuf,
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

    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    pub fn file_stem(&self) -> String {
        self.path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "tape".to_string())
    }

    pub fn frame_of_secs(&self, secs: f64) -> usize {
        ((secs.max(0.0) * self.sample_rate as f64).round() as usize).min(self.frames())
    }
}

pub fn load(path: &Path) -> Result<AudioData, String> {
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
    for t in &mut tracks {
        t.shrink_to_fit();
    }

    let peaks = tracks.iter().map(|t| compute_peaks(t)).collect();

    Ok(AudioData {
        path: path.to_path_buf(),
        tracks,
        sample_rate,
        bits_per_sample,
        peaks,
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
