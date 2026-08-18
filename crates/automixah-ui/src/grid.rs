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

impl EditableGrid {
    /// Extracts the editable subset from a decoded `BeatGrid`.
    #[must_use]
    pub fn from_grid(grid: &djcore::analyzer::BeatGrid) -> Self {
        Self {
            grid_bpm: grid.grid_bpm,
            anchor_seconds: grid.anchor_seconds.rem_euclid(bar_len(grid.grid_bpm)),
            downbeat_phase: phase_of(grid),
        }
    }
}

fn bar_len(bpm: f32) -> f32 {
    BEATS_PER_BAR as f32 * 60.0 / bpm.max(0.01)
}

/// Beat-in-bar of the first downbeat at or after the anchor, wrapped to
/// `[0, BEATS_PER_BAR)`.
fn phase_of(grid: &djcore::analyzer::BeatGrid) -> u8 {
    let Some(&first) = grid.downbeats.first() else {
        return 0;
    };
    let beat = 60.0 / grid.grid_bpm.max(0.01);
    let delta = (first - grid.anchor_seconds) / beat;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "phase is 0..4 after rem_euclid"
    )]
    let phase = delta.round().rem_euclid(BEATS_PER_BAR as f32) as i64;
    u8::try_from(phase).unwrap_or(0)
}

// ── cursor-driven actions ──────────────────────────────────────────────────

impl EditableGrid {
    /// Moves the anchor so the nearest beat lands exactly at `cursor_s`.
    ///
    /// Equivalent to nudging the whole grid by the residual; BPM is
    /// unchanged, so every other beat shifts by the same amount.
    pub fn snap_nearest_beat(&mut self, cursor_s: f32) {
        let beat = self.beat_seconds();
        let nearest =
            self.anchor_seconds + ((cursor_s - self.anchor_seconds) / beat).round() * beat;
        let residual = cursor_s - nearest;
        self.anchor_seconds = (self.anchor_seconds + residual).max(0.0);
        self.normalize();
    }

    /// Sets the downbeat phase so the beat at `cursor_s` is beat 1 of its
    /// bar. Anchor position is untouched — only the bar coloring moves.
    pub fn set_downbeat_at(&mut self, cursor_s: f32) {
        let beat = self.beat_seconds();
        let k = ((cursor_s - self.anchor_seconds) / beat).round();
        #[expect(clippy::cast_possible_truncation, reason = "k is small")]
        let phase = k.rem_euclid(BEATS_PER_BAR as f32) as i64;
        self.downbeat_phase = u8::try_from(phase).unwrap_or(0);
    }

    /// Wraps the anchor into `[0, bar)` in place.
    pub fn normalize(&mut self) {
        self.anchor_seconds = self.anchor_seconds.rem_euclid(self.bar_seconds());
    }

    /// Shifts every beat line by `delta_seconds` (positive = later),
    /// keeping the anchor inside `[0, bar)`.
    pub fn shift_by(&mut self, delta_seconds: f32) {
        self.anchor_seconds += delta_seconds;
        self.normalize();
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

    // Given a grid whose nearest beat to t=1.62 sits 0.02 s away.
    // When snapping the nearest beat to the cursor.
    // Then the anchor shifts by that residual (wrapped) and BPM is unchanged.
    #[test]
    fn snap_nearest_beat_shifts_anchor_by_residual() {
        let mut g = grid(120.0, 0.5, 0); // beat = 0.5 s, beats at 0.5, 1.0, 1.5, 2.0…
        let cursor = 1.62;
        g.snap_nearest_beat(cursor);

        // nearest beat to 1.62 was 1.5 → residual +0.12
        let expected_anchor = f32::rem_euclid(0.5 + 0.12, 2.0);
        assert!(
            (g.anchor_seconds - expected_anchor).abs() < 1e-4,
            "anchor {} vs {expected_anchor}",
            g.anchor_seconds
        );
        assert_eq!(g.grid_bpm, 120.0);
        // And a beat now lands exactly at the cursor.
        let projected = g.project();
        let nearest = projected
            .beats
            .iter()
            .copied()
            .min_by(|a, b| (a - cursor).abs().total_cmp(&(b - cursor).abs()))
            .expect("beats exist");
        assert!((nearest - cursor).abs() < 1e-3);
    }

    // Given a cursor on the 3rd beat after the anchor.
    // When setting the downbeat at the cursor.
    // Then the phase becomes 3 and the anchor does not move.
    #[test]
    fn set_downbeat_marks_cursor_bar_start() {
        let mut g = grid(128.0, 0.25, 0);
        let beat = 60.0 / 128.0;
        let cursor = 0.25 + 3.0 * beat;

        g.set_downbeat_at(cursor);

        assert_eq!(g.downbeat_phase, 3);
        assert!((g.anchor_seconds - 0.25).abs() < 1e-6);
        // And a downbeat now lands at the cursor.
        let projected = g.project();
        assert!(
            projected
                .downbeats
                .iter()
                .any(|&t| (t - cursor).abs() < 1e-3),
            "downbeats {:?}",
            &projected.downbeats[..4.min(projected.downbeats.len())]
        );
    }

    // Given a decoded auto grid at 138 BPM anchored at 0.4 with downbeats on
    // the anchor (phase 0).
    // When converted to the editable subset and back.
    // Then the round-trip preserves BPM, normalized anchor, and phase.
    #[test]
    fn from_grid_round_trips() {
        let beat = 60.0 / 138.0;
        let bar = 4.0 * beat;
        let mut downbeats = Vec::new();
        let mut k = 0;
        while 0.4 + k as f32 * bar < 600.0 {
            downbeats.push(0.4 + k as f32 * bar);
            k += 1;
        }
        let decoded = djcore::analyzer::BeatGrid {
            grid_bpm: 138.0,
            anchor_seconds: 0.4 + 3.0 * bar, // unnormalized on purpose
            downbeats: downbeats.clone(),
            beats: Vec::new(),
            bars: downbeats,
        };

        let editable = EditableGrid::from_grid(&decoded);

        assert!((editable.grid_bpm - 138.0).abs() < 1e-4);
        assert!(
            (editable.anchor_seconds - (0.4 + 3.0 * bar).rem_euclid(bar)).abs() < 1e-4,
            "anchor normalized into [0, bar)"
        );
        assert_eq!(editable.downbeat_phase, 0);

        let reprojected = editable.project();
        assert!(
            (reprojected.downbeats[1] - reprojected.downbeats[0] - bar).abs() < 1e-4,
            "downbeats repeat at one bar"
        );
    }

    // Given a grid whose downbeat sits 3 beats after the anchor.
    // When the phase is extracted.
    // Then it is 3, and re-projection lands downbeats on the same times.
    #[test]
    fn phase_extracts_from_downbeat_offset() {
        let beat = 60.0 / 128.0;
        let mut downbeats = Vec::new();
        let mut k = 0;
        while 3.0 * beat + k as f32 * 4.0 * beat < 600.0 {
            downbeats.push(3.0 * beat + k as f32 * 4.0 * beat);
            k += 1;
        }
        let decoded = djcore::analyzer::BeatGrid {
            grid_bpm: 128.0,
            anchor_seconds: 0.0,
            downbeats: downbeats.clone(),
            beats: Vec::new(),
            bars: downbeats,
        };

        let editable = EditableGrid::from_grid(&decoded);

        assert_eq!(editable.downbeat_phase, 3);
        let reprojected = editable.project();
        assert!(
            (reprojected.downbeats[0] - 3.0 * beat).abs() < 1e-3,
            "reprojected first downbeat {} vs {}",
            reprojected.downbeats[0],
            3.0 * beat
        );
    }

    // Given a grid at 120 BPM (bar = 2 s) with anchor at 0.5 s.
    // When shifted by +0.25 s.
    // Then the anchor advances by exactly the delta.
    #[test]
    fn shift_by_adds_delta() {
        let mut g = EditableGrid {
            grid_bpm: 120.0,
            anchor_seconds: 0.5,
            downbeat_phase: 0,
        };
        g.shift_by(0.25);
        assert!((g.anchor_seconds - 0.75).abs() < 1e-6);
    }

    // Given a grid with anchor near the bar end.
    // When shifted past the bar boundary.
    // Then the anchor wraps into [0, bar).
    #[test]
    fn shift_by_wraps_into_bar() {
        let mut g = EditableGrid {
            grid_bpm: 120.0,
            anchor_seconds: 1.9,
            downbeat_phase: 0,
        };
        g.shift_by(0.5);
        assert!(
            g.anchor_seconds >= 0.0 && g.anchor_seconds < 2.0,
            "wrapped: {}",
            g.anchor_seconds
        );
        assert!(
            (g.anchor_seconds - 0.4).abs() < 1e-6,
            "wrap preserves phase"
        );
    }

    // Given a grid with anchor near 0.
    // When shifted negatively.
    // Then the anchor wraps from the bar end (phase preserved).
    #[test]
    fn shift_by_negative_wraps_from_bar_end() {
        let mut g = EditableGrid {
            grid_bpm: 120.0,
            anchor_seconds: 0.1,
            downbeat_phase: 0,
        };
        g.shift_by(-0.5);
        assert!(
            (g.anchor_seconds - 1.6).abs() < 1e-6,
            "wrapped to {}",
            g.anchor_seconds
        );
    }
}
