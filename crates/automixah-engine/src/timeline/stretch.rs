//! Per-track stretch decisions: mapping a track's tempo onto the session
//! BPM and choosing the time-scaling mode.

use crate::timeline::tempo::fold_bpm;
use crate::timeline::types::{StretchDecision, StretchMode};

/// Comfort band half-width: within ±8% a track is pitch-adjusted
/// (resampled) rather than time-stretched.
pub const COMFORT_BAND: f32 = 0.08;

/// Decides how a track at raw detected `track_bpm` (source sample rate
/// `src_rate`) is matched onto `target_bpm` (engine sample rate
/// `engine_rate`).
///
/// The ratio folds `track_bpm` into `[90, 180)` first, then composes
/// tempo matching with any rate conversion:
///
/// ```text
/// ratio = (folded_bpm / target_bpm) * (engine_rate / src_rate)
/// ```
///
/// The stretch mode follows the comfort band: `|ratio - 1| <= 8%` →
/// [`StretchMode::Resample`] (pitch-adjusted, cheap, sample-exact);
/// otherwise [`StretchMode::Wsola`] (pitch-preserving). The decision
/// also exposes whether the *tempo* delta alone exceeds the band — what
/// the UI tints — independent of rate conversion.
#[must_use]
pub fn decide_stretch(
    track_bpm: f32,
    target_bpm: f32,
    src_rate: u32,
    engine_rate: u32,
) -> StretchDecision {
    let folded = fold_bpm(track_bpm);
    let tempo_ratio = folded / target_bpm;
    let rate_ratio = f64::from(engine_rate) / f64::from(src_rate);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "ratio composed in f64, stored as f32 by design"
    )]
    let ratio = (f64::from(tempo_ratio) * rate_ratio) as f32;

    StretchDecision::constant(
        StretchMode::for_ratio(tempo_ratio),
        ratio,
        (tempo_ratio - 1.0).abs() > COMFORT_BAND + f32::EPSILON,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_band_track_uses_resample() {
        // Given a 128 BPM track and a 126 target (1.6% delta).
        // When deciding the stretch.
        let d = decide_stretch(128.0, 126.0, 44_100, 44_100);

        // Then resample is chosen with no out-of-band flag.
        assert_eq!(d.mode, StretchMode::Resample);
        assert!(!d.out_of_comfort_band);
        assert!((d.ratio - 128.0 / 126.0).abs() < 1e-6);
    }

    #[test]
    fn out_of_band_track_uses_wsola() {
        // Given a 174 BPM track and a 128 target.
        // When deciding the stretch.
        let d = decide_stretch(174.0, 128.0, 44_100, 44_100);

        // Then WSOLA is chosen and the UI flag is set.
        assert_eq!(d.mode, StretchMode::Wsola);
        assert!(d.out_of_comfort_band);
    }

    #[test]
    fn ratio_includes_rate_conversion() {
        // Given a 48 kHz track in a 44.1 kHz engine at matching tempo.
        // When deciding the stretch.
        let d = decide_stretch(120.0, 120.0, 48_000, 44_100);

        // Then the ratio composes tempo and rate conversion.
        assert!((d.ratio - 44_100.0 / 48_000.0).abs() < 1e-6);
    }

    #[test]
    fn bpm_folds_before_ratio() {
        // Given a track detected at 60 BPM (same tempo as 120).
        // When deciding against a 120 target.
        let d = decide_stretch(60.0, 120.0, 44_100, 44_100);

        // Then the ratio is unity — the octaves cancel.
        assert!((d.ratio - 1.0).abs() < 1e-6);
        assert_eq!(d.mode, StretchMode::Resample);
    }

    #[test]
    fn exactly_at_band_edge_is_in_band() {
        // Given a tempo delta of exactly +8%.
        let target = 120.0;
        let track = target / 1.08;

        // When deciding the stretch.
        let d = decide_stretch(track, target, 44_100, 44_100);

        // Then it is treated as in-band (resample).
        assert_eq!(d.mode, StretchMode::Resample);
        assert!(!d.out_of_comfort_band);
    }
}
