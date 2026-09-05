# Multi Track Split

A small desktop tool for organizing backups of multitrack tape recordings (4-, 8- or more-track interleaved WAV/FLAC) into songs, auditioning them with a simple mix, and exporting each song for remixing.

Tape was expensive, so songs often overlap: the end of one song may only use tracks 1–4 while the next one already starts on tracks 5–8. A plain start/end split cannot express that. Here a **song** is a time range on the tape plus, for each tape track:

- whether the song **uses** that track at all,
- an optional **track start / end** inside the song (e.g. "this track only belongs to the song from 0:20 on"),
- a **volume** and **pan** for the quick stereo mix, with mute/solo for auditioning.

## Workflow

1. **Open…** a multitrack WAV or FLAC, or select several mono/stereo files at once: they become tracks 1–n in file-name order (numbers sort numerically), all assumed to start at the same instant, shorter files padded with silence to the longest. A project file saved next to the recording (`<name>.mtsplit.json`, or `<folder>.mtsplit.json` for a set of files) is loaded automatically.
2. Click in the waveform to place the playhead, press **N** (or *+ Song at playhead*) to add a song. Drag the orange flags in the strip, press **I** / **O** to set the song start / end at the playhead, or type times in the song list (Enter applies).
3. In the mixer for the selected song, untick tracks the song does not use, and set per-track *In* / *Out* where a track joins late or leaves early (type a time, press *PH* to use the playhead, or drag the track's edge in its lane). Double-clicking a lane toggles the track.
4. **Play** auditions the selected song with its mix; volume, pan, mute and solo react live.
5. **Save project** writes the JSON sidecar. **Export mix…** writes the song's stereo mix in the format chosen in the *Mix as* list: 32-bit float WAV (nothing scaled, so the mix never clips no matter how hot the faders are), or a normalized 16- or 24-bit WAV/FLAC, where the whole mix is scaled so its peak sits at -1 dBFS. The choice is stored in the project. **Export multitrack…** writes a FLAC/WAV containing only the tracks the song uses, in tape order, each silent outside its own in/out range, so a track that joins 20 seconds in is silent for its first 20 seconds. **Export all multitrack…** writes one multitrack file per song into a folder (`NN - Title.flac`, WAV when a song uses more than 8 tracks, FLAC's limit); **Export all mixes…** does the same for the mixes (`NN - Title (mix).wav` / `.flac`) in the chosen mix format.

Multitrack exports keep the recording's sample rate and bit depth (16–24 bit).

## Shortcuts

Space play/pause · ←/→ step · N new song · I/O song start/end at playhead · +/- zoom · Shift +/- amplitude zoom · wheel zoom · Shift+wheel pan. Shortcuts are inactive while a text field is focused.

## Building

```sh
cargo build --release          # target/release/multi-track-split
cargo test
```

Linux needs ALSA headers (`sudo apt install libasound2-dev pkg-config`). Windows cross-build from Linux with `mingw-w64` installed:

```sh
cargo build --release --target x86_64-pc-windows-gnu
```

The whole recording is decoded into memory as 32-bit float: an 8-track, 24-bit, 48 kHz, 45-minute tape takes about 4 GB of RAM.
