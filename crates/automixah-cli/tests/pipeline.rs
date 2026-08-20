//! Integration tests for the CLI pipeline: real files in, mixed WAV out.

use automixah_cli::{Config, TempoStrategyArg, run};
use std::path::PathBuf;

/// stratum-dsp click fixture with steady 120 BPM beats.
fn click120() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../stratum-dsp/tests/fixtures/120bpm_4bar.wav")
}

/// stratum-dsp click fixture with steady 128 BPM beats.
fn click128() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../stratum-dsp/tests/fixtures/128bpm_4bar.wav")
}

/// 48 kHz resample of the 120 BPM click (first-track-defines-rate test).
fn click120_48k() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/120bpm_4bar_48k.wav")
}

/// Real-music fixture (148 BPM, G minor) if present (gitignored).
fn real_track() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../automixah-engine/tests/fixtures/music/sam-laxton-full-effect.ogg");
    p.exists().then_some(p)
}

/// Basic config with the two click fixtures in order.
fn click_config(out: &std::path::Path) -> Config {
    Config {
        tracks: vec![click120(), click128()],
        out: out.to_path_buf(),
        target_bpm: None,
        tempo_strategy: TempoStrategyArg::Session,
        automation: None,
    }
}

/// Reads a float32 WAV with hound, returning (rate, samples, channels).
fn read_wav(path: &std::path::Path) -> (u32, Vec<f32>, u16) {
    let mut reader = hound::WavReader::open(path).expect("open wav");
    let rate = reader.spec().sample_rate;
    let channels = reader.spec().channels;
    let samples = reader.samples::<f32>().map(Result::unwrap).collect();
    (rate, samples, channels)
}

#[ignore = "real-audio (decode+analyze in debug); run via just test-heavy"]
#[test]
fn two_track_render_has_planned_duration_no_gaps_no_clip() {
    // Given two click fixtures mixed with zero config.
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("mix.wav");
    let config = click_config(&out);

    // When running the pipeline.
    let total = run(&config).expect("run");

    // Then the WAV exists with the planned frame count (stereo) and
    // f32 format.
    let (rate, samples, channels) = read_wav(&out);
    assert_eq!(rate, 44_100);
    assert_eq!(channels, 2, "mix must be stereo");
    assert_eq!(samples.len() as u64, total.0 * 2);

    // And nothing clips.
    assert!(
        samples.iter().all(|s| s.abs() <= 1.0),
        "mix clips beyond ±1.0"
    );

    // And no mix gap: no silent run longer than 1s anywhere.
    let mut run_len = 0_usize;
    let mut worst = 0_usize;
    for s in &samples {
        if s.abs() < 1e-6 {
            run_len += 1;
            worst = worst.max(run_len);
        } else {
            run_len = 0;
        }
    }
    assert!(worst <= 44_100, "silent gap of {worst} samples");
}

// Given a 48 kHz click first and a 44.1 kHz click second.
#[ignore = "real-audio (decode+analyze in debug); run via just test-heavy"]
#[test]
fn forty_eight_k_first_track_writes_forty_eight_k_wav() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("mix48k.wav");
    let config = Config {
        tracks: vec![click120_48k(), click128()],
        out: out.clone(),
        target_bpm: None,
        tempo_strategy: TempoStrategyArg::Session,
        automation: None,
    };

    // When running the pipeline.
    let total = run(&config).expect("run");

    // Then the WAV header reports the first track's 48 kHz and the
    // sample count matches the returned session length at that rate.
    let (rate, samples, channels) = read_wav(&out);
    assert_eq!(rate, 48_000);
    assert_eq!(channels, 2);
    assert_eq!(samples.len() as u64, total.0 * 2);
}

#[ignore = "real-audio (decode+analyze in debug); run via just test-heavy"]
#[test]
fn target_bpm_override_changes_plan() {
    // Given the same two tracks with an explicit target.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = click_config(&dir.path().join("a.wav"));
    let base = run(&config).expect("base run");

    // When targeting a faster BPM.
    config.out = dir.path().join("b.wav");
    config.target_bpm = Some(140.0);
    let faster = run(&config).expect("faster run");

    // Then the session shrinks (everything plays faster).
    assert!(faster.0 < base.0, "140 BPM target should shorten session");
}

#[test]
fn driftback_strategy_changes_plan() {
    // Given analyses matching the click fixtures' BPMs.
    use automixah_engine::timeline::types::{TempoStrategy, TrackAnalysis, TrackHash};
    use djcore::key::{Key, KeyMode};
    let mk = |bpm: f32| TrackAnalysis {
        hash: TrackHash(format!("t{bpm}")),
        bpm,
        bpm_confidence: 0.9,
        key: Key {
            root: 0,
            mode: KeyMode::Minor,
        },
        duration: 8.0,
        beat_grid: djcore::analyzer::BeatGrid::default(),
        grid_stability: 0.9,
        sample_rate: 44_100,
        channels: 1,
        format: String::new(),
    };
    let analyses = vec![mk(120.0), mk(128.0)];
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = click_config(&dir.path().join("a.wav"));

    // When planning with each strategy.
    let session = automixah_cli::plan_only(&config, &analyses);
    config.tempo_strategy = TempoStrategyArg::Driftback;
    let drift = automixah_cli::plan_only(&config, &analyses);

    // Then drift-back segments carry the DriftBack strategy.
    let drift_back_count = drift
        .segments
        .iter()
        .filter(|s| matches!(s.stretch.strategy, TempoStrategy::DriftBack { .. }))
        .count();
    assert!(drift_back_count > 0, "drift-back flag must mark segments");
    assert!(
        !session
            .segments
            .iter()
            .any(|s| matches!(s.stretch.strategy, TempoStrategy::DriftBack { .. })),
        "default must stay session-BPM"
    );
}

#[test]
fn missing_file_fails_loudly() {
    // Given a nonexistent track path.
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        tracks: vec![PathBuf::from("/nonexistent/track.ogg")],
        out: dir.path().join("x.wav"),
        target_bpm: None,
        tempo_strategy: TempoStrategyArg::Session,
        automation: None,
    };

    // When running the pipeline.
    // Then it errors (and main exits non-zero for this case).
    let err = run(&config).expect_err("missing file must fail");
    let text = format!("{err:?}");
    assert!(
        text.contains("/nonexistent/track.ogg"),
        "error names track: {text}"
    );
}

#[ignore = "real-audio (decode+analyze in debug); run via just test-heavy"]
#[test]
fn real_fixture_end_to_end_when_present() {
    // Given the real 148 BPM / Gm fixture (gitignored, may be absent).
    let Some(track) = real_track() else {
        eprintln!("real fixture absent, skipping");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let tone =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../djcore/tests/fixtures/tone440.flac");
    let config = Config {
        tracks: vec![track, tone],
        out: dir.path().join("mix.wav"),
        target_bpm: None,
        tempo_strategy: TempoStrategyArg::Session,
        automation: None,
    };

    // When running the pipeline with the real track.
    let total = run(&config).expect("real fixture run");

    // Then a full-length stereo WAV is produced without clipping.
    let (_, samples, _) = read_wav(&dir.path().join("mix.wav"));
    assert_eq!(samples.len() as u64, total.0 * 2);
    assert!(samples.iter().all(|s| s.abs() <= 1.0));
}
