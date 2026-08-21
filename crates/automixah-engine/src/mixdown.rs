//! The offline mixdown pipeline: a playlist snapshot in, WAV out.
//!
//! The entry point is [`run_mixdown`], driven by a [`MixdownJob`]: a
//! message of per-track metadata (path, canonical beat grid, key,
//! duration) snapshotted **once** by the caller at job-build time.
//! Audio is read and decoded from disk inside the pipeline — analysis
//! never runs; stored grids are treated as vetted (the pipeline fills
//! `grid_stability` with [`CONFIDENT_STABILITY`] so the planner's
//! phrase-aligned cueing always applies).
//!
//! Output is atomic: PCM is written to a `.part` sibling of the target
//! and renamed into place on success; cancel or failure removes the
//! partial file, so the target path never holds a half-rendered mix.
//!
//! Progress and cancellation are cooperative: the pipeline reports
//! [`MixdownStage`] transitions between tracks and render chunks, and
//! polls the `cancel` closure at the same boundaries.

use std::path::{Path, PathBuf};

use error_stack::{Report, ResultExt};
use wherror::Error;

use djcore::analyzer::BeatGrid;
use djcore::decoder::{DecodeAudio, DecoderRegistry};

use crate::render::renderer::{Renderer, TrackFetchError, TrackProvider};
use crate::render::resample::Resampler;
use crate::render::wsola::Wsola;
use crate::timeline::plan::{PlanOptions, plan_with};
use crate::timeline::types::{CuePoints, SessionPlan, SessionTime, TrackAnalysis, TrackHash};

/// Beats per bar assumed when projecting a canonical grid (4/4).
pub const BEATS_PER_BAR: u8 = 4;

/// Stability filled for stored grids: a grid in the library was either
/// auto-detected (good enough to persist) or hand-aligned by the user
/// — either way it is vetted, which is what "confident" means to the
/// planner.
pub const CONFIDENT_STABILITY: f32 = 1.0;

/// Confidence filled for the unused-by-planner `bpm_confidence` field.
const FILLED_CONFIDENCE: f32 = 1.0;

/// Render-chunk length in session frames (~1.5 s at 44.1 kHz): large
/// enough that per-chunk overhead is negligible, small enough that
/// progress updates and cancel checks land within a user-perceptible
/// delay.
const MIX_CHUNK_FRAMES: u64 = 65_536;

/// Mixdown failure modes.
#[derive(Debug, Error)]
#[error(debug)]
pub struct MixdownError;

/// Per-track metadata snapshot (the job message; gathered once at
/// click time — later database edits cannot affect it).
#[derive(Debug, Clone)]
pub struct MixdownTrack {
    /// Identity of the track (content hash).
    pub hash: TrackHash,
    /// Source audio file, read from disk inside the pipeline.
    pub path: PathBuf,
    /// Canonical constant-tempo BPM of the vetted grid.
    pub grid_bpm: f32,
    /// Grid phase anchor in seconds.
    pub anchor_seconds: f32,
    /// Beat-in-bar of the first downbeat at/after the anchor (0..4).
    pub downbeat_phase: u8,
    /// Detected musical key.
    pub key: djcore::key::Key,
    /// Track duration in seconds (source time).
    pub duration: f32,
    /// User-authored source-frame cue points (snapshotted at click time).
    pub cues: CuePoints,
}

/// A mixdown request: ordered tracks plus the output WAV path.
#[derive(Debug, Clone)]
pub struct MixdownJob {
    /// Tracks in playlist order.
    pub tracks: Vec<MixdownTrack>,
    /// Target WAV path (written atomically via a `.part` sibling).
    pub out: PathBuf,

    pub bpm: f32,
}

/// One progress report from a running mixdown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MixdownStage {
    /// Track `done` of `total` decoded from disk.
    Decoding { done: usize, total: usize },
    /// Track `done` of `total` stretched and cue-sliced.
    Stretching { done: usize, total: usize },
    /// Session mixing, `fraction` of total samples in `[0, 1]`.
    Mixing { fraction: f32 },
}

/// Terminal outcome of a mixdown run.
#[derive(Debug, Clone, PartialEq)]
pub enum MixdownOutcome {
    /// The WAV exists at the requested path.
    Done,
    /// The user cancelled; no files remain (`.part` removed).
    Cancelled,
    /// The pipeline failed; the string is the rendered report and no
    /// files remain.
    Failed(String),
}

/// The canonical grid triple projected over `[0, duration]`: beats at
/// `anchor + k·60/bpm`, downbeats every [`BEATS_PER_BAR`] beats
/// starting at beat index `downbeat_phase`, bars == downbeats.
///
/// Mirrors the UI editor's projection math (source of truth is the
/// canonical triple; arrays are projections).
#[must_use]
pub fn project_grid(
    grid_bpm: f32,
    anchor_seconds: f32,
    downbeat_phase: u8,
    duration: f32,
) -> BeatGrid {
    let beat = beat_seconds(grid_bpm);
    let mut beats = Vec::new();
    let mut downbeats = Vec::new();

    #[expect(clippy::cast_possible_truncation, reason = "beat index fits i64")]
    let first_k = (-anchor_seconds / beat).ceil() as i64;
    let mut k = first_k;
    loop {
        let time = anchor_seconds + k as f32 * beat;
        if time > duration {
            break;
        }
        if time >= 0.0 {
            beats.push(time);
            if k.rem_euclid(i64::from(BEATS_PER_BAR)) == i64::from(downbeat_phase) {
                downbeats.push(time);
            }
        }
        k += 1;
    }

    BeatGrid {
        grid_bpm,
        anchor_seconds,
        bars: downbeats.clone(),
        beats,
        downbeats,
    }
}

/// Seconds per beat for a BPM, floored to keep division well-defined.
fn beat_seconds(grid_bpm: f32) -> f32 {
    60.0 / grid_bpm.max(0.01)
}

/// Builds the planner input for one decoded track from its snapshot
/// metadata. Stored grids are vetted by contract, so stability is
/// filled confident — this is the sole place that assumption lives.
fn analysis_from(track: &MixdownTrack, decoded: &DecodeAudio) -> TrackAnalysis {
    TrackAnalysis {
        hash: track.hash.clone(),
        bpm: track.grid_bpm,
        bpm_confidence: FILLED_CONFIDENCE,
        key: track.key.clone(),
        duration: track.duration,
        beat_grid: project_grid(
            track.grid_bpm,
            track.anchor_seconds,
            track.downbeat_phase,
            track.duration,
        ),
        grid_stability: CONFIDENT_STABILITY,
        sample_rate: decoded.sample_rate,
        channels: decoded.channels.max(1),
        format: String::new(),
        cues: track.cues,
    }
}

/// Runs a mixdown job to completion (or cancellation).
///
/// Decode → plan → stretch → mix in chunks → atomic WAV write, with
/// cooperative progress and cancellation. The target path only ever
/// holds a complete mix: PCM lands in a `.part` sibling first.
pub fn run_mixdown(
    job: &MixdownJob,
    progress: &mut dyn FnMut(MixdownStage),
    cancel: &dyn Fn() -> bool,
) -> MixdownOutcome {
    match mixdown(job, progress, cancel) {
        Ok(()) => MixdownOutcome::Done,
        // Debug rendering includes the attachment chain (paths,
        // stage names); `{:#}` drops non-printable attachments.
        Err(Reported::Failed(report)) => MixdownOutcome::Failed(format!("{report:?}")),
        Err(Reported::Cancelled) => MixdownOutcome::Cancelled,
    }
}

/// Internal result: `Err` distinguishes cancel (no message) from
/// failure (a rendered report).
enum Reported {
    Cancelled,
    Failed(Report<MixdownError>),
}

/// Pipeline body; every error site cleans up the `.part` first.
fn mixdown(
    job: &MixdownJob,
    progress: &mut dyn FnMut(MixdownStage),
    cancel: &dyn Fn() -> bool,
) -> Result<(), Reported> {
    let decoded = match decode_all(job, progress, cancel) {
        Ok(d) => d,
        Err(Reported::Cancelled) => {
            cleanup_partial(&job.out);
            return Err(Reported::Cancelled);
        }
        Err(e @ Reported::Failed(_)) => {
            cleanup_partial(&job.out);
            return Err(e);
        }
    };
    let analyses: Vec<TrackAnalysis> = decoded
        .iter()
        .map(|(snapshot, audio)| analysis_from(snapshot, audio))
        .collect();
    let transition = crate::automation::transition_spec::TransitionSpec::default_pair();
    let plan = plan_with(
        &analyses,
        PlanOptions {
            target_bpm: Some(job.bpm),
            force_drift_back: false,
            transition_beats: transition.beats,
            transition_name: transition.name.clone(),
        },
    );
    let provider = match build_provider(&decoded, &plan, progress, cancel) {
        Ok(p) => p,
        Err(Reported::Cancelled) => {
            cleanup_partial(&job.out);
            return Err(Reported::Cancelled);
        }
        Err(e @ Reported::Failed(_)) => {
            cleanup_partial(&job.out);
            return Err(e);
        }
    };
    let mix = match render_mix(&plan, provider, progress, cancel) {
        Ok(m) => m,
        Err(Reported::Cancelled) => {
            cleanup_partial(&job.out);
            return Err(Reported::Cancelled);
        }
        Err(e @ Reported::Failed(_)) => {
            cleanup_partial(&job.out);
            return Err(e);
        }
    };

    write_wav_atomic(&job.out, &mix, plan.sample_rate).inspect_err(|_| {
        cleanup_partial(&job.out);
    })
}

/// Removes the `.part` sibling if present; errors are ignored (best
/// effort — the cancel path must not mask its own outcome).
fn cleanup_partial(out: &Path) {
    let _ = std::fs::remove_file(part_path(out));
}

/// The temp path a mixdown writes to: the target with `.part`
/// appended (not `with_extension`, which would mangle
/// `my.mix.wav`).
fn part_path(out: &Path) -> PathBuf {
    let mut os = out.as_os_str().to_os_string();
    os.push(".part");
    PathBuf::from(os)
}

/// Decodes every snapshot's file from disk, reporting per-track
/// progress and checking cancellation between tracks.
fn decode_all<'job>(
    job: &'job MixdownJob,
    progress: &mut dyn FnMut(MixdownStage),
    cancel: &dyn Fn() -> bool,
) -> Result<Vec<(&'job MixdownTrack, DecodeAudio)>, Reported> {
    let registry = DecoderRegistry::with_symphonia();
    let mut decoded = Vec::with_capacity(job.tracks.len());
    for (i, track) in job.tracks.iter().enumerate() {
        if cancel() {
            return Err(Reported::Cancelled);
        }
        let audio = decode_one(&registry, track).map_err(Reported::Failed)?;
        decoded.push((track, audio));
        progress(MixdownStage::Decoding {
            done: i + 1,
            total: job.tracks.len(),
        });
    }
    Ok(decoded)
}

/// Reads and decodes one track's file.
fn decode_one(
    registry: &DecoderRegistry,
    track: &MixdownTrack,
) -> Result<DecodeAudio, Report<MixdownError>> {
    let bytes = std::fs::read(&track.path)
        .change_context(MixdownError)
        .attach(format!("cannot read {}", track.path.display()))?;
    let ext = track
        .path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default();
    registry
        .decode(&bytes, &ext)
        .change_context(MixdownError)
        .attach(format!("decode failed for {}", track.path.display()))
}

/// Stretched-session PCM provider: one stretched, cue-sliced,
/// stereo-normalized buffer per unique hash, built up front with
/// per-track progress.
struct MixdownPcm {
    pcms: std::collections::HashMap<String, Vec<f32>>,
}

/// Builds the provider from decoded audio and the plan's per-segment
/// stretch decisions. Ported from the CLI's `SessionPcm`: stretch per
/// the segment decision, slice from the stretched cue (the renderer
/// indexes segment PCM relative to the segment start), and duplicate
/// mono to interleaved stereo.
fn build_provider(
    decoded: &[(&MixdownTrack, DecodeAudio)],
    plan: &SessionPlan,
    progress: &mut dyn FnMut(MixdownStage),
    cancel: &dyn Fn() -> bool,
) -> Result<MixdownPcm, Reported> {
    let by_hash: std::collections::HashMap<&str, &DecodeAudio> = decoded
        .iter()
        .map(|(t, a)| (t.hash.0.as_str(), a))
        .collect();
    let mut pcms = std::collections::HashMap::new();
    // Unique-hash count (a duplicate playlist row shares the cache
    // entry; progress counts stretches actually performed).
    let total = plan
        .segments
        .iter()
        .map(|s| s.track_hash.0.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len();
    let mut done = 0;
    for seg in &plan.segments {
        if cancel() {
            return Err(Reported::Cancelled);
        }
        let hash = seg.track_hash.0.as_str();
        if pcms.contains_key(hash) {
            continue;
        }
        let Some(source_audio) = by_hash.get(hash) else {
            return Err(Reported::Failed(
                Report::new(MixdownError).attach(format!("no decoded audio for {hash}")),
            ));
        };
        let source = source_audio.samples.as_slice();
        let channels = usize::from(source_audio.channels.max(1));
        let stretched = stretch_segment(seg, source, channels);
        let sliced = cue_slice_and_stereo(&stretched, channels, seg);
        pcms.insert(hash.to_owned(), sliced);
        done += 1;
        progress(MixdownStage::Stretching { done, total });
    }
    Ok(MixdownPcm { pcms })
}

/// Stretches full-track PCM per the segment's stretch decision.
fn stretch_segment(
    seg: &crate::timeline::types::Segment,
    source: &[f32],
    channels: usize,
) -> Vec<f32> {
    match seg.stretch.mode {
        crate::timeline::types::StretchMode::Resample => {
            Resampler::new(seg.stretch).resample_all_frames(source, channels)
        }
        crate::timeline::types::StretchMode::Wsola => {
            Wsola::new(seg.stretch).stretch_all_frames(source, channels)
        }
    }
}

/// Slices from the stretched cue and normalizes to interleaved
/// stereo. `src_start` is in source *frames*; the stretch ratio maps
/// source frames to stretched frames directly.
fn cue_slice_and_stereo(
    stretched: &[f32],
    channels: usize,
    seg: &crate::timeline::types::Segment,
) -> Vec<f32> {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "cue bounded by stretched length"
    )]
    let cue_frames = ((f64::from(seg.stretch.ratio) * seg.src_start as f64).round() as usize)
        .min(stretched.len() / channels);
    let sliced_raw = &stretched[cue_frames * channels..];
    // The renderer is interleaved stereo; mono sources are
    // duplicated L==R here.
    if channels == 2 {
        sliced_raw.to_vec()
    } else {
        sliced_raw
            .iter()
            .flat_map(|&s| [s, s])
            .collect::<Vec<f32>>()
    }
}

impl TrackProvider for MixdownPcm {
    fn stretched_pcm(&mut self, hash: &TrackHash) -> Result<&[f32], TrackFetchError> {
        self.pcms
            .get(&hash.0)
            .map(Vec::as_slice)
            .ok_or(TrackFetchError)
    }

    fn name(&self) -> &'static str {
        "mixdown-pcm"
    }
}

/// Renders the whole session in chunks, appending each chunk's PCM
/// and reporting mixing progress.
fn render_mix(
    plan: &SessionPlan,
    mut provider: MixdownPcm,
    progress: &mut dyn FnMut(MixdownStage),
    cancel: &dyn Fn() -> bool,
) -> Result<Vec<f32>, Reported> {
    let total = plan.total_len_samples();
    let transition = crate::automation::transition_spec::TransitionSpec::default_pair();
    let mut renderer = Renderer::with_transition(plan.clone(), transition);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "session fits memory by construction"
    )]
    let mut mix: Vec<f32> = Vec::with_capacity(total as usize * 2);
    let mut pos = 0_u64;
    while pos < total {
        if cancel() {
            return Err(Reported::Cancelled);
        }
        let until = SessionTime((pos + MIX_CHUNK_FRAMES).min(total));
        let chunk = renderer
            .render_until(&mut provider, until)
            .map_err(|e| Reported::Failed(Report::new(e).change_context(MixdownError)))?;
        mix.extend_from_slice(&chunk);
        pos = until.0;
        #[expect(clippy::cast_precision_loss, reason = "progress display only")]
        let fraction = pos as f32 / total as f32;
        progress(MixdownStage::Mixing { fraction });
    }
    Ok(mix)
}

/// Writes the mix to a `.part` sibling, then atomically renames it to
/// the target.
fn write_wav_atomic(out: &Path, samples: &[f32], rate: u32) -> Result<(), Reported> {
    let part = part_path(out);
    write_wav(&part, samples, rate).map_err(Reported::Failed)?;
    std::fs::rename(&part, out)
        .change_context(MixdownError)
        .attach(format!("rename {} → {}", part.display(), out.display()))
        .map_err(Reported::Failed)
}

/// Writes a 32-bit float interleaved-stereo WAV.
fn write_wav(path: &Path, samples: &[f32], rate: u32) -> Result<(), Report<MixdownError>> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .change_context(MixdownError)
        .attach(format!("cannot create {}", path.display()))?;
    for s in samples {
        writer
            .write_sample(*s)
            .change_context(MixdownError)
            .attach(format!("write {}", path.display()))?;
    }
    writer
        .finalize()
        .change_context(MixdownError)
        .attach(format!("finalize {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::placement::grid_is_confident;

    // Given a canonical grid triple over 10 seconds.
    // When projected.
    // Then beats sit at the anchor plus beat multiples and
    // downbeats start at the phase offset.
    #[test]
    fn project_grid_builds_beats_and_downbeats_from_canonical_triple() {
        let grid = project_grid(128.0, 0.1, 2, 10.0);

        let beat = 60.0 / 128.0;
        assert!((grid.beats[0] - 0.1).abs() < 1e-4, "first beat at anchor");
        assert!((grid.beats[1] - (0.1 + beat)).abs() < 1e-4, "beat spacing");
        // Phase 2: the first downbeat is the k=2 beat.
        assert!((grid.downbeats[0] - (0.1 + 2.0 * beat)).abs() < 1e-4);
        // Downbeats are every 4th beat, offset by the phase.
        let downbeat_idx = grid
            .beats
            .iter()
            .position(|&t| (t - (0.1 + 2.0 * beat)).abs() < 1e-4)
            .expect("first downbeat is a beat");
        assert_eq!(downbeat_idx, 2, "phase 2 ⇒ third beat is a downbeat");
        assert_eq!(grid.downbeats.len(), (grid.beats.len() - 2).div_ceil(4));
        // No beat exceeds the duration.
        assert!(grid.beats.last().is_some_and(|&t| t <= 10.0));
        assert_eq!(grid.bars, grid.downbeats, "bars == downbeats");
    }

    // Given a negative anchor (grid starts before zero).
    // When projected.
    // Then the first beat is at or after zero and the phase math
    // still lands downbeats on k ≡ phase (mod 4).
    #[test]
    fn project_grid_handles_negative_anchor_before_zero() {
        let grid = project_grid(120.0, -0.3, 0, 5.0);

        assert!(
            grid.beats.first().is_some_and(|&t| t >= 0.0),
            "no negative times"
        );
        // Anchor -0.3 at 120 BPM: beat 0.5s; beats at ..., -0.3+1*0.5=0.2, ...
        assert!((grid.beats[0] - 0.2).abs() < 1e-4, "first in-range beat");
        // k of the first beat is 1; phase 0 ⇒ downbeats at k ≡ 0 (mod 4).
        assert!((grid.downbeats[0] - (-0.3 + 4.0 * 0.5)).abs() < 1e-4);
    }

    #[test]
    fn provider_slice_starts_at_planned_source_frame() {
        // Given stretched stereo PCM with distinct values per source frame and
        // a plan selecting source frame 2.
        let stretched = vec![
            0.0, 0.1, // frame 0
            1.0, 1.1, // frame 1
            2.0, 2.1, // frame 2
            3.0, 3.1, // frame 3
        ];
        let segment = crate::timeline::types::Segment {
            track_hash: TrackHash("slice".to_owned()),
            src_start: 2,
            session_start: SessionTime::ZERO,
            len_samples: 2,
            stretch: crate::timeline::types::StretchDecision::constant(
                crate::timeline::types::StretchMode::Resample,
                1.0,
                false,
            ),
            transition: None,
        };

        // When the provider slices the already-stretched track.
        let sliced = cue_slice_and_stereo(&stretched, 2, &segment);

        // Then it begins at the planned frame and does not reselect cues.
        assert_eq!(sliced, vec![2.0, 2.1, 3.0, 3.1]);
    }

    // Given a mixdown track snapshot and a decoded audio frame.
    // When the analysis is built.
    // Then the planner's confidence gate passes (vetted-grid
    // contract) and the cue snapshot is carried through.
    #[test]
    fn analysis_from_builds_a_confident_grid() {
        let snapshot = MixdownTrack {
            hash: TrackHash("deadbeef".to_owned()),
            path: PathBuf::from("/x.wav"),
            grid_bpm: 128.0,
            anchor_seconds: 0.1,
            downbeat_phase: 2,
            key: djcore::key::Key {
                root: 9,
                mode: djcore::key::KeyMode::Minor,
            },
            duration: 10.0,
            cues: CuePoints {
                ins: [Some(123), None, None, None],
                outs: [None, None, Some(4_567), None],
            },
        };
        let decoded = DecodeAudio {
            samples: vec![0.0; 44_100],
            sample_rate: 44_100,
            channels: 2,
        };

        let analysis = analysis_from(&snapshot, &decoded);

        assert!(
            grid_is_confident(&analysis.beat_grid, analysis.grid_stability),
            "vetted grids always plan as confident"
        );
        assert_eq!(analysis.hash.0, "deadbeef");
        assert!((analysis.bpm - 128.0).abs() < f32::EPSILON, "bpm from grid");
        assert_eq!(
            analysis.cues, snapshot.cues,
            "cue snapshot reaches planning"
        );
    }
}
