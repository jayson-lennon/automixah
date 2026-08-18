//! The editable grid model: the canonical subset a human edits plus the
//! projection back to full `BeatGrid` arrays.

/// Manual-editable subset of a beat grid.
///
/// `grid_bpm` + `anchor_seconds` + `downbeat_phase` fully determine the
/// grid; beats/downbeats/bars are pure projections.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EditableGrid {
    /// Constant-tempo BPM.
    pub grid_bpm: f32,
    /// Phase anchor: a beat time in `[0, bar)` seconds.
    pub anchor_seconds: f32,
    /// Beat-in-bar (0..=3) of the anchor.
    pub downbeat_phase: u8,
}

/// Beats per bar (4/4 assumed throughout).
pub const BEATS_PER_BAR: usize = 4;

impl EditableGrid {
    /// Beat period in seconds.
    #[must_use]
    pub fn beat_seconds(&self) -> f32 {
        60.0 / self.grid_bpm.max(0.01)
    }

    /// Bar period in seconds.
    #[must_use]
    pub fn bar_seconds(&self) -> f32 {
        self.beat_seconds() * BEATS_PER_BAR as f32
    }

    /// Normalizes the anchor into `[0, bar)` without changing phase.
    ///
    /// Keeps the stored invariant after BPM edits shift the bar length.
    #[must_use]
    pub fn normalized_anchor(&self) -> f32 {
        self.anchor_seconds.rem_euclid(self.bar_seconds())
    }

    /// Projects beats/downbeats/bars into `[0, end]`.
    #[must_use]
    pub fn project_to(&self, end: f32) -> djcore::analyzer::BeatGrid {
        let beat = self.beat_seconds();
        let mut beats = Vec::new();
        let mut downbeats = Vec::new();

        let first_k = ((-self.anchor_seconds) / beat).ceil() as i64;
        for k in first_k.. {
            let time = self.anchor_seconds + k as f32 * beat;
            if time > end {
                break;
            }
            beats.push(time);
            if k.rem_euclid(BEATS_PER_BAR as i64) == i64::from(self.downbeat_phase) {
                downbeats.push(time);
            }
        }

        djcore::analyzer::BeatGrid {
            grid_bpm: self.grid_bpm,
            anchor_seconds: self.anchor_seconds,
            bars: downbeats.clone(),
            beats,
            downbeats,
        }
    }
    /// Projects over a generous fixed span (pure-grid tests, no audio).
    #[must_use]
    pub fn project(&self) -> djcore::analyzer::BeatGrid {
        self.project_to(600.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(bpm: f32, anchor: f32, phase: u8) -> EditableGrid {
        EditableGrid {
            grid_bpm: bpm,
            anchor_seconds: anchor,
            downbeat_phase: phase,
        }
    }

    // Given a 140 BPM grid anchored at 0.
    // When projected over 10 seconds.
    // Then beats land every 3/7 s starting at 0.
    #[test]
    fn project_beats_from_anchor() {
        let projected = grid(140.0, 0.0, 0).project_to(10.0);
        let beat = 60.0 / 140.0;
        assert_eq!(projected.beats.first(), Some(&0.0));
        assert!((projected.beats[1] - beat).abs() < 1e-4);
        assert_eq!(projected.beats.len(), 24, "10 s at 140 BPM");
    }

    // Given an anchor mid-bar with phase 2.
    // When projected.
    // Then the first downbeat lands 2 beats after the anchor (beat 2's bar
    // starts 2 beats earlier, but that is before 0) and repeats every bar.
    #[test]
    fn project_marks_downbeats_by_phase() {
        let g = grid(120.0, 0.5, 2);
        let projected = g.project_to(10.0);
        let beat = 0.5;
        assert_eq!(projected.downbeats.first(), Some(&1.5));
        let second = projected.downbeats.get(1).copied();
        assert!((second.unwrap_or(0.0) - (1.5 + 4.0 * beat)).abs() < 1e-4);
    }

    // Given an anchor at or after one bar.
    // When normalized.
    // Then the invariant 0 ≤ anchor < bar holds via call-site normalization.
    #[test]
    fn anchor_wraps_within_bar() {
        let g = grid(120.0, 2.5, 0); // bar = 2.0 s at 120 BPM
        let normalized = g.normalized_anchor();
        assert!((normalized - 0.5).abs() < 1e-4, "wraps into [0, bar)");
    }
}
