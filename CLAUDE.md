# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A desktop GUI tool (Rust + [iced](https://iced.rs) 0.13) for organizing multitrack tape transfers (interleaved 4/8/N-channel WAV or FLAC) into songs. Songs are time ranges on the tape; because tape was reused, songs may overlap, so each song also records *which* tape tracks it uses and may narrow a track's start/end inside the song. Songs get a simple per-track volume/pan mix for auditioning, and can be exported as a stereo mixdown or as a multitrack file containing only the tracks they use. It is a sibling of `../audio-split` and shares its architecture.

## Commands

```sh
cargo build --release   # binary at target/release/multi-track-split
cargo test              # end-to-end test in src/tests.rs (synthesizes a 4-track WAV)
cargo test <name>       # run a single test
```

Linux needs ALSA headers: `sudo apt install libasound2-dev pkg-config`.

Windows cross-build from Linux (needs `mingw-w64`; linker configured in `.cargo/config.toml`):

```sh
cargo build --release --target x86_64-pc-windows-gnu
```

## Architecture

Single-crate iced application. `src/main.rs` holds the `App` state struct, the `Message` enum, and the iced `update`/`view`/`subscription` trio; support modules are wired in through `Message` variants:

- `audio.rs` — decodes WAV/FLAC via symphonia into `AudioData`: the **entire file in memory** as **planar** f32 (`tracks[channel][frame]`, normalized to [-1, 1]) plus per-channel waveform peak bins (`PEAK_BIN` = 512). Also `format_time`/`parse_time` (`[[h:]mm:]ss[.mmm]`). Shared as `Arc<AudioData>`.
- `project.rs` — the data model and its JSON sidecar file (`<stem>.mtsplit.json`, saved next to the recording and auto-loaded when the recording is opened). `Song { start, end, tracks: Vec<TrackSettings> }`; `TrackSettings { active, start, end (absolute-frame overrides), volume, pan, mute, solo }`. `Song::track_range(ch)` is the single source of truth for a track's effective range (song range narrowed by overrides, `None` when inactive).
- `mix.rs` — the "simple mix": `song_mixes(song)` → `Vec<TrackMix>` (constant-power pan × volume, honoring mute/solo/active), and `render_stereo` which sums a frame range to interleaved stereo. Used by both live playback and the mixdown export so they sound identical.
- `player.rs` — playback thread driven by an mpsc `Cmd` channel (rodio/cpal). `MixSource` renders 256-frame blocks, re-reading the mix parameters from a shared `Arc<RwLock<Vec<TrackMix>>>` (`LiveMix`) each block, so fader moves are heard without restarting playback. The UI polls position via a 33 ms tick subscription.
- `export.rs` — `render_multitrack` (active tracks only, zero outside each track's range), `render_mixdown`, and `write_audio` which picks FLAC (flacenc, ≤ 8 channels) or WAV (hand-written RIFF, WAVE_FORMAT_EXTENSIBLE above 2 channels) from the extension. Bit depth follows the source, clamped to 16–24.
- `waveform.rs` — `canvas::Program` drawing the ruler + song strip and one lane per tape track. For the selected song it shades each lane's used/unused span and lets the user drag song flags (strip) and per-track edges (lanes); double-click in a lane toggles the track. `ViewParams` (offset + frames-per-pixel) is the zoom/pan viewport.
- `tests.rs` — end-to-end: synthesizes a 4-track WAV, exports multitrack (FLAC + WAV) and mixdown, decodes them back and checks channel counts, lengths and where silence lands; plus project JSON round trip, time parsing, pan law.

Positions are **absolute sample frames** (`usize`) everywhere, including per-track overrides; conversion to seconds happens only at the UI edges.

Keyboard shortcuts come from `iced::event::listen_with` and only fire when the event status is `Ignored`, i.e. no text input is focused. Time text fields are edited as a draft (`App::draft`) and committed on Enter via `TimeCommit`.

Long-running work (loading, export) runs through `Task::perform`; heavy data crosses task boundaries as `Arc<AudioData>`.
