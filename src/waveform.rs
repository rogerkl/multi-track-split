//! The tape view: a time ruler and song strip on top, then one waveform lane
//! per tape track. For the selected song each lane shows the part of the
//! song that track is used for; the edges can be dragged to set per-track
//! start/end overrides, and the song's own boundaries are dragged in the strip.

use std::time::Instant;

use iced::keyboard;
use iced::mouse;
use iced::widget::canvas::{self, Event, Geometry, Path, Stroke, Text};
use iced::{Color, Pixels, Point, Rectangle, Renderer, Size, Theme};

use crate::Message;
use crate::audio::{AudioData, PEAK_BIN, format_time};
use crate::project::Song;

/// Height of the ruler + song strip at the top of the canvas.
pub const STRIP_H: f32 = 46.0;
/// Where the song bands start inside the strip (the ruler sits above).
const BAND_TOP: f32 = 18.0;
/// Grab distance for draggable edges, in pixels.
const HIT_PX: f32 = 7.0;

pub const MIN_FPP: f64 = 0.02;

#[derive(Clone, Copy)]
pub struct ViewParams {
    /// First visible sample frame (can be fractional while zooming).
    pub offset: f64,
    /// Frames per pixel (zoom level).
    pub fpp: f64,
}

impl Default for ViewParams {
    fn default() -> Self {
        Self {
            offset: 0.0,
            fpp: 1024.0,
        }
    }
}

impl ViewParams {
    pub fn frame_at(&self, x: f32) -> f64 {
        self.offset + x as f64 * self.fpp
    }

    pub fn x_of(&self, frame: f64) -> f32 {
        ((frame - self.offset) / self.fpp) as f32
    }
}

#[derive(Clone, Copy, Debug)]
enum Drag {
    SongStart(u64),
    SongEnd(u64),
    TrackStart(u64, usize),
    TrackEnd(u64, usize),
}

#[derive(Default)]
pub struct WfState {
    dragging: Option<Drag>,
    scrubbing: bool,
    shift: bool,
    last_click: Option<(Instant, Point)>,
}

pub struct WaveformProgram<'a> {
    pub audio: &'a AudioData,
    pub songs: &'a [Song],
    pub selected: Option<u64>,
    pub track_names: &'a [String],
    pub view: ViewParams,
    /// Vertical (amplitude) zoom factor; peaks are clamped to the lane.
    pub v_zoom: f32,
    pub playhead: usize,
    pub cache: &'a canvas::Cache,
}

impl WaveformProgram<'_> {
    fn clamp_frame(&self, frame: f64) -> usize {
        frame.max(0.0).min(self.audio.frames() as f64) as usize
    }

    fn selected_song(&self) -> Option<&Song> {
        self.songs.iter().find(|s| Some(s.id) == self.selected)
    }

    fn lane_geometry(&self, height: f32) -> (f32, f32) {
        let channels = self.audio.channels().max(1);
        (STRIP_H, (height - STRIP_H) / channels as f32)
    }

    fn lane_at(&self, y: f32, height: f32) -> Option<usize> {
        let (top, lane_h) = self.lane_geometry(height);
        if y < top || lane_h <= 0.0 {
            return None;
        }
        let ch = ((y - top) / lane_h) as usize;
        (ch < self.audio.channels()).then_some(ch)
    }

    /// Nearest of the selected song's start/end flags within grab distance.
    fn hit_song_edge(&self, x: f32) -> Option<Drag> {
        let song = self.selected_song()?;
        let ds = (self.view.x_of(song.start as f64) - x).abs();
        let de = (self.view.x_of(song.end as f64) - x).abs();
        if ds <= HIT_PX && ds <= de {
            Some(Drag::SongStart(song.id))
        } else if de <= HIT_PX {
            Some(Drag::SongEnd(song.id))
        } else {
            None
        }
    }

    /// A song band under `x`, preferring one that is not already selected.
    fn hit_song_band(&self, x: f32) -> Option<u64> {
        let frame = self.view.frame_at(x);
        let mut fallback = None;
        for s in self.songs {
            if frame >= s.start as f64 && frame <= s.end as f64 {
                if Some(s.id) != self.selected {
                    return Some(s.id);
                }
                fallback = Some(s.id);
            }
        }
        fallback
    }

    /// A per-track range edge of the selected song in the lane under `y`.
    fn hit_track_edge(&self, x: f32, y: f32, height: f32) -> Option<Drag> {
        let song = self.selected_song()?;
        let ch = self.lane_at(y, height)?;
        let (start, end) = song.track_range(ch)?;
        let ds = (self.view.x_of(start as f64) - x).abs();
        let de = (self.view.x_of(end as f64) - x).abs();
        if ds <= HIT_PX && ds <= de {
            Some(Drag::TrackStart(song.id, ch))
        } else if de <= HIT_PX {
            Some(Drag::TrackEnd(song.id, ch))
        } else {
            None
        }
    }

    fn drag_message(&self, drag: Drag, frame: usize) -> Option<Message> {
        match drag {
            Drag::SongStart(id) => Some(Message::MoveSongStart(id, frame)),
            Drag::SongEnd(id) => Some(Message::MoveSongEnd(id, frame)),
            Drag::TrackStart(id, ch) => {
                let song = self.songs.iter().find(|s| s.id == id)?;
                // Dragged back onto the song boundary: the override is gone.
                let value = (frame > song.start).then_some(frame.min(song.end));
                Some(Message::TrackStart(id, ch, value))
            }
            Drag::TrackEnd(id, ch) => {
                let song = self.songs.iter().find(|s| s.id == id)?;
                let value = (frame < song.end).then_some(frame.max(song.start));
                Some(Message::TrackEnd(id, ch, value))
            }
        }
    }
}

impl canvas::Program<Message> for WaveformProgram<'_> {
    type State = WfState;

    fn update(
        &self,
        state: &mut WfState,
        event: Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
        use canvas::event::Status;

        if let Event::Keyboard(keyboard::Event::ModifiersChanged(mods)) = event {
            state.shift = mods.shift();
            return (Status::Ignored, None);
        }

        let Event::Mouse(mouse_event) = event else {
            return (Status::Ignored, None);
        };

        let Some(pos) = cursor.position_in(bounds) else {
            if let mouse::Event::ButtonReleased(_) = mouse_event {
                state.dragging = None;
                state.scrubbing = false;
            }
            return (Status::Ignored, None);
        };

        match mouse_event {
            mouse::Event::WheelScrolled { delta } => {
                let (dx, dy) = match delta {
                    mouse::ScrollDelta::Lines { x, y } => (x * 40.0, y * 40.0),
                    mouse::ScrollDelta::Pixels { x, y } => (x, y),
                };
                let msg = if state.shift || dx.abs() > dy.abs() {
                    // Pan horizontally.
                    let d = if dx.abs() > dy.abs() { dx } else { dy };
                    Message::ViewChanged {
                        offset: self.view.offset - d as f64 * self.view.fpp,
                        fpp: self.view.fpp,
                    }
                } else {
                    // Zoom around the cursor position.
                    let factor = if dy > 0.0 { 1.0 / 1.3 } else { 1.3 };
                    let max_fpp =
                        (self.audio.frames() as f64 / bounds.width.max(1.0) as f64).max(MIN_FPP);
                    let new_fpp = (self.view.fpp * factor).clamp(MIN_FPP, max_fpp * 1.1);
                    let anchor = self.view.frame_at(pos.x);
                    Message::ViewChanged {
                        offset: anchor - pos.x as f64 * new_fpp,
                        fpp: new_fpp,
                    }
                };
                (Status::Captured, Some(msg))
            }
            mouse::Event::ButtonPressed(mouse::Button::Left) => {
                let frame = self.clamp_frame(self.view.frame_at(pos.x));
                if pos.y <= STRIP_H {
                    if let Some(drag) = self.hit_song_edge(pos.x) {
                        state.dragging = Some(drag);
                        return (Status::Captured, None);
                    }
                    if pos.y >= BAND_TOP {
                        if let Some(id) = self.hit_song_band(pos.x) {
                            if Some(id) != self.selected {
                                return (Status::Captured, Some(Message::SelectSong(Some(id))));
                            }
                        }
                    }
                    (Status::Captured, Some(Message::SetPlayhead(frame)))
                } else {
                    if let Some(drag) = self.hit_track_edge(pos.x, pos.y, bounds.height) {
                        state.dragging = Some(drag);
                        return (Status::Captured, None);
                    }
                    let is_double = state
                        .last_click
                        .map(|(t, p)| t.elapsed().as_millis() < 400 && p.distance(pos) < 6.0)
                        .unwrap_or(false);
                    state.last_click = Some((Instant::now(), pos));
                    if is_double {
                        if let (Some(song), Some(ch)) =
                            (self.selected_song(), self.lane_at(pos.y, bounds.height))
                        {
                            return (
                                Status::Captured,
                                Some(Message::ToggleTrackActive(song.id, ch)),
                            );
                        }
                    }
                    state.scrubbing = true;
                    (Status::Captured, Some(Message::SetPlayhead(frame)))
                }
            }
            mouse::Event::CursorMoved { .. } => {
                let frame = self.clamp_frame(self.view.frame_at(pos.x));
                if let Some(drag) = state.dragging {
                    (Status::Captured, self.drag_message(drag, frame))
                } else if state.scrubbing {
                    (Status::Captured, Some(Message::SetPlayhead(frame)))
                } else {
                    (Status::Ignored, None)
                }
            }
            mouse::Event::ButtonReleased(_) => {
                state.dragging = None;
                state.scrubbing = false;
                (Status::Ignored, None)
            }
            _ => (Status::Ignored, None),
        }
    }

    fn draw(
        &self,
        _state: &WfState,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let waveform = self.cache.draw(renderer, bounds.size(), |frame| {
            self.draw_static(frame, bounds.size());
        });

        // Playhead + cursor readout are drawn uncached so playback stays cheap.
        let mut overlay = canvas::Frame::new(renderer, bounds.size());
        let px = self.view.x_of(self.playhead as f64);
        if px >= 0.0 && px <= bounds.width {
            overlay.stroke(
                &Path::line(Point::new(px, 0.0), Point::new(px, bounds.height)),
                Stroke::default()
                    .with_color(Color::from_rgb(0.30, 0.85, 0.40))
                    .with_width(1.5),
            );
        }
        if let Some(pos) = cursor.position_in(bounds) {
            let secs = self.view.frame_at(pos.x).max(0.0) / self.audio.sample_rate as f64;
            overlay.fill_text(Text {
                content: format_time(secs),
                position: Point::new(bounds.width - 8.0, bounds.height - 6.0),
                color: Color::from_rgba(1.0, 1.0, 1.0, 0.7),
                size: Pixels(12.0),
                horizontal_alignment: iced::alignment::Horizontal::Right,
                vertical_alignment: iced::alignment::Vertical::Bottom,
                ..Text::default()
            });
        }

        vec![waveform, overlay.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &WfState,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if let Some(drag) = state.dragging {
            return match drag {
                Drag::SongStart(_) | Drag::SongEnd(_) => mouse::Interaction::Grabbing,
                _ => mouse::Interaction::ResizingHorizontally,
            };
        }
        if let Some(pos) = cursor.position_in(bounds) {
            if pos.y <= STRIP_H {
                if self.hit_song_edge(pos.x).is_some() {
                    return mouse::Interaction::Grab;
                }
                if pos.y >= BAND_TOP
                    && self
                        .hit_song_band(pos.x)
                        .is_some_and(|id| Some(id) != self.selected)
                {
                    return mouse::Interaction::Pointer;
                }
            } else if self.hit_track_edge(pos.x, pos.y, bounds.height).is_some() {
                return mouse::Interaction::ResizingHorizontally;
            }
        }
        mouse::Interaction::default()
    }
}

impl WaveformProgram<'_> {
    fn draw_static(&self, frame: &mut canvas::Frame, size: Size) {
        let bg = Color::from_rgb(0.10, 0.11, 0.13);
        let strip_bg = Color::from_rgb(0.15, 0.16, 0.19);
        let wave_color = Color::from_rgb(0.35, 0.60, 0.90);
        let wave_dim = Color::from_rgba(0.35, 0.60, 0.90, 0.45);
        let center_color = Color::from_rgba(1.0, 1.0, 1.0, 0.15);
        let lane_sep = Color::from_rgba(1.0, 1.0, 1.0, 0.08);
        let tick_color = Color::from_rgba(1.0, 1.0, 1.0, 0.22);
        let label_color = Color::from_rgba(1.0, 1.0, 1.0, 0.55);
        let song_color = Color::from_rgb(0.95, 0.60, 0.15);
        let other_song_color = Color::from_rgba(0.45, 0.55, 0.85, 0.55);
        let used_fill = Color::from_rgba(0.30, 0.85, 0.40, 0.13);
        let unused_fill = Color::from_rgba(0.0, 0.0, 0.0, 0.45);
        let inactive_fill = Color::from_rgba(0.90, 0.30, 0.30, 0.10);
        let edge_color = Color::from_rgb(0.95, 0.85, 0.30);

        frame.fill_rectangle(Point::ORIGIN, size, bg);
        frame.fill_rectangle(Point::ORIGIN, Size::new(size.width, STRIP_H), strip_bg);

        let channels = self.audio.channels().max(1);
        let (wave_top, lane_h) = self.lane_geometry(size.height);
        let frames_total = self.audio.frames();
        let sr = self.audio.sample_rate as f64;
        let selected = self.selected_song();

        // Time ruler ticks.
        let secs_per_px = self.view.fpp / sr;
        let target = secs_per_px * 90.0; // aim for a tick roughly every 90 px
        let intervals = [
            0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0,
            600.0, 1200.0, 1800.0,
        ];
        let tick = intervals
            .iter()
            .copied()
            .find(|&i| i >= target)
            .unwrap_or(3600.0);
        let start_secs = self.view.offset.max(0.0) / sr;
        let mut t = (start_secs / tick).floor() * tick;
        let end_secs = self.view.frame_at(size.width) / sr;
        while t <= end_secs {
            if t >= 0.0 {
                let x = self.view.x_of(t * sr);
                if x >= 0.0 && x <= size.width {
                    frame.stroke(
                        &Path::line(Point::new(x, BAND_TOP - 4.0), Point::new(x, size.height)),
                        Stroke::default().with_color(tick_color).with_width(1.0),
                    );
                    let label = if tick >= 1.0 {
                        let total = t.round() as u64;
                        format!("{}:{:02}", total / 60, total % 60)
                    } else {
                        format!("{:.2}", t)
                    };
                    frame.fill_text(Text {
                        content: label,
                        position: Point::new(x + 3.0, BAND_TOP - 4.0),
                        color: label_color,
                        size: Pixels(10.0),
                        vertical_alignment: iced::alignment::Vertical::Bottom,
                        ..Text::default()
                    });
                }
            }
            t += tick;
        }

        // Waveform lanes, one min/max column per pixel.
        for ch in 0..channels {
            let lane_top = wave_top + ch as f32 * lane_h;
            let mid = lane_top + lane_h / 2.0;
            let half = lane_h / 2.0 - 2.0;

            if ch > 0 {
                frame.stroke(
                    &Path::line(Point::new(0.0, lane_top), Point::new(size.width, lane_top)),
                    Stroke::default().with_color(lane_sep).with_width(1.0),
                );
            }
            frame.stroke(
                &Path::line(Point::new(0.0, mid), Point::new(size.width, mid)),
                Stroke::default().with_color(center_color).with_width(1.0),
            );

            // Outside the selected song the tape is drawn dimmed so the song
            // stands out; without a selection everything is full strength.
            let range = selected.and_then(|s| s.track_range(ch));
            let color = if selected.is_some() { wave_dim } else { wave_color };
            let path = Path::new(|builder| {
                for x in 0..size.width as usize {
                    let f0 = self.view.frame_at(x as f32);
                    let f1 = f0 + self.view.fpp;
                    if f1 < 0.0 || f0 >= frames_total as f64 {
                        continue;
                    }
                    let begin = f0.max(0.0) as usize;
                    let end = (f1.max(0.0) as usize).min(frames_total).max(begin + 1);
                    let (min, max) = self.min_max(ch, begin, end.min(frames_total));
                    let y0 = mid - (max * self.v_zoom).clamp(-1.0, 1.0) * half;
                    let y1 = mid - (min * self.v_zoom).clamp(-1.0, 1.0) * half;
                    builder.move_to(Point::new(x as f32 + 0.5, y0));
                    builder.line_to(Point::new(x as f32 + 0.5, y1.max(y0 + 0.5)));
                }
            });
            frame.stroke(&path, Stroke::default().with_color(color).with_width(1.0));

            if let Some(song) = selected {
                let sx0 = self.view.x_of(song.start as f64).clamp(-1.0, size.width + 1.0);
                let sx1 = self.view.x_of(song.end as f64).clamp(-1.0, size.width + 1.0);
                match range {
                    Some((start, end)) => {
                        let x0 = self.view.x_of(start as f64).clamp(-1.0, size.width + 1.0);
                        let x1 = self.view.x_of(end as f64).clamp(-1.0, size.width + 1.0);
                        // Song span the track does not cover: dark.
                        if x0 > sx0 {
                            frame.fill_rectangle(
                                Point::new(sx0, lane_top),
                                Size::new(x0 - sx0, lane_h),
                                unused_fill,
                            );
                        }
                        if sx1 > x1 {
                            frame.fill_rectangle(
                                Point::new(x1, lane_top),
                                Size::new(sx1 - x1, lane_h),
                                unused_fill,
                            );
                        }
                        // The part that is used: green, re-drawn at full
                        // strength on top of the dimmed tape.
                        frame.fill_rectangle(
                            Point::new(x0, lane_top),
                            Size::new((x1 - x0).max(0.0), lane_h),
                            used_fill,
                        );
                        let used = Path::new(|builder| {
                            let px0 = x0.max(0.0) as usize;
                            let px1 = x1.min(size.width) as usize;
                            for x in px0..px1 {
                                let f0 = self.view.frame_at(x as f32);
                                let f1 = f0 + self.view.fpp;
                                if f1 < 0.0 || f0 >= frames_total as f64 {
                                    continue;
                                }
                                let begin = f0.max(0.0) as usize;
                                let end = (f1.max(0.0) as usize).min(frames_total).max(begin + 1);
                                let (min, max) = self.min_max(ch, begin, end.min(frames_total));
                                let y0 = mid - (max * self.v_zoom).clamp(-1.0, 1.0) * half;
                                let y1 = mid - (min * self.v_zoom).clamp(-1.0, 1.0) * half;
                                builder.move_to(Point::new(x as f32 + 0.5, y0));
                                builder.line_to(Point::new(x as f32 + 0.5, y1.max(y0 + 0.5)));
                            }
                        });
                        frame.stroke(
                            &used,
                            Stroke::default().with_color(wave_color).with_width(1.0),
                        );
                        // Edges; overrides get a bolder line than the song's own boundary.
                        let track = &song.tracks[ch];
                        for (x, is_override) in
                            [(x0, track.start.is_some()), (x1, track.end.is_some())]
                        {
                            frame.stroke(
                                &Path::line(Point::new(x, lane_top), Point::new(x, lane_top + lane_h)),
                                Stroke::default()
                                    .with_color(if is_override { edge_color } else { song_color })
                                    .with_width(if is_override { 2.5 } else { 1.0 }),
                            );
                        }
                    }
                    None => {
                        frame.fill_rectangle(
                            Point::new(sx0, lane_top),
                            Size::new((sx1 - sx0).max(0.0), lane_h),
                            inactive_fill,
                        );
                        frame.fill_rectangle(
                            Point::new(sx0, lane_top),
                            Size::new((sx1 - sx0).max(0.0), lane_h),
                            unused_fill,
                        );
                    }
                }
            }

            // Lane label.
            let name = self
                .track_names
                .get(ch)
                .cloned()
                .unwrap_or_else(|| format!("Track {}", ch + 1));
            let label = match (selected, range) {
                (Some(_), None) => format!("{name}  (not used)"),
                _ => name,
            };
            frame.fill_text(Text {
                content: label,
                position: Point::new(6.0, lane_top + 3.0),
                color: label_color,
                size: Pixels(11.0),
                ..Text::default()
            });
        }

        // Song bands in the strip: other songs as thin bars, the selected one
        // as a tall band with start/end flags.
        for (i, s) in self.songs.iter().enumerate() {
            let is_selected = Some(s.id) == self.selected;
            let x0 = self.view.x_of(s.start as f64);
            let x1 = self.view.x_of(s.end as f64);
            if x1 < -30.0 || x0 > size.width + 30.0 {
                continue;
            }
            let cx0 = x0.max(0.0);
            let cx1 = x1.min(size.width);
            if is_selected {
                frame.fill_rectangle(
                    Point::new(cx0, BAND_TOP),
                    Size::new((cx1 - cx0).max(0.0), STRIP_H - BAND_TOP - 2.0),
                    Color::from_rgba(0.95, 0.60, 0.15, 0.35),
                );
                for x in [x0, x1] {
                    frame.stroke(
                        &Path::line(Point::new(x, BAND_TOP), Point::new(x, size.height)),
                        Stroke::default().with_color(song_color).with_width(1.5),
                    );
                }
                frame.fill_rectangle(
                    Point::new(x0, BAND_TOP),
                    Size::new(6.0, STRIP_H - BAND_TOP - 2.0),
                    song_color,
                );
                frame.fill_rectangle(
                    Point::new(x1 - 6.0, BAND_TOP),
                    Size::new(6.0, STRIP_H - BAND_TOP - 2.0),
                    song_color,
                );
            } else {
                frame.fill_rectangle(
                    Point::new(cx0, STRIP_H - 12.0),
                    Size::new((cx1 - cx0).max(0.0), 10.0),
                    other_song_color,
                );
            }
            if cx1 - cx0 > 40.0 {
                let title = if s.title.is_empty() {
                    format!("{:02}", i + 1)
                } else {
                    format!("{:02} {}", i + 1, s.title)
                };
                frame.fill_text(Text {
                    content: title,
                    position: Point::new(cx0 + 9.0, if is_selected { BAND_TOP + 3.0 } else { STRIP_H - 12.0 }),
                    color: if is_selected {
                        Color::WHITE
                    } else {
                        Color::from_rgba(1.0, 1.0, 1.0, 0.75)
                    },
                    size: Pixels(if is_selected { 12.0 } else { 9.0 }),
                    ..Text::default()
                });
            }
        }
    }

    /// Min/max of samples in [begin, end) for one channel, using the peak
    /// pyramid when the range is large and raw samples when zoomed in.
    fn min_max(&self, ch: usize, begin: usize, end: usize) -> (f32, f32) {
        if end <= begin {
            return (0.0, 0.0);
        }
        if end - begin >= PEAK_BIN {
            let b0 = begin / PEAK_BIN;
            let b1 = (end / PEAK_BIN).min(self.audio.peaks[ch].len());
            let mut min = f32::MAX;
            let mut max = f32::MIN;
            for &(lo, hi) in &self.audio.peaks[ch][b0..b1.max(b0 + 1)] {
                min = min.min(lo);
                max = max.max(hi);
            }
            (min, max)
        } else {
            let track = &self.audio.tracks[ch];
            track[begin..end]
                .iter()
                .fold((f32::MAX, f32::MIN), |(lo, hi), &s| (lo.min(s), hi.max(s)))
        }
    }
}
