//! The "very simple mix": per-track volume and pan summed to stereo. Shared
//! by live playback and the mixdown export so both sound identical.

use crate::audio::AudioData;
use crate::project::Song;

/// Everything the mixer needs to know about one source track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackMix {
    /// Source channel index in the recording.
    pub channel: usize,
    pub gain_l: f32,
    pub gain_r: f32,
    /// Absolute frame range in which this track is audible.
    pub start: usize,
    pub end: usize,
}

/// Constant-power pan: -1 = hard left, 0 = center (-3 dB per side), 1 = hard right.
pub fn pan_gains(pan: f32) -> (f32, f32) {
    let angle = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    (angle.cos(), angle.sin())
}

/// Mixing parameters for a song. Inactive, muted and (when anything is
/// soloed) non-soloed tracks are left out entirely.
pub fn song_mixes(song: &Song) -> Vec<TrackMix> {
    let any_solo = song.tracks.iter().any(|t| t.solo);
    song.tracks
        .iter()
        .enumerate()
        .filter_map(|(ch, t)| {
            let (start, end) = song.track_range(ch)?;
            if t.mute || (any_solo && !t.solo) {
                return None;
            }
            let (l, r) = pan_gains(t.pan);
            Some(TrackMix {
                channel: ch,
                gain_l: l * t.volume,
                gain_r: r * t.volume,
                start,
                end,
            })
        })
        .collect()
}

/// Every channel centered over the whole recording, scaled by 1/sqrt(n) so
/// a full tape does not clip outright. Used when no song is selected so the
/// raw tape can still be auditioned.
pub fn flat_mixes(channels: usize, frames: usize) -> Vec<TrackMix> {
    let (l, r) = pan_gains(0.0);
    let scale = 1.0 / (channels.max(1) as f32).sqrt();
    let (l, r) = (l * scale, r * scale);
    (0..channels)
        .map(|channel| TrackMix {
            channel,
            gain_l: l,
            gain_r: r,
            start: 0,
            end: frames,
        })
        .collect()
}

/// Renders frames `[from, to)` of the stereo mix and appends them to `out`
/// as interleaved L/R, clamped to [-1, 1]. Returns the peak absolute level
/// before clamping so callers can report clipping.
pub fn render_stereo(
    audio: &AudioData,
    mixes: &[TrackMix],
    from: usize,
    to: usize,
    out: &mut Vec<f32>,
) -> f32 {
    let to = to.min(audio.frames());
    if to <= from {
        return 0.0;
    }
    let base = out.len();
    out.resize(base + 2 * (to - from), 0.0);
    let block = &mut out[base..];

    for m in mixes {
        let s = m.start.max(from);
        let e = m.end.min(to);
        if e <= s || m.channel >= audio.channels() {
            continue;
        }
        let src = &audio.tracks[m.channel][s..e];
        let dst = &mut block[2 * (s - from)..2 * (e - from)];
        for (pair, &v) in dst.chunks_exact_mut(2).zip(src) {
            pair[0] += v * m.gain_l;
            pair[1] += v * m.gain_r;
        }
    }

    let mut peak = 0.0f32;
    for v in block.iter_mut() {
        peak = peak.max(v.abs());
        *v = v.clamp(-1.0, 1.0);
    }
    peak
}

pub fn volume_to_db_label(volume: f32) -> String {
    if volume <= 0.0005 {
        "-inf dB".to_string()
    } else {
        format!("{:+.1} dB", 20.0 * volume.log10())
    }
}

pub fn pan_label(pan: f32) -> String {
    let pct = (pan.abs() * 100.0).round() as i32;
    if pct == 0 {
        "C".to_string()
    } else if pan < 0.0 {
        format!("L{pct}")
    } else {
        format!("R{pct}")
    }
}
