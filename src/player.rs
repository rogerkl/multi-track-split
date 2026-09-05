//! Playback of the live stereo mix on a dedicated thread. The mix parameters
//! live behind a shared lock so volume, pan, mute and solo changes take effect
//! while playing, without restarting the stream.

use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rodio::cpal::traits::{DeviceTrait, HostTrait};

use crate::audio::AudioData;
use crate::mix::{TrackMix, render_stereo};

/// Shared, live-updatable mix parameters.
pub type LiveMix = Arc<RwLock<Vec<TrackMix>>>;

enum Cmd {
    Play {
        audio: Arc<AudioData>,
        mix: LiveMix,
        start_frame: usize,
        end_frame: usize,
    },
    Stop,
    SetDevice(Option<String>),
}

/// Names of the available audio output devices.
pub fn list_output_devices() -> Vec<String> {
    rodio::cpal::default_host()
        .output_devices()
        .map(|devices| devices.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

fn open_stream(
    device_name: &Option<String>,
) -> Option<(rodio::OutputStream, rodio::OutputStreamHandle)> {
    if let Some(name) = device_name {
        let device = rodio::cpal::default_host()
            .output_devices()
            .ok()?
            .find(|d| d.name().map(|n| n == *name).unwrap_or(false));
        if let Some(device) = device {
            if let Ok(s) = rodio::OutputStream::try_from_device(&device) {
                return Some(s);
            }
            eprintln!("Cannot open audio device {name:?}, falling back to default");
        } else {
            eprintln!("Audio device {name:?} not found, falling back to default");
        }
    }
    rodio::OutputStream::try_default().ok()
}

/// The rodio output stream is not Send, so it lives entirely inside the
/// playback thread and is driven by commands.
pub struct Player {
    tx: Sender<Cmd>,
}

impl Player {
    pub fn new() -> Self {
        let (tx, rx) = channel::<Cmd>();
        std::thread::spawn(move || {
            let mut device_name: Option<String> = None;
            let mut stream = open_stream(&device_name);
            let mut sink: Option<rodio::Sink> = None;
            while let Ok(cmd) = rx.recv() {
                match cmd {
                    Cmd::Play {
                        audio,
                        mix,
                        start_frame,
                        end_frame,
                    } => {
                        if let Some(s) = sink.take() {
                            s.stop();
                        }
                        if stream.is_none() {
                            stream = open_stream(&device_name);
                        }
                        if let Some((_, handle)) = &stream {
                            if let Ok(s) = rodio::Sink::try_new(handle) {
                                s.append(MixSource::new(audio, mix, start_frame, end_frame));
                                sink = Some(s);
                            }
                        }
                    }
                    Cmd::Stop => {
                        if let Some(s) = sink.take() {
                            s.stop();
                        }
                    }
                    Cmd::SetDevice(name) => {
                        if let Some(s) = sink.take() {
                            s.stop();
                        }
                        device_name = name;
                        // Drop the old stream before opening the new one so
                        // exclusive backends release the device first.
                        drop(stream.take());
                        stream = open_stream(&device_name);
                    }
                }
            }
        });
        Self { tx }
    }

    /// Plays frames `[start_frame, end_frame)` of the mix described by `mix`.
    pub fn play(&self, audio: Arc<AudioData>, mix: LiveMix, start_frame: usize, end_frame: usize) {
        let _ = self.tx.send(Cmd::Play {
            audio,
            mix,
            start_frame,
            end_frame,
        });
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }

    /// `None` selects the system default device.
    pub fn set_device(&self, name: Option<String>) {
        let _ = self.tx.send(Cmd::SetDevice(name));
    }
}

/// Frames rendered per lock acquisition; ~6 ms at 44.1 kHz, so mixer changes
/// are audible almost immediately while the lock is taken rarely.
const BLOCK_FRAMES: usize = 256;

struct MixSource {
    audio: Arc<AudioData>,
    mix: LiveMix,
    frame: usize,
    end: usize,
    buf: Vec<f32>,
    pos: usize,
}

impl MixSource {
    fn new(audio: Arc<AudioData>, mix: LiveMix, start: usize, end: usize) -> Self {
        let end = end.min(audio.frames());
        Self {
            audio,
            mix,
            frame: start.min(end),
            end,
            buf: Vec::with_capacity(2 * BLOCK_FRAMES),
            pos: 0,
        }
    }

    fn refill(&mut self) -> bool {
        if self.frame >= self.end {
            return false;
        }
        let to = (self.frame + BLOCK_FRAMES).min(self.end);
        let mixes = self.mix.read().map(|m| m.clone()).unwrap_or_default();
        self.buf.clear();
        render_stereo(&self.audio, &mixes, self.frame, to, true, &mut self.buf);
        self.frame = to;
        self.pos = 0;
        true
    }
}

impl Iterator for MixSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.pos >= self.buf.len() && !self.refill() {
            return None;
        }
        let s = self.buf[self.pos];
        self.pos += 1;
        Some(s)
    }
}

impl rodio::Source for MixSource {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        2
    }

    fn sample_rate(&self) -> u32 {
        self.audio.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}
