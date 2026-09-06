use std::path::Path;

use crate::audio::{self, parse_time};
use crate::export::{self, Format, MixFormat, StemFormat, StemLayout};
use crate::mix;
use crate::project::{self, Project, Song};

const SR: u32 = 44100;
const TONES: [f32; 4] = [220.0, 330.0, 440.0, 550.0];

/// A 30 s four-track WAV: each track carries its own continuous sine tone.
fn make_test_tape(dir: &Path) -> std::path::PathBuf {
    let frames = 30 * SR as usize;
    let mut samples = Vec::with_capacity(frames * TONES.len());
    for i in 0..frames {
        let t = i as f32 / SR as f32;
        for &freq in &TONES {
            samples.push(0.5 * (2.0 * std::f32::consts::PI * freq * t).sin());
        }
    }
    let path = dir.join("tape.wav");
    export::write_audio(&path, &samples, TONES.len(), 16, SR).unwrap();
    path
}

fn rms(track: &[f32]) -> f32 {
    (track.iter().map(|s| s * s).sum::<f32>() / track.len().max(1) as f32).sqrt()
}

fn secs(s: f64) -> usize {
    (s * SR as f64) as usize
}

/// Song from 5 s to 25 s: track 1 starts 10 s in, track 2 ends 5 s early,
/// track 3 is not used at all.
fn test_song() -> Song {
    let mut song = Song::new(0, "Overlap".to_string(), secs(5.0), secs(25.0), TONES.len());
    song.tracks[1].start = Some(secs(15.0));
    song.tracks[2].end = Some(secs(20.0));
    song.tracks[3].active = false;
    song
}

#[test]
fn end_to_end() {
    let dir = std::env::temp_dir().join("multi_track_split_test");
    std::fs::create_dir_all(&dir).unwrap();
    let tape_path = make_test_tape(&dir);

    let audio = audio::load(&tape_path).expect("load test tape");
    assert_eq!(audio.channels(), 4);
    assert_eq!(audio.frames(), 30 * SR as usize);
    assert_eq!(audio.sample_rate, SR);

    let song = test_song();
    assert_eq!(song.track_range(0), Some((secs(5.0), secs(25.0))));
    assert_eq!(song.track_range(1), Some((secs(15.0), secs(25.0))));
    assert_eq!(song.track_range(2), Some((secs(5.0), secs(20.0))));
    assert_eq!(song.track_range(3), None);

    // Multitrack export in both formats: 3 tracks, 20 s, with silence where
    // a track is not (yet / any more) part of the song.
    for ext in ["flac", "wav"] {
        let out = dir.join(format!("song.{ext}"));
        export::export_multitrack(&audio, &song, &out).expect("multitrack export");
        let back = audio::load(&out).expect("decode exported multitrack");
        assert_eq!(back.channels(), 3, "{ext}: only active tracks are exported");
        assert_eq!(back.frames(), secs(20.0), "{ext}: song length");

        let loud = 0.5 / 2f32.sqrt();
        // Track 0: tone throughout.
        assert!((rms(&back.tracks[0][..secs(5.0)]) - loud).abs() < 0.02, "{ext}");
        assert!((rms(&back.tracks[0][secs(15.0)..]) - loud).abs() < 0.02, "{ext}");
        // Track 1 (tape track 2): silent for its first 10 s, then tone.
        assert!(rms(&back.tracks[1][..secs(10.0)]) < 0.001, "{ext}: leading silence");
        assert!((rms(&back.tracks[1][secs(10.0)..]) - loud).abs() < 0.02, "{ext}");
        // Track 2 (tape track 3): tone for 15 s, then silent.
        assert!((rms(&back.tracks[2][..secs(15.0)]) - loud).abs() < 0.02, "{ext}");
        assert!(rms(&back.tracks[2][secs(15.0)..]) < 0.001, "{ext}: trailing silence");
    }

    // Mixdown: pan track 0 hard left, track 2 hard right, mute track 1 → the
    // right channel is silent once track 2 has ended at 15 s into the song.
    // Track 0 is boosted so the left channel exceeds 0 dBFS: the float WAV
    // must keep that, the FLAC must clamp it and say so.
    let mut mixed = song.clone();
    mixed.tracks[0].pan = -1.0;
    mixed.tracks[0].volume = 1.5;
    mixed.tracks[1].mute = true;
    mixed.tracks[2].pan = 1.0;
    let out = dir.join("song (mix).wav");
    let msg =
        export::export_mixdown(&audio, &mixed, &out, MixFormat::Float32).expect("mixdown export");
    assert!(msg.contains("32-bit float"), "{msg}");
    let back = audio::load(&out).unwrap();
    assert_eq!(back.channels(), 2);
    assert_eq!(back.bits_per_sample, 32);
    assert_eq!(back.frames(), secs(20.0));
    let left_peak = back.tracks[0].iter().fold(0f32, |m, v| m.max(v.abs()));
    assert!((left_peak - 0.75).abs() < 0.01, "float keeps the hot mix: {left_peak}");
    assert!(rms(&back.tracks[1][..secs(15.0)]) > 0.3, "right carries track 2");
    assert!(rms(&back.tracks[1][secs(15.0)..]) < 0.001, "right silent after track 2 ends");

    mixed.tracks[0].volume = 2.5; // 0.5 × 2.5 = 1.25 → would clip in integer formats
    let out_wav = dir.join("song hot (mix).wav");
    export::export_mixdown(&audio, &mixed, &out_wav, MixFormat::Float32).unwrap();
    let back = audio::load(&out_wav).unwrap();
    let left_peak = back.tracks[0].iter().fold(0f32, |m, v| m.max(v.abs()));
    assert!((left_peak - 1.25).abs() < 0.01, "float wav keeps >0 dBFS: {left_peak}");

    // Float cannot go into FLAC.
    let err = export::export_mixdown(&audio, &mixed, &dir.join("x.flac"), MixFormat::Float32)
        .unwrap_err();
    assert!(err.contains("float"), "{err}");

    // Normalized integer exports land at the -1 dBFS target, whether the
    // mix was too hot (scaled down) or quiet (scaled up).
    let target = 10f32.powf(export::NORMALIZE_TARGET_DB / 20.0);
    for (name, mode, bits) in [
        ("hot16.wav", MixFormat::Normalized { format: Format::Wav, bits: 16 }, 16),
        ("hot24.flac", MixFormat::Normalized { format: Format::Flac, bits: 24 }, 24),
    ] {
        let out = dir.join(name);
        let msg = export::export_mixdown(&audio, &mixed, &out, mode).unwrap();
        assert!(msg.contains("normalized -"), "{msg}");
        let back = audio::load(&out).unwrap();
        assert_eq!(back.bits_per_sample, bits, "{name}");
        let left_peak = back.tracks[0].iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!((left_peak - target).abs() < 0.002, "{name} normalized peak {left_peak}");
    }
    let mut quiet = mixed.clone();
    quiet.tracks[0].volume = 0.1;
    let out = dir.join("quiet.wav");
    let msg = export::export_mixdown(
        &audio,
        &quiet,
        &out,
        MixFormat::Normalized { format: Format::Wav, bits: 24 },
    )
    .unwrap();
    assert!(msg.contains("normalized +"), "{msg}");
    let back = audio::load(&out).unwrap();
    // Track 2 on the right is now the loudest part of the mix.
    let peak = back
        .tracks
        .iter()
        .flatten()
        .fold(0f32, |m, v| m.max(v.abs()));
    assert!((peak - target).abs() < 0.002, "quiet mix raised to target: {peak}");

    // Export all mixes into a folder, in the chosen format.
    let mix_dir = dir.join("mixes");
    let _ = std::fs::remove_dir_all(&mix_dir);
    std::fs::create_dir_all(&mix_dir).unwrap();
    export::export_all_mixdowns(&audio, &[song.clone(), mixed.clone()], &mix_dir, MixFormat::Float32)
        .unwrap();
    assert!(mix_dir.join("01 - Overlap (mix).wav").is_file());
    assert!(mix_dir.join("02 - Overlap (mix).wav").is_file());
    export::export_all_mixdowns(
        &audio,
        &[song.clone()],
        &mix_dir,
        MixFormat::Normalized { format: Format::Flac, bits: 16 },
    )
    .unwrap();
    assert!(mix_dir.join("01 - Overlap (mix).flac").is_file());

    // Solo wins over everything else.
    let mut soloed = song.clone();
    soloed.tracks[2].solo = true;
    let mixes = mix::song_mixes(&soloed);
    assert_eq!(mixes.len(), 1);
    assert_eq!(mixes[0].channel, 2);

    // FLAC is limited to 8 channels; a 9-track song must be refused.
    let wide = Song::new(1, "wide".into(), 0, 100, 9);
    let err = export::write_audio(&dir.join("wide.flac"), &vec![0.0; 900], 9, 16, SR).unwrap_err();
    assert!(err.contains("8 channels"), "{err}");
    assert_eq!(wide.active_channels().len(), 9);

    // Per-track stems: one mono file per used track in a song folder, with
    // the same silence placement as the interleaved file.
    let names: Vec<String> = ["Kick", "Bass", "Gtr", "Vox"].map(String::from).to_vec();
    let loud = 0.5 / 2f32.sqrt();
    let stem_dir = dir.join("01 - Overlap");
    let _ = std::fs::remove_dir_all(&stem_dir);
    let msg = export::export_stem_tracks(&audio, &song, &names, &stem_dir, Format::Wav).unwrap();
    assert!(msg.contains("3 track files"), "{msg}");
    assert!(!stem_dir.join("T04 - Vox.wav").exists(), "unused track is not written");
    let bass = audio::load(&stem_dir.join("T02 - Bass.wav")).unwrap();
    assert_eq!(bass.channels(), 1);
    assert_eq!(bass.frames(), secs(20.0));
    assert!(rms(&bass.tracks[0][..secs(10.0)]) < 0.001);
    assert!((rms(&bass.tracks[0][secs(10.0)..]) - loud).abs() < 0.02);
    let gtr = audio::load(&stem_dir.join("T03 - Gtr.wav")).unwrap();
    assert!((rms(&gtr.tracks[0][..secs(15.0)]) - loud).abs() < 0.02);
    assert!(rms(&gtr.tracks[0][secs(15.0)..]) < 0.001);
    assert_eq!(export::stem_file_name(5, &names, "flac"), "T06 - Track 6.flac");

    // Export-all names files by number and title, in either layout.
    let all_dir = dir.join("all");
    let _ = std::fs::remove_dir_all(&all_dir);
    std::fs::create_dir_all(&all_dir).unwrap();
    let mut second = Song::new(1, "Second: Take/2".into(), secs(20.0), secs(30.0), 4);
    second.tracks[0].active = false;
    let both = [song.clone(), second.clone()];
    let interleaved = StemFormat { format: Format::Flac, layout: StemLayout::Interleaved };
    export::export_all_multitrack(&audio, &both, &names, &all_dir, interleaved).unwrap();
    assert!(all_dir.join("01 - Overlap.flac").is_file());
    assert!(all_dir.join("02 - Second_ Take_2.flac").is_file());
    let per_track = StemFormat { format: Format::Flac, layout: StemLayout::Tracks };
    export::export_all_multitrack(&audio, &both, &names, &all_dir, per_track).unwrap();
    assert!(all_dir.join("01 - Overlap").join("T01 - Kick.flac").is_file());
    assert!(all_dir.join("02 - Second_ Take_2").join("T02 - Bass.flac").is_file());
    assert!(!all_dir.join("02 - Second_ Take_2").join("T01 - Kick.flac").exists());

    // Project round trip through JSON, referenced by bare file name.
    let project_path = project::sidecar_path(std::slice::from_ref(&tape_path));
    let project = Project {
        audio_files: vec![project::audio_reference(&project_path, &tape_path)],
        audio_file: String::new(),
        sample_rate: SR,
        channels: 4,
        track_names: vec!["Kick".into(), "Bass".into(), "Gtr".into(), "Vox".into()],
        songs: vec![song.clone(), second],
        mix_format: Some(MixFormat::Normalized { format: Format::Flac, bits: 24 }),
        stem_format: Some(StemFormat { format: Format::Wav, layout: StemLayout::Tracks }),
    };
    assert_eq!(project.audio_files, ["tape.wav"]);
    project::save(&project_path, &project).unwrap();
    let loaded = project::load(&project_path).unwrap();
    assert_eq!(loaded.songs.len(), 2);
    assert_eq!(loaded.songs[0].tracks, song.tracks);
    assert_eq!(loaded.songs[0].start, song.start);
    assert_eq!(loaded.songs[1].id, 1, "ids are reassigned on load");
    assert_eq!(loaded.track_names[2], "Gtr");
    assert_eq!(loaded.mix_format, project.mix_format);
    assert_eq!(loaded.stem_format, project.stem_format);
    assert_eq!(
        project::resolve_audio_path(&project_path, &loaded.files()[0]).as_deref(),
        Some(tape_path.as_path())
    );

    // Older single-file projects still load.
    let legacy = r#"{"audio_file":"tape.wav","sample_rate":44100,"channels":4,"track_names":[],"songs":[]}"#;
    let legacy_path = dir.join("legacy.mtsplit.json");
    std::fs::write(&legacy_path, legacy).unwrap();
    assert_eq!(project::load(&legacy_path).unwrap().files(), ["tape.wav"]);
}

/// Several mono/stereo files become consecutive tracks, ordered naturally by
/// name, padded with silence to the longest file.
#[test]
fn multi_file_tape() {
    let dir = std::env::temp_dir().join("multi_track_split_multi");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let tone = |freq: f32, secs: usize, channels: usize| -> Vec<f32> {
        (0..secs * SR as usize)
            .flat_map(|i| {
                let v = 0.5 * (2.0 * std::f32::consts::PI * freq * i as f32 / SR as f32).sin();
                std::iter::repeat_n(v, channels)
            })
            .collect()
    };
    // "track 10" must sort after "track 2"; the stereo file is the longest.
    let files = [
        ("track 10 vox.wav", 220.0, 6, 1),
        ("track 2 bass.wav", 330.0, 10, 1),
        ("track 1 drums.flac", 440.0, 8, 2),
    ];
    let mut paths = Vec::new();
    for (name, freq, secs, ch) in files {
        let path = dir.join(name);
        export::write_audio(&path, &tone(freq, secs, ch), ch, 16, SR).unwrap();
        paths.push(path);
    }

    let audio = audio::load_many(&paths).expect("load set");
    assert_eq!(audio.channels(), 4);
    assert_eq!(audio.frames(), 10 * SR as usize, "padded to the longest file");
    assert_eq!(audio.source_channels, [2, 1, 1]);
    assert_eq!(
        audio.default_track_names(),
        ["track 1 drums L", "track 1 drums R", "track 2 bass", "track 10 vox"]
    );
    assert_eq!(audio.path, dir.join("track 1 drums.flac"));
    assert!(audio.file_name().starts_with("3 files in "));

    let loud = 0.5 / 2f32.sqrt();
    // Drums: 8 s of tone on both channels, then silence.
    assert!((rms(&audio.tracks[0][..secs(8.0)]) - loud).abs() < 0.02);
    assert!((rms(&audio.tracks[1][..secs(8.0)]) - loud).abs() < 0.02);
    assert!(rms(&audio.tracks[0][secs(8.0)..]) < 0.001);
    // Bass fills the whole length; vox stops after 6 s.
    assert!((rms(&audio.tracks[2][secs(9.0)..]) - loud).abs() < 0.02);
    assert!((rms(&audio.tracks[3][..secs(6.0)]) - loud).abs() < 0.02);
    assert!(rms(&audio.tracks[3][secs(6.0)..]) < 0.001);

    // The sidecar for a set is named after the directory.
    assert_eq!(
        project::sidecar_path(&audio.sources),
        dir.join("multi_track_split_multi.mtsplit.json")
    );

    // Mismatched sample rates are refused.
    let odd = dir.join("odd.wav");
    export::write_audio(&odd, &tone(100.0, 1, 1), 1, 16, 48000).unwrap();
    let err = audio::load_many(&[paths[0].clone(), odd]).unwrap_err();
    assert!(err.contains("48000 Hz"), "{err}");
}

#[test]
fn time_parsing() {
    assert_eq!(parse_time("90"), Some(90.0));
    assert_eq!(parse_time("1:30"), Some(90.0));
    assert_eq!(parse_time("01:30.500"), Some(90.5));
    assert_eq!(parse_time("1:01:30"), Some(3690.0));
    assert_eq!(parse_time(" 0:05 "), Some(5.0));
    assert_eq!(parse_time("abc"), None);
    assert_eq!(parse_time("1:2:3:4"), None);
    assert_eq!(parse_time("-5"), None);
    assert_eq!(audio::format_time(90.5), "01:30.500");
    assert_eq!(audio::format_time(3690.0), "1:01:30.000");
    assert_eq!(parse_time(&audio::format_time(1234.567)), Some(1234.567));
}

#[test]
fn pan_law() {
    let (l, r) = mix::pan_gains(0.0);
    assert!((l - r).abs() < 1e-6);
    assert!((l * l + r * r - 1.0).abs() < 1e-5, "constant power");
    let (l, r) = mix::pan_gains(-1.0);
    assert!((l - 1.0).abs() < 1e-6 && r.abs() < 1e-6);
    let (l, r) = mix::pan_gains(1.0);
    assert!(l.abs() < 1e-6 && (r - 1.0).abs() < 1e-6);
}
