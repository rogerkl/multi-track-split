//! Song export: a stereo mixdown (the simple mix, as heard in playback) or a
//! multitrack file holding only the tracks a song uses, each silent outside
//! its own start/end. Format is picked from the file extension: `.flac`
//! (up to 8 channels) or `.wav`.

use std::io::Write;
use std::path::Path;

use flacenc::bitsink::ByteSink;
use flacenc::component::BitRepr;
use flacenc::error::Verify;

use crate::audio::AudioData;
use crate::mix;
use crate::project::Song;

/// FLAC's channel limit (and flacenc's).
pub const FLAC_MAX_CHANNELS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// Interleaved stereo mixdown of the song plus the pre-clamp peak level.
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
    let peak = mix::render_stereo(audio, &mixes, song.start, end, &mut out);
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
        Format::Wav => encode_wav(&quantized, channels, bits, sample_rate)?,
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

pub fn export_mixdown(audio: &AudioData, song: &Song, path: &Path) -> Result<String, String> {
    let (samples, peak) = render_mixdown(audio, song)?;
    write_audio(path, &samples, 2, audio.bits_per_sample, audio.sample_rate)?;
    let clip_note = if peak > 1.0 {
        format!(" Warning: mix peaked at {:+.1} dB and was clipped.", 20.0 * peak.log10())
    } else {
        String::new()
    };
    Ok(format!("Wrote {}.{clip_note}", path.display()))
}

/// Exports every song as a multitrack file into `dir`, named `NN - Title`.
/// Songs with more active tracks than FLAC allows fall back to WAV.
pub fn export_all_multitrack(
    audio: &AudioData,
    songs: &[Song],
    dir: &Path,
    format: Format,
) -> Result<String, String> {
    let mut written = 0;
    for (i, song) in songs.iter().enumerate() {
        let format = if format == Format::Flac && song.active_channels().len() > FLAC_MAX_CHANNELS
        {
            Format::Wav
        } else {
            format
        };
        let path = dir.join(format!("{}.{}", song_file_stem(i + 1, song), format.extension()));
        export_multitrack(audio, song, &path).map_err(|e| format!("Song {}: {e}", i + 1))?;
        written += 1;
    }
    Ok(format!("Exported {written} multitrack files to {}.", dir.display()))
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

/// PCM WAV; WAVE_FORMAT_EXTENSIBLE for more than two channels (what DAWs
/// expect for multichannel files), plain WAVE_FORMAT_PCM otherwise.
fn encode_wav(
    samples: &[i32],
    channels: usize,
    bits: u32,
    sample_rate: u32,
) -> Result<Vec<u8>, String> {
    let bytes_per_sample = (bits / 8) as usize;
    let data_len = samples.len() * bytes_per_sample;
    let extensible = channels > 2;
    let fmt_len: u32 = if extensible { 40 } else { 16 };
    let riff_len = 4 + (8 + fmt_len as usize) + (8 + data_len);
    if riff_len > u32::MAX as usize {
        return Err("Output exceeds the 4 GB WAV limit; export as FLAC or shorten the song".into());
    }
    let block_align = (channels * bytes_per_sample) as u16;
    let byte_rate = sample_rate * block_align as u32;

    let mut out = Vec::with_capacity(riff_len + 8);
    let w = &mut out;
    let write = |w: &mut Vec<u8>, b: &[u8]| w.write_all(b).unwrap();
    write(w, b"RIFF");
    write(w, &(riff_len as u32).to_le_bytes());
    write(w, b"WAVE");
    write(w, b"fmt ");
    write(w, &fmt_len.to_le_bytes());
    write(w, &(if extensible { 0xFFFEu16 } else { 1u16 }).to_le_bytes());
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
        // KSDATAFORMAT_SUBTYPE_PCM
        write(
            w,
            &[
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38,
                0x9b, 0x71,
            ],
        );
    }
    write(w, b"data");
    write(w, &(data_len as u32).to_le_bytes());
    for &s in samples {
        let b = s.to_le_bytes();
        write(w, &b[..bytes_per_sample]);
    }
    Ok(out)
}
