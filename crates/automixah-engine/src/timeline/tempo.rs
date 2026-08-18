//! Tempo normalization: octave folding and target-BPM selection.
//!
//! stratum-dsp reports tempo at whichever octave its tempogram locked
//! onto (60, 120, 240... are all "the same" tempo). The planner folds
//! every BPM into `[90, 180)` before comparing tracks or choosing a
//! session target.

/// Folds a raw detected BPM into the `[90, 180)` range by halving or
/// doubling.
///
/// # Examples
///
/// - `60` → `120`, `240` → `120` (both fold to the same tempo class)
/// - `100` → `100` (already in range)
/// - `85` → `170` (up one octave rather than staying below 90)
#[must_use]
pub fn fold_bpm(bpm: f32) -> f32 {
    let mut v = bpm.max(f32::EPSILON);
    while v < 90.0 {
        v *= 2.0;
    }
    while v >= 180.0 {
        v /= 2.0;
    }
    v
}

/// Selects the session target BPM from the playlist.
///
/// Zero-config default: the median of octave-normalized BPMs — robust
/// against outliers (one 174 BPM track in a 128 BPM playlist does not
/// drag the target). A user override, when present, is returned as-is.
///
/// Returns `None` for an empty playlist.
#[must_use]
pub fn select_target_bpm(bpms: &[f32], user_override: Option<f32>) -> Option<f32> {
    if let Some(explicit) = user_override {
        return Some(explicit);
    }
    if bpms.is_empty() {
        return None;
    }

    let mut folded: Vec<f32> = bpms.iter().map(|b| fold_bpm(*b)).collect();
    folded.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(median_of_sorted(&folded))
}

fn median_of_sorted(sorted: &[f32]) -> f32 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        f32::midpoint(sorted[n / 2 - 1], sorted[n / 2])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_halves_tempos_above_range() {
        // Given 240 BPM.
        // When folding.
        let folded = fold_bpm(240.0);

        // Then it lands at 120.
        assert!((folded - 120.0).abs() < f32::EPSILON);
    }

    #[test]
    fn fold_doubles_tempos_below_range() {
        // Given 60 BPM.
        // When folding.
        let folded = fold_bpm(60.0);

        // Then it lands at 120.
        assert!((folded - 120.0).abs() < f32::EPSILON);
    }

    #[test]
    fn fold_leaves_in_range_tempos_alone() {
        // Given 128 BPM (already in [90, 180)).
        // When folding.
        let folded = fold_bpm(128.0);

        // Then it is unchanged.
        assert!((folded - 128.0).abs() < f32::EPSILON);
    }

    #[test]
    fn fold_moves_85_up_an_octave() {
        // Given 85 BPM (below the floor).
        // When folding.
        let folded = fold_bpm(85.0);

        // Then it doubles to 170, staying inside the range.
        assert!((folded - 170.0).abs() < f32::EPSILON);
    }

    #[test]
    fn fold_octave_equivalents_collapse() {
        // Given the same tempo at three octaves.
        let a = fold_bpm(60.0);
        let b = fold_bpm(120.0);
        let c = fold_bpm(240.0);

        // Then they all fold to the same value.
        assert!((a - b).abs() < f32::EPSILON);
        assert!((b - c).abs() < f32::EPSILON);
    }

    #[test]
    fn target_bpm_is_median_for_odd_count() {
        // Given BPMs whose median is the middle value.
        let bpms = [120.0, 128.0, 140.0];

        // When selecting a target.
        let target = select_target_bpm(&bpms, None);

        // Then the median is chosen.
        assert_eq!(target, Some(128.0));
    }

    #[test]
    fn target_bpm_is_mean_of_middle_pair_for_even_count() {
        // Given an even count of BPMs.
        let bpms = [120.0, 124.0, 128.0, 132.0];

        // When selecting a target.
        let target = select_target_bpm(&bpms, None);

        // Then the mean of the middle pair is chosen.
        assert_eq!(target, Some(126.0));
    }

    #[test]
    fn target_bpm_ignores_outliers_via_median() {
        // Given a 128-BPM playlist with one 174 BPM outlier.
        let bpms = [127.0, 128.0, 129.0, 128.5, 174.0];

        // When selecting a target.
        let target = select_target_bpm(&bpms, None);

        // Then the outlier does not drag the target.
        assert_eq!(target, Some(128.5));
    }

    #[test]
    fn target_bpm_folds_before_median() {
        // Given the same tempo detected at different octaves.
        let bpms = [60.0, 120.0, 240.0];

        // When selecting a target.
        let target = select_target_bpm(&bpms, None);

        // Then all fold to 120 and the target is 120.
        assert_eq!(target, Some(120.0));
    }

    #[test]
    fn target_bpm_user_override_wins() {
        // Given a playlist and an explicit user target.
        let bpms = [120.0, 128.0, 140.0];

        // When selecting with an override.
        let target = select_target_bpm(&bpms, Some(100.0));

        // Then the override is returned as-is.
        assert_eq!(target, Some(100.0));
    }

    #[test]
    fn target_bpm_empty_playlist_returns_none() {
        // Given no tracks.
        // When selecting a target.
        let target = select_target_bpm(&[], None);

        // Then there is no target.
        assert_eq!(target, None);
    }
}
