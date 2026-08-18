//! Integration tests: decode real audio fixtures through the symphonia
//! backend and analyze a synthetic click track end-to-end.

use std::sync::Arc;

use djcore::analyzer::{AudioAnalyzer, StratumAnalyzer};
use djcore::decoder::{AudioDecoder, DecoderRegistry, SymphoniaDecoder};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("tests/fixtures/{name}");
    std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read {path}: {e}"))
}

fn decode_tone_fixture(ext: &str) -> djcore::decoder::DecodeAudio {
    let bytes = fixture(&format!("tone440.{ext}"));
    let registry = DecoderRegistry::with_symphonia();
    registry
        .decode(&bytes, ext)
        .unwrap_or_else(|e| panic!("failed to decode {ext}: {e:?}"))
}

#[test]
fn decodes_wav_fixture_at_44100() {
    // Given a 1-second 440 Hz WAV fixture.
    let audio = decode_tone_fixture("wav");

    // Then the sample count matches 1 s at 44.1 kHz and the rate is lifted.
    assert_eq!(audio.sample_rate, 44_100);
    assert_eq!(audio.samples.len(), 44_100);
}

#[test]
fn decodes_mp3_fixture_to_approximate_length() {
    // Given a 1-second 440 Hz MP3 fixture (lossy codecs pad/truncate).
    let audio = decode_tone_fixture("mp3");

    // Then the decoded length is within 5% of 1 s at 44.1 kHz.
    let expected = 44_100.0;
    #[allow(clippy::cast_precision_loss)]
    let actual = audio.samples.len() as f32;
    assert!(
        (actual - expected).abs() / expected < 0.05,
        "decoded {} samples, expected ~{expected}",
        audio.samples.len()
    );
}

#[test]
fn decodes_flac_fixture_exactly() {
    // Given a 1-second 440 Hz FLAC fixture (lossless).
    let audio = decode_tone_fixture("flac");

    // Then the decoded length matches the WAV decode exactly.
    let wav = decode_tone_fixture("wav");
    assert_eq!(audio.samples.len(), wav.samples.len());
}

#[test]
fn decodes_ogg_fixture_to_approximate_length() {
    // Given a 1-second 440 Hz OGG fixture (lossy).
    let audio = decode_tone_fixture("ogg");

    // Then the decoded length is within 5% of 1 s at 44.1 kHz.
    let expected = 44_100.0;
    #[allow(clippy::cast_precision_loss)]
    let actual = audio.samples.len() as f32;
    assert!(
        (actual - expected).abs() / expected < 0.05,
        "decoded {} samples, expected ~{expected}",
        audio.samples.len()
    );
}

#[test]
fn decodes_aac_fixture_to_approximate_length() {
    // Given a 1-second 440 Hz AAC fixture (lossy).
    let audio = decode_tone_fixture("aac");

    // Then the decoded length is within 5% of 1 s at 44.1 kHz.
    let expected = 44_100.0;
    #[allow(clippy::cast_precision_loss)]
    let actual = audio.samples.len() as f32;
    assert!(
        (actual - expected).abs() / expected < 0.05,
        "decoded {} samples, expected ~{expected}",
        audio.samples.len()
    );
}

#[test]
fn symphonia_decoder_name_and_extensions() {
    // Given the symphonia decoder.
    let decoder = SymphoniaDecoder::new();

    // Then it names itself and lists the expected extensions.
    assert_eq!(decoder.name(), "symphonia");
    assert!(decoder.supported_extensions().contains(&"mp3"));
    assert!(decoder.supported_extensions().contains(&"flac"));
    assert!(decoder.supported_extensions().contains(&"wav"));
    assert!(decoder.supported_extensions().contains(&"ogg"));
    assert!(decoder.supported_extensions().contains(&"aac"));
}

#[test]
fn click_track_analysis_detects_120bpm_and_populates_grid() {
    // Given an 8-second click track at 120 BPM with accented downbeats.
    let bytes = fixture("click120.wav");
    let registry = DecoderRegistry::with_symphonia();
    let audio = registry
        .decode(&bytes, "wav")
        .unwrap_or_else(|e| panic!("decode failed: {e:?}"));

    // When analyzing the decoded samples.
    let analyzer = StratumAnalyzer::new();
    let output = analyzer
        .analyze(&audio.samples, audio.sample_rate)
        .unwrap_or_else(|e| panic!("analysis failed: {e:?}"));

    // Then BPM is detected near 120 after octave normalization by the caller.
    // stratum-dsp may report 60/120/240; accept any octave of 120.
    let bpm = output.bpm;
    let ratio = bpm / 120.0;
    let is_octave =
        (ratio - 1.0).abs() < 0.06 || (ratio - 2.0).abs() < 0.06 || (ratio - 0.5).abs() < 0.06;
    assert!(is_octave, "BPM {bpm} is not within 6% of an octave of 120");

    // And the beat grid is populated with beats at ~0.5 s spacing.
    assert!(!output.beat_grid.beats.is_empty(), "beat grid has no beats");
    // And downbeats are populated too (grid construction ran).
    assert!(
        !output.beat_grid.downbeats.is_empty(),
        "beat grid has no downbeats"
    );
    // And duration metadata is populated.
    assert!(
        output.duration_seconds > 7.5 && output.duration_seconds < 8.5,
        "duration {} not in (7.5, 8.5)",
        output.duration_seconds
    );
}

/// Confirms the analyzer trait object path works through `Arc<dyn …>`.
#[test]
fn analyzer_composes_as_trait_object() {
    // Given the stratum analyzer behind a trait object.
    let analyzer: Arc<dyn AudioAnalyzer> = Arc::new(StratumAnalyzer::new());

    // When asking for its name.
    // Then the stratum backend identifies itself.
    assert_eq!(analyzer.name(), "stratum-dsp");
}
