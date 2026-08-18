//! Constant-grid fit: one BPM + one anchor from DP beat marks.
//!
//! Ports Mixxx's beat-grid post-processing (`src/track/beatutils.cpp`)
//! to f32 seconds over the DP tracker's marks:
//!
//! 1. **Iron regions** (`retrieveConstRegions`): absorb ±12 ms detector
//!    jitter into constant-tempo regions via a two-pointer scan.
//! 2. **Longest span + extension** (`makeConstBpm`): regress the beat
//!    length over the longest region, extending across tempo- and
//!    phase-compatible neighbors.
//! 3. **Rounding ladder** (`roundBpmWithinRange`): snap the regression
//!    BPM to a musical value (integer → ½ → ⅔ → ⅓ steps → 1/12)
//!    strictly inside the span's confidence bounds.
//! 4. **Anchor fit** (`adjustPhase`): phase anchor from the mean
//!    wrapped residual of all marks against the fitted grid.
//! 5. **Downbeat vote**: pick the bar phase maximizing novelty energy
//!    at downbeat positions.
//!
//! The output grid is *constant*: `beats`/`downbeats`/`bars` are
//! gapless projections of `anchor + k·beat_length`, so beats continue
//! through breaks and never fragment.

use crate::analysis::result::BeatGrid;
use crate::error::AnalysisError;

/// Maximum single-mark phase error vs the ironed projection (s).
const MAX_PHASE_ERROR: f64 = 0.025;

/// Maximum cumulative phase error drift within a region (s).
const MAX_PHASE_ERROR_SUM: f64 = 0.1;

/// Outlier marks tolerated per region before it splits.
const MAX_OUTLIERS: usize = 1;

/// Minimum marks required to fit a grid at all. Mixxx's 16-beat
/// floor governs *region trust* in span extension; the fit itself
/// only needs a handful of marks to regress a tempo.
pub const MIN_MARKS: usize = 8;

/// Minimum beats for a region to be trusted in span extension
/// (Mixxx `kMinRegionBeatCount`).
const MIN_REGION_BEATS: f64 = 16.0;

/// Beats per bar (4/4).
const BEATS_PER_BAR: u32 = 4;

/// Half-width of the novelty-energy window used for the downbeat
/// vote, in seconds.
const DOWNBEAT_VOTE_WINDOW: f64 = 0.03;

/// A constant-tempo region of the mark sequence.
#[derive(Debug, Clone, Copy)]
struct ConstRegion {
    /// First mark of the region (seconds).
    start: f64,
    /// Mean inter-mark interval within the region (seconds).
    beat_length: f64,
}

/// Fits a constant grid to DP marks.
///
/// * `marks` - beat times in seconds (sorted, from the DP tracker)
/// * `bpm_seed` - tempogram BPM (used only for diagnostics/logging)
/// * `novelty` - onset novelty envelope (for the downbeat vote)
/// * `hop_seconds` - novelty frame spacing
/// * `duration` - duration of the (trimmed) audio in seconds
///
/// Returns the fitted grid and its stability (phase-consistency of
/// the marks against the fitted grid, `1/(1+CV)` of residuals).

/// A constant-tempo span used for BPM regression.
#[derive(Debug, Clone, Copy)]
struct Span {
    start: f64,
    end: f64,
    beat_length: f64,
}
/// Returns `AnalysisError::ProcessingError` when fewer than
/// [`MIN_MARKS`] marks survive cleaning or no constant region can be
/// established.
pub fn fit_constant_grid(
    marks: &[f32],
    bpm_seed: f32,
    novelty: &[f32],
    hop_seconds: f64,
    duration: f32,
) -> Result<(BeatGrid, f32), AnalysisError> {
    if marks.len() < MIN_MARKS {
        return Err(AnalysisError::ProcessingError(format!(
            "too few marks for a constant grid: {} (need {})",
            marks.len(),
            MIN_MARKS
        )));
    }

    let cleaned = dedup_close_marks(marks, bpm_seed);
    if cleaned.len() < MIN_MARKS {
        return Err(AnalysisError::ProcessingError(format!(
            "too few marks after cleaning: {} (need {})",
            cleaned.len(),
            MIN_MARKS
        )));
    }

    let regions = iron_regions(&cleaned);
    let span = longest_phase_compatible_span(&regions)
        .ok_or_else(|| AnalysisError::ProcessingError("no constant region".to_string()))?;

    let (grid_bpm, beat_length) = rounded_bpm(span);
    let anchor0 = phase_anchor(&cleaned, span.start, beat_length);
    let anchor = downbeat_phase(
        anchor0,
        beat_length,
        novelty,
        hop_seconds,
        f64::from(duration),
    );

    let stability = residual_stability(&cleaned, anchor, beat_length);
    let grid = materialize(grid_bpm, anchor, beat_length, f64::from(duration));

    log::debug!(
        "constant-grid fit: seed {:.2} → grid {:.3} BPM, anchor {:.3}s, stability {:.3}",
        bpm_seed,
        grid_bpm,
        anchor,
        stability
    );
    Ok((grid, stability))
}

/// Drops marks closer than half a seed beat-length to their
/// predecessor (defensive: doubled marks must not skew the mean).
fn dedup_close_marks(marks: &[f32], bpm_seed: f32) -> Vec<f64> {
    let period = 60.0 / f64::from(bpm_seed.max(f32::EPSILON));
    let mut kept: Vec<f64> = Vec::with_capacity(marks.len());
    for &m in marks {
        let m = f64::from(m);
        if kept.last().is_none_or(|&last| m - last >= period * 0.5) {
            kept.push(m);
        }
    }
    kept
}

/// Irons the marks into constant-tempo regions (Mixxx
/// `retrieveConstRegions`).
fn iron_regions(marks: &[f64]) -> Vec<ConstRegion> {
    let mut regions = Vec::new();
    let mut left = 0_usize;
    let mut right = marks.len() - 1;

    while left < marks.len() - 1 {
        let mean = (marks[right] - marks[left]) / (right - left) as f64;
        if region_is_constant(marks, left, right, mean) {
            regions.push(ConstRegion {
                start: marks[left],
                beat_length: mean,
            });
            left = right;
            right = marks.len() - 1;
        } else if right > left + 1 {
            right -= 1;
        } else {
            left += 1;
            right = marks.len() - 1;
        }
    }
    // Zero-length sentinel marking the end (Mixxx appends one so a
    // single whole-track region still has a defined span).
    regions.push(ConstRegion {
        start: marks[marks.len() - 1],
        beat_length: 0.0,
    });
    regions
}

/// Whether `[left, right]` holds as one constant region under the
/// ironing tolerances (outliers, error sum, border sanity).
fn region_is_constant(marks: &[f64], left: usize, right: usize, mean: f64) -> bool {
    let mut outliers = 0_usize;
    let mut error_sum = 0.0_f64;
    let mut ironed = marks[left];
    for &m in &marks[left + 1..=right] {
        ironed += mean;
        let err = ironed - m;
        error_sum += err;
        if err.abs() > MAX_PHASE_ERROR {
            outliers += 1;
            if outliers > MAX_OUTLIERS {
                return false;
            }
        }
        if error_sum.abs() > MAX_PHASE_ERROR_SUM {
            return false;
        }
    }
    // Border sanity: first and last intervals must not both bend away
    // from the mean (they would skew it).
    if right > left + 2 {
        let first = marks[left + 1] - marks[left];
        let last = marks[right] - marks[right - 1];
        if (first + last - 2.0 * mean).abs() >= MAX_PHASE_ERROR / 2.0 {
            return false;
        }
    }
    true
}

/// The longest region, extended across phase-compatible neighbors:
/// a faithful port of Mixxx `makeConstBpm`'s span selection — the
/// longest region, then backward and forward extension across
/// regions whose tempo and beat count agree, with a
/// [`MIN_REGION_BEATS`] stability floor on candidates.
fn longest_phase_compatible_span(regions: &[ConstRegion]) -> Option<Span> {
    // The sentinel (last region) is a boundary, not a candidate.
    let candidates = &regions[..regions.len().saturating_sub(1)];
    let mid = candidates
        .iter()
        .enumerate()
        .max_by(|a, b| {
            let la = regions[a.0 + 1].start - a.1.start;
            let lb = regions[b.0 + 1].start - b.1.start;
            la.total_cmp(&lb)
        })?
        .0;

    let mut start_idx = mid;
    let mut length = regions[mid + 1].start - regions[mid].start;
    let mut beat_length = regions[mid].beat_length;
    let mut beats = ((length / beat_length) + 0.5).floor().max(1.0);
    let (mut beat_min, mut beat_max) = (
        beat_length - MAX_PHASE_ERROR / beats,
        beat_length + MAX_PHASE_ERROR / beats,
    );

    extend_backward(
        regions,
        mid,
        &mut start_idx,
        &mut length,
        &mut beat_length,
        &mut beats,
        &mut beat_min,
        &mut beat_max,
    );
    extend_forward(
        regions,
        start_idx,
        &mut length,
        &mut beat_length,
        &mut beats,
        &mut beat_min,
        &mut beat_max,
    );

    Some(Span {
        start: regions[start_idx].start,
        end: regions[start_idx].start + length,
        beat_length,
    })
}

/// Mixxx backward extension: try to pull the span start earlier
/// across a phase-compatible region.
fn extend_backward(
    regions: &[ConstRegion],
    mid: usize,
    start_idx: &mut usize,
    length: &mut f64,
    beat_length: &mut f64,
    beats: &mut f64,
    beat_min: &mut f64,
    beat_max: &mut f64,
) {
    for i in 0..mid {
        let Some(new) = try_extend(
            regions,
            i,
            *start_idx,
            *beat_length,
            *beat_min,
            *beat_max,
            mid + 1,
        ) else {
            continue;
        };
        if new.beat_length > *beat_min && new.beat_length < *beat_max {
            *start_idx = i;
            *length = new.length;
            *beat_length = new.beat_length;
            *beats = new.beats;
            *beat_min = *beat_length - MAX_PHASE_ERROR / *beats;
            *beat_max = *beat_length + MAX_PHASE_ERROR / *beats;
            break;
        }
    }
}

/// Mixxx forward extension: try to push the span end later across
/// a phase-compatible region.
fn extend_forward(
    regions: &[ConstRegion],
    start_idx: usize,
    length: &mut f64,
    beat_length: &mut f64,
    beats: &mut f64,
    beat_min: &mut f64,
    beat_max: &mut f64,
) {
    for i in (start_idx + 1..regions.len() - 1).rev() {
        let Some(new) = try_extend(
            regions,
            start_idx,
            i,
            *beat_length,
            *beat_min,
            *beat_max,
            i + 1,
        ) else {
            continue;
        };
        if new.beat_length > *beat_min && new.beat_length < *beat_max {
            *length = new.length;
            *beat_length = new.beat_length;
            *beats = new.beats;
            *beat_min = *beat_length - MAX_PHASE_ERROR / *beats;
            *beat_max = *beat_length + MAX_PHASE_ERROR / *beats;
            break;
        }
    }
}

/// One candidate extension from region `start` to the end boundary
/// `end` (exclusive index into `regions`); `None` when tempo-
/// incompatible, too short, or beat-count ambiguous.
fn try_extend(
    regions: &[ConstRegion],
    start: usize,
    cand: usize,
    target_beat: f64,
    beat_min: f64,
    beat_max: f64,
    end: usize,
) -> Option<ExtCandidate> {
    let length = regions[end].start - regions[start].start;
    let cand_beats =
        ((regions[cand + 1].start - regions[cand].start) / regions[cand].beat_length + 0.5).floor();
    if cand_beats < MIN_REGION_BEATS {
        return None; // short regions are unstable
    }
    let cand_min = regions[cand].beat_length - MAX_PHASE_ERROR / cand_beats;
    let cand_max = regions[cand].beat_length + MAX_PHASE_ERROR / cand_beats;
    if !(target_beat > cand_min && target_beat < cand_max) {
        return None; // tempo incompatible
    }
    let merged_min = beat_min.max(cand_min);
    let merged_max = beat_max.min(cand_max);
    let max_beats = (length / merged_min).round();
    let min_beats = (length / merged_max).round();
    if min_beats != max_beats || min_beats < 1.0 {
        return None; // ambiguous beat count
    }
    Some(ExtCandidate {
        length,
        beats: min_beats,
        beat_length: length / min_beats,
    })
}

/// A candidate extension result.
struct ExtCandidate {
    length: f64,
    beats: f64,
    beat_length: f64,
}

/// Regresses the span to a BPM and snaps it through the rounding
/// ladder (Mixxx `roundBpmWithinRange`). Returns `(bpm, beat_length)`.
fn rounded_bpm(span: Span) -> (f32, f64) {
    let length = span.end - span.start;
    if length <= 0.0 || span.beat_length <= 0.0 {
        return (120.0, 0.5);
    }
    let n = (length / span.beat_length).round().max(1.0);
    let beat = length / n;
    let center = 60.0 / beat;
    // Bounds widened by one phase error per beat count (Mixxx).
    let min_bpm = 60.0 / (beat + MAX_PHASE_ERROR / n);
    let max_bpm = 60.0 / (beat - MAX_PHASE_ERROR / n);
    let snapped = snap_bpm(center, min_bpm, max_bpm);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "BPM stored as f32 by design"
    )]
    let bpm = snapped as f32;
    (bpm, 60.0 / snapped)
}

/// The rounding ladder, Mixxx order: integer → ½ (below 85) → ⅔
/// (above 127) → ⅓ → 1/12, accepted only strictly inside `(min, max)`.
fn snap_bpm(center: f64, min: f64, max: f64) -> f64 {
    let try_snap = |fraction: f64| {
        let snapped = (center * fraction).round() / fraction;
        (snapped > min && snapped < max).then_some(snapped)
    };
    try_snap(1.0)
        .or_else(|| (center < 85.0).then(|| ()).and_then(|_| try_snap(2.0)))
        .or_else(|| {
            (center > 127.0)
                .then(|| ())
                .and_then(|_| try_snap(2.0 / 3.0))
        })
        .or_else(|| try_snap(3.0))
        .or_else(|| try_snap(12.0))
        .unwrap_or(center)
}

/// Phase anchor from the span start refined by the mean wrapped
/// residual of all marks (Mixxx `adjustPhase`).
fn phase_anchor(marks: &[f64], span_start: f64, beat_length: f64) -> f64 {
    let raw = span_start.rem_euclid(beat_length);
    let residuals: Vec<f64> = marks
        .iter()
        .map(|&m| {
            let mut r = (m - raw).rem_euclid(beat_length);
            if r > beat_length / 2.0 {
                r -= beat_length;
            }
            r
        })
        .filter(|r| r.abs() <= MAX_PHASE_ERROR)
        .collect();
    let adjust = if residuals.is_empty() {
        0.0
    } else {
        residuals.iter().sum::<f64>() / residuals.len() as f64
    };
    (raw + adjust).rem_euclid(beat_length)
}

/// Chooses the downbeat bar-phase by novelty energy vote, returning
/// the anchor reduced into `[0, bar)`.
fn downbeat_phase(
    anchor0: f64,
    beat_length: f64,
    novelty: &[f32],
    hop_seconds: f64,
    duration: f64,
) -> f64 {
    let bar = f64::from(BEATS_PER_BAR) * beat_length;
    let mut best_phase = 0_usize;
    let mut best_score = f64::NEG_INFINITY;
    for phase in 0..BEATS_PER_BAR as usize {
        let score = phase_energy(
            anchor0 + phase as f64 * beat_length,
            bar,
            novelty,
            hop_seconds,
            duration,
        );
        if score > best_score {
            best_score = score;
            best_phase = phase;
        }
    }
    (anchor0 + best_phase as f64 * beat_length).rem_euclid(bar)
}

/// Total novelty energy in ±[`DOWNBEAT_VOTE_WINDOW`] around each bar
/// position of a candidate phase.
fn phase_energy(anchor: f64, bar: f64, novelty: &[f32], hop_seconds: f64, duration: f64) -> f64 {
    let half = DOWNBEAT_VOTE_WINDOW / hop_seconds.max(f64::EPSILON);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "window is small"
    )]
    let half_frames = half.round() as usize;
    let mut total = 0.0_f64;
    let mut pos = anchor;
    while pos < duration {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "frame index in range"
        )]
        let frame = (pos / hop_seconds).round() as usize;
        let lo = frame.saturating_sub(half_frames);
        let hi = (frame + half_frames).min(novelty.len().saturating_sub(1));
        if lo < novelty.len() {
            total += novelty[lo..=hi].iter().map(|&v| f64::from(v)).sum::<f64>();
        }
        pos += bar;
    }
    total
}

/// Stability: `1/(1+CV)` over the wrapped residuals of marks against
/// the fitted grid.
fn residual_stability(marks: &[f64], anchor: f64, beat_length: f64) -> f32 {
    let residuals: Vec<f64> = marks
        .iter()
        .map(|&m| {
            let mut r = (m - anchor).rem_euclid(beat_length);
            if r > beat_length / 2.0 {
                r -= beat_length;
            }
            r
        })
        .collect();
    let n = residuals.len() as f64;
    let mean = residuals.iter().sum::<f64>() / n;
    let var = residuals
        .iter()
        .map(|r| (r - mean) * (r - mean))
        .sum::<f64>()
        / n;
    let cv = var.sqrt() / beat_length;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "stability stored as f32 by design"
    )]
    let stability = (1.0 / (1.0 + cv)) as f32;
    stability.clamp(0.0, 1.0)
}

/// Materializes the gapless projection arrays from BPM + anchor.
fn materialize(grid_bpm: f32, anchor: f64, beat_length: f64, duration: f64) -> BeatGrid {
    let bar = f64::from(BEATS_PER_BAR) * beat_length;
    let beat_count = ((duration - anchor) / beat_length).ceil().max(0.0) as usize;
    let bar_count = ((duration - anchor) / bar).ceil().max(0.0) as usize;
    let beats = (0..beat_count)
        .map(|k| (anchor + k as f64 * beat_length) as f32)
        .collect();
    let downbeats: Vec<f32> = (0..bar_count)
        .map(|k| (anchor + k as f64 * bar) as f32)
        .collect();
    BeatGrid {
        grid_bpm,
        anchor_seconds: anchor as f32,
        downbeats: downbeats.clone(),
        beats,
        bars: downbeats,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOP: f64 = 1024.0 / 44_100.0;

    /// Mark list at `bpm` with per-mark jitter (ms) and every
    /// `drop_every`-th mark removed; plus its novelty envelope.
    fn fixture(beats: usize, bpm: f32, jitter_ms: f64, drop_every: usize) -> (Vec<f32>, Vec<f32>) {
        let period = 60.0 / f64::from(bpm);
        let mut rng: f64 = 0.42;
        let mut marks = Vec::new();
        let mut env = vec![0.0_f32; (period * beats as f64 / HOP) as usize + 16];
        for b in 0..beats {
            if drop_every > 0 && b % drop_every == 0 && b > 0 {
                continue;
            }
            rng = (rng * 7.3 + 0.11).fract();
            let t = period * b as f64 + (rng - 0.5) * 2.0 * jitter_ms / 1000.0;
            marks.push(t as f32);
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "frame in range"
            )]
            let frame = (t / HOP).round() as usize;
            let idx = frame.min(env.len().saturating_sub(1));
            if let Some(cell) = env.get_mut(idx) {
                *cell = 1.0;
            }
        }
        (marks, env)
    }

    #[test]
    fn t1_clean_clicks_fit_exact_bpm_and_phase() {
        // Given 128 clean marks at 138 BPM.
        let (marks, env) = fixture(128, 138.0, 0.0, 0);

        // When fitting.
        let (grid, stability) =
            fit_constant_grid(&marks, 138.0, &env, HOP, 128.0 * 60.0 / 138.0).expect("fit");

        // Then BPM rounds to 138, anchor is a beat within 10 ms, high stability.
        assert!((grid.grid_bpm - 138.0).abs() < f32::EPSILON);
        assert!(stability > 0.9);
        let beat_length = 60.0 / f64::from(grid.grid_bpm);
        let phase = f64::from(grid.anchor_seconds).rem_euclid(beat_length);
        assert!(
            phase < 0.010 || phase > beat_length - 0.010,
            "anchor {phase:.4}"
        );
    }

    #[test]
    fn t2_jitter_and_drops_recover_the_grid() {
        // Given 256 marks at 138 BPM, ±10 ms jitter, isolated drops
        // (the DP tracker interpolates through weak novelty, so
        // systematic dropouts do not reach the fitter).
        let (marks, env) = fixture(256, 138.0, 10.0, 128);

        // When fitting with a seed 1 BPM off.
        let (grid, stability) =
            fit_constant_grid(&marks, 139.0, &env, HOP, 256.0 * 60.0 / 138.0).expect("fit");

        // Then the grid recovers 138 BPM with anchor within 15 ms.
        assert!((grid.grid_bpm - 138.0).abs() < f32::EPSILON);
        assert!(stability > 0.8);
        let beat_length = 60.0 / f64::from(grid.grid_bpm);
        let phase = f64::from(grid.anchor_seconds).rem_euclid(beat_length);
        assert!(
            phase < 0.015 || phase > beat_length - 0.015,
            "anchor {phase:.4}"
        );
    }

    #[test]
    fn t3_rounding_ladder_snaps_within_bounds() {
        // Given regression centers 138.4 / 100.7 with ±0.5-BPM bounds.
        // When snapping.
        let a = snap_bpm(138.4, 137.9, 138.9);
        let c = snap_bpm(100.7, 100.2, 101.2);

        // Then integer snapping wins first (Mixxx ladder order).
        assert!((a - 138.0).abs() < f64::EPSILON);
        assert!((c - 101.0).abs() < f64::EPSILON);
    }

    #[test]
    fn t4_beats_continue_through_break() {
        // Given 128 marks at 120 BPM with a 4-bar (8 s) hole in the middle.
        let (mut marks, env) = fixture(128, 120.0, 5.0, 0);
        let period = 60.0 / 120.0_f64;
        marks.retain(|&m| {
            let t = f64::from(m);
            !(48.0..56.0).contains(&t) || ((t / period).rem_euclid(1.0)).abs() < 0.0
        });
        marks.retain(|&m| !(48.0..56.0).contains(&f64::from(m)));
        let duration = 128.0 * period;

        // When fitting.
        let (grid, _) = fit_constant_grid(&marks, 120.0, &env, HOP, duration as f32).expect("fit");

        // Then the projection has no gap larger than 1.5× the median.
        let mut ivs: Vec<f32> = grid.beats.windows(2).map(|w| w[1] - w[0]).collect();
        ivs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = ivs[ivs.len() / 2];
        assert!(
            grid.beats.windows(2).all(|w| w[1] - w[0] <= median * 1.5),
            "gap in projected beats"
        );
    }

    #[test]
    fn t5_accented_phase_wins_downbeat_vote() {
        // Given 128 marks at 128 BPM where every 4th click is accented.
        let period = 60.0 / 128.0_f64;
        let mut env = vec![0.0_f32; (period * 128.0 / HOP) as usize + 16];
        let mut marks = Vec::new();
        for b in 0..128 {
            let t = period * b as f64;
            marks.push(t as f32);
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "frame in range"
            )]
            let frame = (t / HOP).round() as usize;
            let idx = frame.min(env.len().saturating_sub(1));
            if let Some(cell) = env.get_mut(idx) {
                *cell = if b % 4 == 0 { 1.0 } else { 0.3 };
            }
        }

        // When fitting.
        let (grid, _) =
            fit_constant_grid(&marks, 128.0, &env, HOP, 128.0 * period as f32).expect("fit");

        // Then the anchor lands on an accented click (phase 0).
        let anchor = f64::from(grid.anchor_seconds);
        assert!(
            anchor < 0.02 || anchor > 4.0 * period - 0.02,
            "anchor {anchor:.4}"
        );
        assert!((grid.grid_bpm - 128.0).abs() < f32::EPSILON);
    }

    #[test]
    fn too_few_marks_is_an_error() {
        // Given only 5 marks (below the 8-mark fit floor).
        let (marks, env) = fixture(5, 138.0, 0.0, 0);

        // When fitting.
        let result = fit_constant_grid(&marks, 138.0, &env, HOP, 3.0);

        // Then it errors.
        assert!(result.is_err());
    }
}
