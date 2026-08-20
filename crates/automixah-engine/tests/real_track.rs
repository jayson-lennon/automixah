//! Analysis sanity over the real-music fixture.
//!
//! `#[ignore]`d because the fixture is local-only (gitignored) and full-track
//! analysis is slow in debug builds. Run explicitly:
//! `cargo test -p automixah-engine --release --test real_track -- --ignored --nocapture`

use automixah_engine::timeline::TrackAnalysis;
use djcore::analyzer::{AudioAnalyzer, StratumAnalyzer};
use djcore::decoder::DecoderRegistry;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/music/sam-laxton-full-effect.ogg"
);

#[test]
#[ignore = "requires local fixture; slow in debug"]
fn full_track_analysis_reports_expected_bpm_and_key() {
    // Given the real fixture (user-stated ground truth: 148 BPM, Gm).
    let Ok(bytes) = std::fs::read(FIXTURE) else {
        eprintln!("real fixture absent, skipping");
        return;
    };
    let registry = DecoderRegistry::with_symphonia();

    // When decoding and analyzing.
    let audio = registry.decode(&bytes, "ogg").expect("decode");
    let started = std::time::Instant::now();
    let result = StratumAnalyzer::new()
        .analyze(&audio.samples, audio.sample_rate)
        .expect("analyze");
    let elapsed = started.elapsed();
    let out = TrackAnalysis {
        hash: automixah_engine::timeline::TrackHash("fixture".into()),
        bpm: result.bpm,
        bpm_confidence: result.bpm_confidence,
        key: result.key,
        duration: result.duration_seconds,
        beat_grid: result.beat_grid,
        grid_stability: result.grid_stability,
        sample_rate: audio.sample_rate,
        channels: 2,
        format: "ogg".into(),
    };

    // Then the reported BPM is near 148 and the key is G minor.
    println!(
        "bpm={} conf={:.2} key={} stab={:.2} dur={:.1}s analysis={elapsed:?}",
        out.bpm, out.bpm_confidence, out.key, out.grid_stability, out.duration
    );
    println!(
        "beats={} downbeats={}",
        out.beat_grid.beats.len(),
        out.beat_grid.downbeats.len()
    );
    assert!(
        (out.bpm - 148.0).abs() <= 2.0,
        "bpm {} not within ±2 of 148",
        out.bpm
    );
    assert_eq!((out.key.root, out.key.mode), (7, djcore::KeyMode::Minor));
}
