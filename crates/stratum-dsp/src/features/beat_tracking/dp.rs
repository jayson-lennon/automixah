//! Dynamic-programming beat tracker (Ellis-style).
//!
//! Marks beats by finding the globally optimal sequence through an
//! onset novelty envelope, biased toward a seed period from the
//! tempogram BPM estimate. Unlike the HMM it replaces, the DP score
//! is carried *on the envelope peaks*, so a seed BPM that is off by
//! a percent does not destroy the path: the log-Gaussian period
//! prior tolerates the error while the envelope keeps the beats
//! anchored to actual onsets.
//!
//! Reference: Ellis, D. (2007). *Beat Tracking by Dynamic
//! Programming*. JASA.

/// Prior tightness: how strongly the period window penalizes
/// deviations from the seed period (librosa's default 100).
const TIGHTNESS: f64 = 100.0;

/// Minimum envelope length (in seed periods) required to track.
const MIN_PERIODS: usize = 8;

/// Leading/trailing marks with envelope strength below this fraction
/// of the peak are trimmed (beats projected into silence).
const TRIM_FRACTION: f32 = 0.05;

/// Tracks beats through a novelty envelope.
///
/// `novelty` is one onset-strength value per analysis frame;
/// `hop_seconds` is the frame spacing; `bpm_seed` biases the period
/// prior. Returns beat times in seconds (frame-quantized), sorted.
/// Returns an empty vector when the envelope is too short to hold
/// [`MIN_PERIODS`] seed periods.
#[must_use]
pub fn track_beats_dp(novelty: &[f32], hop_seconds: f64, bpm_seed: f32) -> Vec<f32> {
    let period = 60.0 / f64::from(bpm_seed.max(f32::EPSILON));
    let period_frames = (period / hop_seconds.max(f64::EPSILON)).round().max(2.0);
    #[expect(clippy::cast_precision_loss, reason = "frame count is small")]
    let min_frames = MIN_PERIODS as f64 * period_frames;
    if f64::from(novelty.len().min(u16::MAX as usize) as f32) < min_frames
        || (novelty.len() as f64) < min_frames
    {
        return Vec::new();
    }

    let peak = novelty.iter().fold(0.0_f32, |m, &v| v.max(m));
    if peak <= f32::EPSILON {
        return Vec::new();
    }
    let env = normalized(novelty, peak);

    let (cumscore, backlink) = dp_pass(&env, period_frames);
    backtrack(&cumscore, &backlink, &env, peak, hop_seconds)
}

/// Scales the envelope so its peak is 1.
fn normalized(novelty: &[f32], peak: f32) -> Vec<f32> {
    novelty.iter().map(|&v| v / peak).collect()
}

/// Forward DP pass: cumulative score and predecessor links.
///
/// `cumscore[t] = env[t] + max_lag (txwt(lag) + cumscore[t-lag])`
/// with `txwt(lag) = -TIGHTNESS · ln(lag/period)²` and the lag
/// window `[ceil(period/2), floor(2·period)]`. Links of `usize::MAX`
/// mean "path starts here".
fn dp_pass(env: &[f32], period_frames: f64) -> (Vec<f32>, Vec<usize>) {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "lag is small"
    )]
    let min_lag = (period_frames / 2.0).ceil().max(1.0) as usize;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "lag is small"
    )]
    let max_lag = (2.0 * period_frames).floor() as usize;

    let mut cumscore = vec![0.0_f32; env.len()];
    let mut backlink = vec![usize::MAX; env.len()];
    for (t, &score) in env.iter().enumerate() {
        let upper = max_lag.min(t);
        let best = (min_lag..=upper)
            .filter(|&lag| lag >= min_lag && t >= lag)
            .map(|lag| {
                #[expect(clippy::cast_precision_loss, reason = "lag is small")]
                let log_ratio = (lag as f64 / period_frames).ln();
                let txwt = -TIGHTNESS * log_ratio * log_ratio;
                (txwt + f64::from(cumscore[t - lag]), lag)
            })
            .max_by(|a, b| a.0.total_cmp(&b.0));
        match best {
            Some((cand, lag)) => {
                cumscore[t] = score + cand as f32;
                backlink[t] = t - lag;
            }
            None => {
                cumscore[t] = score;
            }
        }
    }
    (cumscore, backlink)
}

/// Backtracks the best path from the cumulative-score peak and
/// converts it to seconds, trimming marks that fall into silence.
fn backtrack(
    cumscore: &[f32],
    backlink: &[usize],
    env: &[f32],
    peak: f32,
    hop_seconds: f64,
) -> Vec<f32> {
    let Some(start) = cumscore
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, _)| i)
    else {
        return Vec::new();
    };

    let mut path = Vec::new();
    let mut t = start;
    while t != usize::MAX {
        path.push(t);
        t = backlink[t];
    }
    path.reverse();

    let weak = TRIM_FRACTION * peak;
    path.into_iter()
        .filter(|&frame| env[frame] >= weak)
        .map(|frame| (frame as f64 * hop_seconds) as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOP: f64 = 512.0 / 44_100.0;

    /// Impulse envelope with clicks every `period` seconds starting
    /// at `lead` seconds. `jitter_ms` perturbs each click, and every
    /// `drop_every`-th click is omitted (0 = never).
    fn click_env(beats: usize, bpm: f32, lead: f64, jitter_ms: f64, drop_every: usize) -> Vec<f32> {
        let period = 60.0 / f64::from(bpm);
        let total = lead + period * (beats as f64 + 2.0);
        let frames = (total / HOP).ceil() as usize + 8;
        let mut env = vec![0.0_f32; frames];
        let mut rng: f64 = 0.123_456_789;
        for b in 0..beats {
            if drop_every > 0 && b % drop_every == 0 && b > 0 {
                continue;
            }
            // deterministic pseudo-random jitter in [-jitter, +jitter]
            rng = (rng * 9.7 + 0.31).fract();
            let jit = (rng - 0.5) * 2.0 * jitter_ms / 1000.0;
            let pos = lead + period * b as f64 + jit;
            let frame = (pos / HOP).round() as usize;
            env[frame.min(frames - 1)] = 1.0;
        }
        env
    }

    #[test]
    fn tracks_clean_click_train() {
        // Given 48 clean clicks at 138 BPM.
        let env = click_env(48, 138.0, 0.0, 0.0, 0);

        // When tracking with an exact seed.
        let beats = track_beats_dp(&env, HOP, 138.0);

        // Then one mark per click, each within one hop of its click.
        assert_eq!(beats.len(), 48);
        let period = 60.0 / 138.0;
        for (i, &b) in beats.iter().enumerate() {
            let expected = period * i as f64;
            assert!(
                (f64::from(b) - expected).abs() <= HOP,
                "beat {i}: {b:.4}s vs {expected:.4}s"
            );
        }
    }

    #[test]
    fn survives_seed_error_jitter_and_drops() {
        // Given 96 clicks at 138 BPM with ±10 ms jitter, every 7th
        // dropped, and a seed off by 1 BPM.
        let env = click_env(96, 138.0, 0.0, 10.0, 7);

        // When tracking with the wrong seed.
        let beats = track_beats_dp(&env, HOP, 139.0);

        // Then the marks stay dense: ~82 expected (96 - 13 dropped),
        // every interval within 2× the true period (a dropped
        // click yields a 2-period gap, which the fit irons later).
        let period = 60.0 / 138.0;
        assert!(beats.len() >= 78, "only {} marks", beats.len());
        for w in beats.windows(2) {
            let iv = f64::from(w[1] - w[0]);
            assert!(
                (iv / period > 0.75) && (iv / period < 2.05),
                "interval {iv:.4}s at period {period:.4}s"
            );
        }
    }

    #[test]
    fn short_envelope_returns_no_marks() {
        // Given an envelope holding fewer than 8 periods.
        let env = click_env(4, 138.0, 0.0, 0.0, 0);

        // When tracking.
        let beats = track_beats_dp(&env, HOP, 138.0);

        // Then nothing is marked (caller falls back).
        assert!(beats.is_empty());
    }

    #[test]
    fn leading_silence_is_trimmed() {
        // Given clicks starting after 3 s of silence.
        let env = click_env(32, 138.0, 3.0, 0.0, 0);

        // When tracking.
        let beats = track_beats_dp(&env, HOP, 138.0);

        // Then the first mark sits at the first click, not inside the
        // silence.
        assert!(!beats.is_empty());
        assert!(
            f64::from(beats[0]) >= 3.0 - HOP,
            "first mark {beats:?} inside leading silence"
        );
        assert_eq!(beats.len(), 32);
    }

    #[test]
    fn silent_envelope_returns_no_marks() {
        // Given a flat zero envelope.
        let env = vec![0.0_f32; 20_000];

        // When tracking.
        let beats = track_beats_dp(&env, HOP, 138.0);

        // Then nothing is marked.
        assert!(beats.is_empty());
    }
}
