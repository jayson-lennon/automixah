//! Data-driven transition-preset selection.
//!
//! Per-transition preset choice (which of the four built-ins drives
//! the A→B automation) is a *data table*, not code: rules evaluated
//! top-down over observable features (harmonic distance, BPM gap,
//! grid stability), first match wins. The table is
//! serde-serializable (RON in the app bundle), so behavior changes
//! without recompiling.

use crate::automation::presets::{PresetSpec, preset_specs};
use crate::timeline::types::TrackAnalysis;
use serde::{Deserialize, Serialize};

/// Observable features of an adjacent track pair, for rule matching.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransitionFeatures {
    /// Harmonic distance between the pair's keys, `[0, 1]`
    /// (0 = same key, 1 = maximally clashing).
    pub harmonic_distance: f32,
    /// Folded-BPM gap in BPM (absolute).
    pub bpm_gap: f32,
    /// Minimum of the two tracks' grid stability, `[0, 1]`.
    pub min_grid_stability: f32,
}

impl TransitionFeatures {
    /// Extracts the features of adjacent tracks `a` → `b`.
    #[must_use]
    pub fn from_pair(a: &TrackAnalysis, b: &TrackAnalysis) -> Self {
        Self {
            harmonic_distance: a.key.harmonic_distance(&b.key),
            bpm_gap: (a.bpm - b.bpm).abs(),
            min_grid_stability: a.grid_stability.min(b.grid_stability),
        }
    }
}

/// One rule: match features, name a preset.
///
/// Each threshold is an upper bound on the feature ("at most");
/// `None` matches anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectionRule {
    /// Preset name to select when matched.
    pub preset: String,
    /// Match only if harmonic distance is at most this.
    pub max_harmonic_distance: Option<f32>,
    /// Match only if BPM gap is at most this.
    pub max_bpm_gap: Option<f32>,
    /// Match only if min grid stability is at least this.
    pub min_grid_stability: Option<f32>,
}

impl SelectionRule {
    /// Whether a feature set satisfies this rule's bounds.
    #[must_use]
    pub fn matches(&self, f: &TransitionFeatures) -> bool {
        self.max_harmonic_distance
            .is_none_or(|max| f.harmonic_distance <= max)
            && self.max_bpm_gap.is_none_or(|max| f.bpm_gap <= max)
            && self
                .min_grid_stability
                .is_none_or(|min| f.min_grid_stability >= min)
    }
}

/// The default rule table (first match wins, terminating in a
/// catch-all).
///
/// Intent, encoded as data:
///
/// - Harmonically close + confident grids + tight tempo → the blends
///   (BassSwap is the flagship for compatible keys).
/// - Clashing keys or shaky grids → plain Crossfade (safest).
/// - Large tempo gaps → Cut (a hard switch masks the mismatch).
#[must_use]
pub fn default_rules() -> Vec<SelectionRule> {
    vec![
        SelectionRule {
            preset: "BassSwap".into(),
            max_harmonic_distance: Some(0.2),
            max_bpm_gap: Some(6.0),
            min_grid_stability: Some(0.3),
        },
        SelectionRule {
            preset: "LowCutBlend".into(),
            max_harmonic_distance: Some(0.5),
            max_bpm_gap: Some(6.0),
            min_grid_stability: Some(0.3),
        },
        SelectionRule {
            preset: "Cut".into(),
            max_harmonic_distance: None,
            max_bpm_gap: None,
            min_grid_stability: Some(0.3),
        },
        // Catch-all: never-failing crossfade.
        SelectionRule {
            preset: "Crossfade".into(),
            max_harmonic_distance: None,
            max_bpm_gap: None,
            min_grid_stability: None,
        },
    ]
}

/// Selects the preset for a transition from a rule table.
///
/// First matching rule wins; if the table is exhausted (shouldn't
/// happen with a catch-all), falls back to Crossfade.
#[must_use]
pub fn select_preset(rules: &[SelectionRule], features: &TransitionFeatures) -> String {
    rules
        .iter()
        .find(|r| r.matches(features))
        .map_or_else(|| "Crossfade".into(), |r| r.preset.clone())
}

/// Resolves a preset name to its spec (built-ins only).
#[must_use]
pub fn spec_by_name(name: &str) -> Option<PresetSpec> {
    preset_specs().into_iter().find(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(h: f32, gap: f32, stability: f32) -> TransitionFeatures {
        TransitionFeatures {
            harmonic_distance: h,
            bpm_gap: gap,
            min_grid_stability: stability,
        }
    }

    #[test]
    fn rule_matches_respects_each_bound() {
        // Given a rule bounded on all three features.
        let rule = SelectionRule {
            preset: "X".into(),
            max_harmonic_distance: Some(0.2),
            max_bpm_gap: Some(6.0),
            min_grid_stability: Some(0.3),
        };

        // Then each bound independently rejects violations.
        assert!(rule.matches(&features(0.1, 5.0, 0.5)));
        assert!(!rule.matches(&features(0.5, 5.0, 0.5)));
        assert!(!rule.matches(&features(0.1, 9.0, 0.5)));
        assert!(!rule.matches(&features(0.1, 5.0, 0.1)));
    }

    #[test]
    fn close_keys_confident_grids_tight_tempo_pick_bass_swap() {
        // Given the default table and a friendly pair.
        let f = features(0.1, 3.0, 0.9);

        // When selecting.
        let preset = select_preset(&default_rules(), &f);

        // Then BassSwap is chosen.
        assert_eq!(preset, "BassSwap");
    }

    #[test]
    fn moderate_keys_pick_low_cut_blend() {
        // Given a moderately compatible pair.
        let f = features(0.35, 3.0, 0.9);

        // When selecting.
        let preset = select_preset(&default_rules(), &f);

        // Then LowCutBlend is chosen.
        assert_eq!(preset, "LowCutBlend");
    }

    #[test]
    fn clashing_keys_pick_cut() {
        // Given a harmonically distant pair with good grids.
        let f = features(0.9, 2.0, 0.9);

        // When selecting.
        let preset = select_preset(&default_rules(), &f);

        // Then Cut is chosen.
        assert_eq!(preset, "Cut");
    }

    #[test]
    fn unconfident_grids_fall_through_to_crossfade() {
        // Given a shaky-grid pair (fails every stability bound).
        let f = features(0.1, 3.0, 0.1);

        // When selecting.
        let preset = select_preset(&default_rules(), &f);

        // Then the catch-all Crossfade is chosen.
        assert_eq!(preset, "Crossfade");
    }

    #[test]
    fn empty_table_falls_back_to_crossfade() {
        // Given no rules.
        // When selecting.
        let preset = select_preset(&[], &features(0.5, 10.0, 0.5));

        // Then Crossfade is the fallback.
        assert_eq!(preset, "Crossfade");
    }

    #[test]
    fn default_table_roundtrips_through_ron() {
        // Given the default rules.
        let rules = default_rules();

        // When serializing to RON and back.
        let text = ron::to_string(&rules).expect("ron ser");
        let back: Vec<SelectionRule> = ron::from_str(&text).expect("ron de");

        // Then the table survives.
        assert_eq!(back, rules);
    }

    #[test]
    fn every_selected_preset_resolves_to_a_spec() {
        // Given a sweep over the feature space.
        for h in [0.0, 0.2, 0.5, 0.9] {
            for gap in [0.0, 6.0, 12.0] {
                for stability in [0.0, 0.3, 0.9] {
                    // When selecting and resolving.
                    let name = select_preset(&default_rules(), &features(h, gap, stability));

                    // Then a built-in spec exists.
                    assert!(
                        spec_by_name(&name).is_some(),
                        "no spec for {name} (h={h}, gap={gap}, stab={stability})"
                    );
                }
            }
        }
    }
}
