//! Song export: a stereo mixdown (the simple mix, as heard in playback) or a
//! multitrack file holding only the tracks a song uses, each silent outside
//! its own start/end. Format is picked from the file extension: `.flac`
//! (up to 8 channels) or `.wav`. Multitrack files keep the recording's
//! integer bit depth; mixdowns go to 32-bit float WAV so the sum of the
//! tracks never clips (FLAC mixdowns are integer and clamped).

use std::io::Write;
use std::path::Path;

use flacenc::bitsink::ByteSink;
use flacenc::component::BitRepr;
use flacenc::error::Verify;

use serde::{Deserialize, Serialize};

use crate::audio::AudioData;
use crate::mix;
use crate::project::Song;

/// Whether a song's stems go into one interleaved file or one file per track.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StemLayout {
    Interleaved,
    Tracks,
}

/// How multitrack stems are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StemFormat {
    pub format: Format,
    pub layout: StemLayout,
}

impl StemFormat {
    pub const ALL: [StemFormat; 4] = [
        StemFormat {
            format: Format::Wav,
            layout: StemLayout::Interleaved,
        },
        StemFormat {
            format: Format::Wav,
            layout: StemLayout::Tracks,
        },
        StemFormat {
            format: Format::Flac,
            layout: StemLayout::Interleaved,
        },
        StemFormat {
            format: Format::Flac,
            layout: StemLayout::Tracks,
        },
    ];

    pub fn extension(self) -> &'static str {
        self.format.extension()
    }

    /// Interleaved FLAC cannot hold a recording with more tracks than FLAC
    /// allows, so that choice is offered only for recordings that fit.
    pub fn available_for(channels: usize) -> Vec<StemFormat> {
        Self::ALL
            .into_iter()
            .filter(|f| f.is_available_for(channels))
            .collect()
    }

    pub fn is_available_for(self, channels: usize) -> bool {
        !(self.format == Format::Flac
            && self.layout == StemLayout::Interleaved
            && channels > FLAC_MAX_CHANNELS)
    }
}

impl Default for StemFormat {
    fn default() -> Self {
        StemFormat {
            format: Format::Flac,
            layout: StemLayout::Interleaved,
        }
    }
}

impl std::fmt::Display for StemFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self.format {
            Format::Wav => "WAV",
            Format::Flac => "FLAC",
        };
        match self.layout {
            StemLayout::Interleaved => write!(f, "{name} interleaved"),
            StemLayout::Tracks => write!(f, "{name} one file per track"),
        }
    }
}

/// Peak level normalized mixes are scaled to, leaving a little headroom
/// for inter-sample peaks and lossy transcodes.
pub const NORMALIZE_TARGET_DB: f32 = -1.0;

/// How stereo mixdowns are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MixFormat {
    /// 32-bit float WAV; nothing is scaled or clamped.
    Float32,
    /// Integer file at `bits` (16 or 24), the mix scaled so its peak sits at
    /// `NORMALIZE_TARGET_DB`.
    Normalized { format: Format, bits: u32 },
}

impl MixFormat {
    pub const ALL: [MixFormat; 5] = [
        MixFormat::Float32,
        MixFormat::Normalized {
            format: Format::Wav,
            bits: 24,
        },
        MixFormat::Normalized {
            format: Format::Wav,
            bits: 16,
        },
        MixFormat::Normalized {
            format: Format::Flac,
            bits: 24,
        },
        MixFormat::Normalized {
            format: Format::Flac,
            bits: 16,
        },
    ];

    pub fn extension(self) -> &'static str {
        match self {
            MixFormat::Float32 => "wav",
            MixFormat::Normalized { format, .. } => format.extension(),
        }
    }
}

impl Default for MixFormat {
    fn default() -> Self {
        MixFormat::Float32
    }
}

impl std::fmt::Display for MixFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MixFormat::Float32 => write!(f, "WAV 32-bit float"),
            MixFormat::Normalized { format, bits } => write!(
                f,
                "{} {bits}-bit, normalized",
                match format {
                    Format::Wav => "WAV",
                    Format::Flac => "FLAC",
                }
            ),
        }
    }
}

/// FLAC's channel limit (and flacenc's).
pub const FLAC_MAX_CHANNELS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Format {
    Wav,
    Flac,
}

impl Format {
    pub fn from_path(path: &Path) -> Result<Format, String> {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("flac") => Ok(Format::Flac),
            Some("wav") => Ok(Format::Wav),
            other => Err(format!(
                "Unsupported output extension {:?}: use .flac or .wav",
                other.unwrap_or("")
            )),
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Wav => "wav",
            Format::Flac => "flac",
        }
    }
}

pub fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// `NN - Title`, with a placeholder when the song has no title.
pub fn song_file_stem(number: usize, song: &Song) -> String {
    let title = if song.title.trim().is_empty() {
        format!("Song {number:02}")
    } else {
        sanitize_filename(&song.title)
    };
    format!("{number:02} - {title}")
}

/// Interleaved samples of the song's active tracks, in tape track order.
/// Each track is zero outside its effective range, so a track that starts
/// 20 s into the song is silent for its first 20 s in the file.
pub fn render_multitrack(audio: &AudioData, song: &Song) -> Result<(Vec<f32>, usize), String> {
    let channels: Vec<usize> = song
        .active_channels()
        .into_iter()
        .filter(|&ch| ch < audio.channels())
        .collect();
    if channels.is_empty() {
        return Err("The song has no active tracks".to_string());
    }
    let end = song.end.min(audio.frames());
    if end <= song.start {
        return Err("The song range is empty".to_string());
    }
    let frames = end - song.start;
    let n = channels.len();
    let mut out = vec![0.0f32; frames * n];
    for (slot, &ch) in channels.iter().enumerate() {
        let Some((s, e)) = song.track_range(ch) else {
            continue;
        };
        let e = e.min(end);
        for (f, &v) in (s..e).zip(&audio.tracks[ch][s..e]) {
            out[(f - song.start) * n + slot] = v;
        }
    }
    Ok((out, n))
}

/// One track of the song as a mono stem: zero outside the track's effective
/// range, `None` when the song does not use the track.
pub fn render_track_stem(audio: &AudioData, song: &Song, ch: usize) -> Option<Vec<f32>> {
    let end = song.end.min(audio.frames());
    if end <= song.start || ch >= audio.channels() || !song.tracks.get(ch)?.active {
        return None;
    }
    let mut out = vec![0.0f32; end - song.start];
    if let Some((s, e)) = song.track_range(ch) {
        let e = e.min(end);
        out[s - song.start..e - song.start].copy_from_slice(&audio.tracks[ch][s..e]);
    }
    Some(out)
}

/// Interleaved stereo mixdown of the song plus its peak level. The samples
/// are not clamped; integer writers clamp when quantizing.
pub fn render_mixdown(audio: &AudioData, song: &Song) -> Result<(Vec<f32>, f32), String> {
    let end = song.end.min(audio.frames());
    if end <= song.start {
        return Err("The song range is empty".to_string());
    }
    let mixes = mix::song_mixes(song);
    if mixes.is_empty() {
        return Err("Nothing to mix: every track is inactive or muted".to_string());
    }
    let mut out = Vec::with_capacity(2 * (end - song.start));
    let peak = mix::render_stereo(audio, &mixes, song.start, end, false, &mut out);
    Ok((out, peak))
}

/// Encodes normalized samples at the recording's bit depth (16–24 bit) and
/// writes them to `path` in the format its extension names.
pub fn write_audio(
    path: &Path,
    samples: &[f32],
    channels: usize,
    bits_per_sample: u32,
    sample_rate: u32,
) -> Result<(), String> {
    let format = Format::from_path(path)?;
    if format == Format::Flac && channels > FLAC_MAX_CHANNELS {
        return Err(format!(
            "FLAC supports at most {FLAC_MAX_CHANNELS} channels; this song uses {channels}. Export as .wav instead."
        ));
    }
    let bits = bits_per_sample.clamp(16, 24);
    let scale = (1i64 << (bits - 1)) as f32;
    let max_val = scale - 1.0;
    let quantized: Vec<i32> = samples
        .iter()
        .map(|&s| (s * scale).round().clamp(-scale, max_val) as i32)
        .collect();

    let bytes = match format {
        Format::Flac => encode_flac(&quantized, channels, bits as usize, sample_rate as usize)?,
        Format::Wav => encode_wav(&WavSamples::Int(&quantized), channels, bits, sample_rate)?,
    };
    std::fs::write(path, bytes).map_err(|e| format!("Cannot write {}: {e}", path.display()))
}

pub fn export_multitrack(audio: &AudioData, song: &Song, path: &Path) -> Result<String, String> {
    let (samples, channels) = render_multitrack(audio, song)?;
    write_audio(path, &samples, channels, audio.bits_per_sample, audio.sample_rate)?;
    Ok(format!(
        "Wrote {} ({channels} tracks, {}).",
        path.display(),
        crate::audio::format_time(song.frames() as f64 / audio.sample_rate as f64)
    ))
}

/// Writes a 32-bit float WAV; nothing is clamped, so hot mixes survive.
pub fn write_float_wav(
    path: &Path,
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
) -> Result<(), String> {
    let bytes = encode_wav(&WavSamples::Float(samples), channels, 32, sample_rate)?;
    std::fs::write(path, bytes).map_err(|e| format!("Cannot write {}: {e}", path.display()))
}

/// Stereo mixdown. The container follows the file extension; `mode` decides
/// between float (WAV only) and a normalized integer file.
pub fn export_mixdown(
    audio: &AudioData,
    song: &Song,
    path: &Path,
    mode: MixFormat,
) -> Result<String, String> {
    let format = Format::from_path(path)?;
    let (mut samples, peak) = render_mixdown(audio, song)?;
    let peak_db = 20.0 * peak.max(1e-9).log10();
    match mode {
        MixFormat::Float32 => {
            if format == Format::Flac {
                return Err(
                    "FLAC cannot hold 32-bit float; pick a normalized mix format or a .wav name"
                        .to_string(),
                );
            }
            write_float_wav(path, &samples, 2, audio.sample_rate)?;
            Ok(format!(
                "Wrote {} (32-bit float, peak {peak_db:+.1} dB).",
                path.display()
            ))
        }
        MixFormat::Normalized { bits, .. } => {
            let target = 10f32.powf(NORMALIZE_TARGET_DB / 20.0);
            let gain = if peak > 0.0 { target / peak } else { 1.0 };
            for v in &mut samples {
                *v *= gain;
            }
            write_audio(path, &samples, 2, bits, audio.sample_rate)?;
            Ok(format!(
                "Wrote {} ({bits}-bit, normalized {:+.1} dB to a {NORMALIZE_TARGET_DB:.0} dBFS peak).",
                path.display(),
                20.0 * gain.log10()
            ))
        }
    }
}

/// Exports every song's mixdown into `dir`, named `NN - Title (mix).<ext>`.
pub fn export_all_mixdowns(
    audio: &AudioData,
    songs: &[Song],
    dir: &Path,
    mode: MixFormat,
) -> Result<String, String> {
    let mut written = 0;
    for (i, song) in songs.iter().enumerate() {
        let path = dir.join(format!("{} (mix).{}", song_file_stem(i + 1, song), mode.extension()));
        export_mixdown(audio, song, &path, mode).map_err(|e| format!("Song {}: {e}", i + 1))?;
        written += 1;
    }
    Ok(format!("Exported {written} mixes ({mode}) to {}.", dir.display()))
}

/// File name of one stem inside a song's folder: `T03 - Bass.flac`.
pub fn stem_file_name(ch: usize, track_names: &[String], ext: &str) -> String {
    let name = track_names
        .get(ch)
        .map(|n| sanitize_filename(n))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("Track {}", ch + 1));
    format!("T{:02} - {name}.{ext}", ch + 1)
}

/// Writes the song's used tracks as mono files into `dir` (created if
/// needed), each silent outside the track's own range.
pub fn export_stem_tracks(
    audio: &AudioData,
    song: &Song,
    track_names: &[String],
    dir: &Path,
    format: Format,
) -> Result<String, String> {
    let channels = song.active_channels();
    if channels.is_empty() {
        return Err("The song has no active tracks".to_string());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    let mut written = 0;
    for ch in channels {
        let Some(samples) = render_track_stem(audio, song, ch) else {
            continue;
        };
        let path = dir.join(stem_file_name(ch, track_names, format.extension()));
        write_audio(&path, &samples, 1, audio.bits_per_sample, audio.sample_rate)?;
        written += 1;
    }
    Ok(format!("Wrote {written} track files to {}.", dir.display()))
}

/// Exports every song into `dir`: an interleaved `NN - Title.<ext>` per
/// song, or a `NN - Title/` folder of per-track files. Interleaved FLAC
/// falls back to WAV for songs with more tracks than FLAC allows.
pub fn export_all_multitrack(
    audio: &AudioData,
    songs: &[Song],
    track_names: &[String],
    dir: &Path,
    stems: StemFormat,
) -> Result<String, String> {
    let mut written = 0;
    for (i, song) in songs.iter().enumerate() {
        let stem = song_file_stem(i + 1, song);
        match stems.layout {
            StemLayout::Interleaved => {
                let format = if stems.format == Format::Flac
                    && song.active_channels().len() > FLAC_MAX_CHANNELS
                {
                    Format::Wav
                } else {
                    stems.format
                };
                let path = dir.join(format!("{stem}.{}", format.extension()));
                export_multitrack(audio, song, &path).map_err(|e| format!("Song {}: {e}", i + 1))?;
            }
            StemLayout::Tracks => {
                export_stem_tracks(audio, song, track_names, &dir.join(stem), stems.format)
                    .map_err(|e| format!("Song {}: {e}", i + 1))?;
            }
        }
        written += 1;
    }
    Ok(format!("Exported {written} songs ({stems}) to {}.", dir.display()))
}

fn encode_flac(
    samples: &[i32],
    channels: usize,
    bits: usize,
    sample_rate: usize,
) -> Result<Vec<u8>, String> {
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|e| format!("Encoder config error: {e:?}"))?;
    let source = flacenc::source::MemSource::from_samples(samples, channels, bits, sample_rate);
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|e| format!("FLAC encoding failed: {e:?}"))?;
    let mut sink = ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|e| format!("FLAC serialization failed: {e:?}"))?;

    // flacenc records the (shorter) final block in STREAMINFO's min_blocksize,
    // but the FLAC spec excludes the last block: a fixed-blocksize stream must
    // have min == max, and strict decoders (e.g. symphonia) reject the frames
    // otherwise. Patch min_blocksize (bytes 8..10) to max_blocksize (10..12).
    let mut bytes = sink.as_slice().to_vec();
    if bytes.len() > 12 && &bytes[0..4] == b"fLaC" {
        let (min_bs, max_bs) = bytes.split_at_mut(10);
        min_bs[8..10].copy_from_slice(&max_bs[..2]);
    }
    Ok(bytes)
}

/// Sample payload for `encode_wav`: little-endian integers of the given bit
/// depth, or IEEE 32-bit floats.
enum WavSamples<'a> {
    Int(&'a [i32]),
    Float(&'a [f32]),
}

/// WAV writer. WAVE_FORMAT_EXTENSIBLE is used for more than two channels
/// (what DAWs expect for multichannel files); otherwise plain PCM or plain
/// IEEE float.
fn encode_wav(
    samples: &WavSamples<'_>,
    channels: usize,
    bits: u32,
    sample_rate: u32,
) -> Result<Vec<u8>, String> {
    let (count, is_float) = match samples {
        WavSamples::Int(s) => (s.len(), false),
        WavSamples::Float(s) => (s.len(), true),
    };
    let bytes_per_sample = (bits / 8) as usize;
    let data_len = count * bytes_per_sample;
    let extensible = channels > 2;
    let fmt_len: u32 = if extensible { 40 } else { 16 };
    let riff_len = 4 + (8 + fmt_len as usize) + (8 + data_len);
    if riff_len > u32::MAX as usize {
        return Err("Output exceeds the 4 GB WAV limit; export as FLAC or shorten the song".into());
    }
    let block_align = (channels * bytes_per_sample) as u16;
    let byte_rate = sample_rate * block_align as u32;
    let format_tag: u16 = match (extensible, is_float) {
        (true, _) => 0xFFFE,
        (false, false) => 1,
        (false, true) => 3,
    };

    let mut out = Vec::with_capacity(riff_len + 8);
    let w = &mut out;
    let write = |w: &mut Vec<u8>, b: &[u8]| w.write_all(b).unwrap();
    write(w, b"RIFF");
    write(w, &(riff_len as u32).to_le_bytes());
    write(w, b"WAVE");
    write(w, b"fmt ");
    write(w, &fmt_len.to_le_bytes());
    write(w, &format_tag.to_le_bytes());
    write(w, &(channels as u16).to_le_bytes());
    write(w, &sample_rate.to_le_bytes());
    write(w, &byte_rate.to_le_bytes());
    write(w, &block_align.to_le_bytes());
    write(w, &(bits as u16).to_le_bytes());
    if extensible {
        write(w, &22u16.to_le_bytes()); // cbSize
        write(w, &(bits as u16).to_le_bytes()); // valid bits per sample
        // Channel mask: one bit per channel, low bits first. Multitrack files
        // have no speaker positions; this just keeps readers happy.
        let mask: u32 = if channels >= 32 { u32::MAX } else { (1u32 << channels) - 1 };
        write(w, &mask.to_le_bytes());
        // KSDATAFORMAT_SUBTYPE_PCM / KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: they
        // differ only in the first byte.
        write(
            w,
            &[
                if is_float { 0x03 } else { 0x01 },
                0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38,
                0x9b, 0x71,
            ],
        );
    }
    write(w, b"data");
    write(w, &(data_len as u32).to_le_bytes());
    match samples {
        WavSamples::Int(s) => {
            for &v in *s {
                let b = v.to_le_bytes();
                write(w, &b[..bytes_per_sample]);
            }
        }
        WavSamples::Float(s) => {
            for &v in *s {
                write(w, &v.to_le_bytes());
            }
        }
    }
    Ok(out)
}
