//! Integration tests for the mixdown pipeline: real files in, mixed
//! WAV out (no analyzer — grids are hand-built to the fixtures' known
//! BPMs).

use std::path::{Path, PathBuf};

use automixah_engine::mixdown::{
    MixdownJob, MixdownOutcome, MixdownStage, MixdownTrack, run_mixdown,
};
use djcore::key::{Key, KeyMode};

/// stratum-dsp click fixture with steady 120 BPM beats.
fn click120() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../stratum-dsp/tests/fixtures/120bpm_4bar.wav")
}

/// stratum-dsp click fixture with steady 128 BPM beats.
fn click128() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../stratum-dsp/tests/fixtures/128bpm_4bar.wav")
}

/// 48 kHz resample of the 120 BPM click (first-track-defines-rate).
fn click120_48k() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/120bpm_4bar_48k.wav")
}

/// A two-track job over the click fixtures with hand-built grids
/// (duration 8 s: the 4-bar fixtures at 44.1 kHz).
fn click_job(out: &Path) -> MixdownJob {
    let mk = |hash: &str, path: PathBuf, bpm: f32| MixdownTrack {
        hash: automixah_engine::timeline::types::TrackHash(hash.to_owned()),
        path,
        grid_bpm: bpm,
        anchor_seconds: 0.0,
        downbeat_phase: 0,
        key: Key {
            root: 9,
            mode: KeyMode::Minor,
        },
        duration: 8.0,
        cues: automixah_engine::timeline::types::CuePoints::default(),
    };
    MixdownJob {
        tracks: vec![mk("t120", click120(), 120.0), mk("t128", click128(), 128.0)],
        out: out.to_path_buf(),
    }
}

/// Reads a float32 WAV with hound, returning (rate, samples, channels).
fn read_wav(path: &Path) -> (u32, Vec<f32>, u16) {
    let mut reader = hound::WavReader::open(path).expect("open wav");
    let rate = reader.spec().sample_rate;
    let channels = reader.spec().channels;
    let samples = reader.samples::<f32>().map(Result::unwrap).collect();
    (rate, samples, channels)
}

/// The `.part` sibling path a mixdown writes before renaming.
fn part_of(out: &Path) -> PathBuf {
    let mut os = out.as_os_str().to_os_string();
    os.push(".part");
    PathBuf::from(os)
}

#[ignore = "real-audio (decode+stretch in debug); run via just test-heavy"]
#[test]
fn mixdown_writes_wav_with_planned_duration_no_gaps_no_clip() {
    // Given two click fixtures mixed with zero config.
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("mix.wav");

    // When running the mixdown.
    let outcome = run_mixdown(&click_job(&out), &mut |_| {}, &|| false);

    // Then a full-length stereo f32 WAV exists.
    assert_eq!(outcome, MixdownOutcome::Done);
    let (rate, samples, channels) = read_wav(&out);
    assert_eq!(rate, 44_100, "session rate = first track's rate");
    assert_eq!(channels, 2, "mix must be stereo");
    assert!(!samples.is_empty());

    // And nothing clips.
    assert!(
        samples.iter().all(|s| s.abs() <= 1.0),
        "mix clips beyond ±1.0"
    );

    // And no mix seam gap: cueing pads the final segment's tail with
    // silence by design (the stored duration spans the cue), so only
    // the audible body is seam-checked — no silent run > 1 s inside it.
    let body = samples
        .iter()
        .rposition(|s| s.abs() >= 1e-6)
        .map_or(0, |i| i + 1);
    let mut run_len = 0_usize;
    let mut worst = 0_usize;
    for s in &samples[..body] {
        if s.abs() < 1e-6 {
            run_len += 1;
            worst = worst.max(run_len);
        } else {
            run_len = 0;
        }
    }
    assert!(worst <= 44_100, "silent gap of {worst} samples");
}

#[ignore = "real-audio (decode+stretch in debug); run via just test-heavy"]
#[test]
fn forty_eight_k_first_track_writes_forty_eight_k_wav() {
    // Given a 48 kHz click first and a 44.1 kHz click second.
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("mix48k.wav");
    let mut job = click_job(&out);
    job.tracks[0] = MixdownTrack {
        path: click120_48k(),
        ..job.tracks[0].clone()
    };

    // When running the mixdown.
    let outcome = run_mixdown(&job, &mut |_| {}, &|| false);

    // Then the WAV header reports the first track's 48 kHz.
    assert_eq!(outcome, MixdownOutcome::Done);
    let (rate, samples, channels) = read_wav(&out);
    assert_eq!(rate, 48_000);
    assert_eq!(channels, 2);
    assert!(!samples.is_empty());
}

#[test]
fn mixdown_missing_track_fails_without_output_file() {
    // Given a job naming a nonexistent track.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut job = click_job(&dir.path().join("x.wav"));
    job.tracks[0].path = PathBuf::from("/nonexistent/track.ogg");

    // When running the mixdown.
    let outcome = run_mixdown(&job, &mut |_| {}, &|| false);

    // Then it fails naming the track and writes nothing.
    let MixdownOutcome::Failed(message) = outcome else {
        panic!("missing file must fail");
    };
    assert!(
        message.contains("/nonexistent/track.ogg"),
        "error names track: {message}"
    );
    assert!(!dir.path().join("x.wav").exists());
    assert!(!part_of(&dir.path().join("x.wav")).exists());
}

#[test]
fn mixdown_cancel_stops_early_and_leaves_no_files() {
    // Given a job whose first mixing callback flips cancellation.
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("mix.wav");

    // When running with a cancel that trips at the first stage report.
    let outcome = run_mixdown(&click_job(&out), &mut |_| {}, &|| true);

    // Then the outcome is Cancelled and neither target nor `.part`
    // exists.
    assert_eq!(outcome, MixdownOutcome::Cancelled);
    assert!(!out.exists());
    assert!(!part_of(&out).exists());
}

#[test]
fn mixdown_reports_staged_progress_reaching_one() {
    // Given a click job (fast enough in debug for a smoke run: the
    // fixtures are 8 s clicks).
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("mix.wav");

    // When running with a recording progress callback.
    let mut stages: Vec<MixdownStage> = Vec::new();
    let outcome = run_mixdown(&click_job(&out), &mut |s| stages.push(s), &|| false);

    // Then stage classes appear in order and mixing reaches 1.0.
    assert_eq!(outcome, MixdownOutcome::Done);
    let class = |i: usize| match stages[i] {
        MixdownStage::Decoding { .. } => 0,
        MixdownStage::Stretching { .. } => 1,
        MixdownStage::Mixing { .. } => 2,
    };
    let classes: Vec<u8> = (0..stages.len()).map(class).collect();
    let first_of = |c: u8| classes.iter().position(|&x| x == c);
    let d = first_of(0).expect("decoding stage present");
    let s = first_of(1).expect("stretching stage present");
    let m = first_of(2).expect("mixing stage present");
    assert!(
        d < s && s < m,
        "stage order decode→stretch→mix: {classes:?}"
    );
    // And the last report is a complete mixing fraction.
    assert_eq!(
        stages.last(),
        Some(&MixdownStage::Mixing { fraction: 1.0 }),
        "mixing reaches 1.0"
    );
}
