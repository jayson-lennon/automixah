//! Musical key representation and formatting.
//!
//! Provides types for representing musical keys in both standard notation
//! (e.g., `Am`, `C#`) and Camelot wheel notation (e.g., `8A`, `8B`),
//! as well as harmonic distance calculation for DJ mixing.
//!
//! Vendored from `harmonic-playlist` `feat/key` with behavior preserved
//! verbatim (parity is test-enforced).

/// Display format for musical keys.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum KeyFormat {
    /// Standard music notation (e.g., `Am`, `Dbm`, `C`).
    #[default]
    Standard,
    /// Camelot wheel notation (e.g., `8A`, `8B`).
    Camelot,
}

impl KeyFormat {
    /// Returns the next format in the cycle.
    #[must_use]
    pub fn cycle(&self) -> Self {
        match self {
            Self::Standard => Self::Camelot,
            Self::Camelot => Self::Standard,
        }
    }
}

const CAMELOT_MAJOR: [u8; 12] = [8, 3, 10, 5, 12, 7, 2, 9, 4, 11, 6, 1];
const CAMELOT_MINOR: [u8; 12] = [5, 12, 7, 2, 9, 4, 11, 6, 1, 8, 3, 10];

/// Musical key of a track.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Key {
    /// Root note index: 0=C, 1=C#/Db, ..., 11=B.
    pub root: u8,
    /// Whether the key is major or minor.
    pub mode: KeyMode,
}

/// Mode of a musical key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum KeyMode {
    /// Major key.
    Major,
    /// Minor key.
    Minor,
}

impl std::fmt::Display for KeyMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Major => write!(f, ""),
            Self::Minor => write!(f, "m"),
        }
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.format_with(KeyFormat::Standard))
    }
}

impl Key {
    /// Formats the key using the specified notation.
    #[must_use]
    pub fn format_with(&self, format: KeyFormat) -> String {
        match format {
            KeyFormat::Standard => {
                const SHARP_ROOTS: [&str; 12] = [
                    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
                ];
                const FLAT_ROOTS: [&str; 12] = [
                    "C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B",
                ];
                let root = self.root as usize % 12;
                let root_name = match self.mode {
                    KeyMode::Major => SHARP_ROOTS[root],
                    KeyMode::Minor => FLAT_ROOTS[root],
                };
                format!("{root_name}{}", self.mode)
            }
            KeyFormat::Camelot => {
                let root = self.root as usize % 12;
                let number = match self.mode {
                    KeyMode::Major => CAMELOT_MAJOR[root],
                    KeyMode::Minor => CAMELOT_MINOR[root],
                };
                let suffix = match self.mode {
                    KeyMode::Major => "B",
                    KeyMode::Minor => "A",
                };
                format!("{number}{suffix}")
            }
        }
    }

    /// Returns the normalized harmonic distance between this key and another,
    /// based on the Camelot wheel.
    ///
    /// Distance ranges from `0.0` (identical key) to `1.0` (opposite side of
    /// the Camelot wheel). The calculation uses the Camelot wheel number
    /// assignment and accounts for mode compatibility:
    ///
    /// - Same Camelot number, same mode → `0.0`
    /// - Same Camelot number, different mode → `~0.077` (relative major/minor)
    /// - Different number, same mode → wheel distance / 6.5
    /// - Different number, different mode → (wheel distance + 0.5) / 6.5
    ///
    /// The maximum raw distance is `6.5` (opposite side of the wheel with
    /// different mode), which normalizes to `1.0`.
    pub fn harmonic_distance(&self, other: &Key) -> f32 {
        let self_number = i32::from(match self.mode {
            KeyMode::Major => CAMELOT_MAJOR[self.root as usize % 12],
            KeyMode::Minor => CAMELOT_MINOR[self.root as usize % 12],
        });

        let other_number = i32::from(match other.mode {
            KeyMode::Major => CAMELOT_MAJOR[other.root as usize % 12],
            KeyMode::Minor => CAMELOT_MINOR[other.root as usize % 12],
        });

        let same_mode = self.mode == other.mode;
        let wheel_distance = (self_number - other_number).unsigned_abs();
        #[allow(clippy::cast_precision_loss)]
        let wheel_distance = wheel_distance.min(12 - wheel_distance) as f32;

        let raw_distance = if self_number == other_number {
            if same_mode { 0.0 } else { 0.5 }
        } else if same_mode {
            wheel_distance
        } else {
            wheel_distance + 0.5
        };

        raw_distance / 6.5
    }

    /// Parses a key string in standard notation (e.g., `"Am"`, `"C"`, `"Bbm"`, `"F#"`) into a [`Key`].
    ///
    /// Recognizes both sharp (`C#`, `D#`, ...) and flat (`Db`, `Eb`, `Gb`, `Ab`, `Bb`) root
    /// names. The `'m'` suffix indicates minor mode; its absence indicates major.
    ///
    /// # Examples
    ///
    /// - `"Am"` → `Key { root: 9, mode: Minor }`
    /// - `"C"` → `Key { root: 0, mode: Major }`
    /// - `"Bbm"` → `Key { root: 10, mode: Minor }`
    /// - `"F#"` → `Key { root: 6, mode: Major }`
    #[must_use]
    pub fn parse(s: &str) -> Option<Key> {
        const SHARP_ROOTS: [&str; 12] = [
            "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
        ];
        const FLAT_ROOTS: [&str; 12] = [
            "C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B",
        ];

        let is_minor = s.to_ascii_lowercase().ends_with('m');
        let mode = if is_minor {
            KeyMode::Minor
        } else {
            KeyMode::Major
        };
        let root_str = if is_minor {
            &s[..s.len().saturating_sub(1)]
        } else {
            s
        };

        let root = SHARP_ROOTS
            .iter()
            .position(|&r| r.eq_ignore_ascii_case(root_str))
            .or_else(|| {
                FLAT_ROOTS
                    .iter()
                    .position(|&r| r.eq_ignore_ascii_case(root_str))
            })?;

        Some(Key {
            root: u8::try_from(root).ok()?,
            mode,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Key, KeyFormat, KeyMode};

    #[test]
    fn standard_format_uses_sharps_for_major_keys() {
        // Given C# major.
        let key = Key {
            root: 1,
            mode: KeyMode::Major,
        };

        // When formatting in standard notation.
        let formatted = key.format_with(KeyFormat::Standard);

        // Then the sharp name is used.
        assert_eq!(formatted, "C#");
    }

    #[test]
    fn standard_format_uses_flats_for_minor_keys() {
        // Given D# minor (root 3).
        let key = Key {
            root: 3,
            mode: KeyMode::Minor,
        };

        // When formatting in standard notation.
        let formatted = key.format_with(KeyFormat::Standard);

        // Then the flat name is used.
        assert_eq!(formatted, "Ebm");
    }

    #[test]
    fn camelot_format_c_major() {
        // Given C major.
        let key = Key {
            root: 0,
            mode: KeyMode::Major,
        };

        // When formatting in Camelot notation.
        let formatted = key.format_with(KeyFormat::Camelot);

        // Then the Camelot code is 8B.
        assert_eq!(formatted, "8B");
    }

    #[test]
    fn camelot_format_a_minor() {
        // Given A minor.
        let key = Key {
            root: 9,
            mode: KeyMode::Minor,
        };

        // When formatting in Camelot notation.
        let formatted = key.format_with(KeyFormat::Camelot);

        // Then the Camelot code is 8A.
        assert_eq!(formatted, "8A");
    }

    #[rstest::rstest]
    #[case(0, KeyMode::Major, "C")]
    #[case(1, KeyMode::Major, "C#")]
    #[case(2, KeyMode::Major, "D")]
    #[case(3, KeyMode::Major, "D#")]
    #[case(4, KeyMode::Major, "E")]
    #[case(5, KeyMode::Major, "F")]
    #[case(6, KeyMode::Major, "F#")]
    #[case(7, KeyMode::Major, "G")]
    #[case(8, KeyMode::Major, "G#")]
    #[case(9, KeyMode::Major, "A")]
    #[case(10, KeyMode::Major, "A#")]
    #[case(11, KeyMode::Major, "B")]
    fn standard_major_keys(#[case] root: u8, #[case] mode: KeyMode, #[case] expected: &str) {
        // Given a major key.
        let key = Key { root, mode };

        // When formatting in standard notation.
        let formatted = key.format_with(KeyFormat::Standard);

        // Then the expected sharp name is produced.
        assert_eq!(formatted, expected);
    }

    #[rstest::rstest]
    #[case(0, KeyMode::Minor, "Cm")]
    #[case(1, KeyMode::Minor, "Dbm")]
    #[case(2, KeyMode::Minor, "Dm")]
    #[case(3, KeyMode::Minor, "Ebm")]
    #[case(4, KeyMode::Minor, "Em")]
    #[case(5, KeyMode::Minor, "Fm")]
    #[case(6, KeyMode::Minor, "Gbm")]
    #[case(7, KeyMode::Minor, "Gm")]
    #[case(8, KeyMode::Minor, "Abm")]
    #[case(9, KeyMode::Minor, "Am")]
    #[case(10, KeyMode::Minor, "Bbm")]
    #[case(11, KeyMode::Minor, "Bm")]
    fn standard_minor_keys_use_flats(
        #[case] root: u8,
        #[case] mode: KeyMode,
        #[case] expected: &str,
    ) {
        // Given a minor key.
        let key = Key { root, mode };

        // When formatting in standard notation.
        let formatted = key.format_with(KeyFormat::Standard);

        // Then the expected flat name is produced.
        assert_eq!(formatted, expected);
    }

    #[rstest::rstest]
    #[case(0, KeyMode::Major, "8B")]
    #[case(1, KeyMode::Major, "3B")]
    #[case(2, KeyMode::Major, "10B")]
    #[case(3, KeyMode::Major, "5B")]
    #[case(4, KeyMode::Major, "12B")]
    #[case(5, KeyMode::Major, "7B")]
    #[case(6, KeyMode::Major, "2B")]
    #[case(7, KeyMode::Major, "9B")]
    #[case(8, KeyMode::Major, "4B")]
    #[case(9, KeyMode::Major, "11B")]
    #[case(10, KeyMode::Major, "6B")]
    #[case(11, KeyMode::Major, "1B")]
    fn camelot_major_keys(#[case] root: u8, #[case] mode: KeyMode, #[case] expected: &str) {
        // Given a major key.
        let key = Key { root, mode };

        // When formatting in Camelot notation.
        let formatted = key.format_with(KeyFormat::Camelot);

        // Then the expected Camelot code is produced.
        assert_eq!(formatted, expected);
    }

    #[rstest::rstest]
    #[case(0, KeyMode::Minor, "5A")]
    #[case(1, KeyMode::Minor, "12A")]
    #[case(2, KeyMode::Minor, "7A")]
    #[case(3, KeyMode::Minor, "2A")]
    #[case(4, KeyMode::Minor, "9A")]
    #[case(5, KeyMode::Minor, "4A")]
    #[case(6, KeyMode::Minor, "11A")]
    #[case(7, KeyMode::Minor, "6A")]
    #[case(8, KeyMode::Minor, "1A")]
    #[case(9, KeyMode::Minor, "8A")]
    #[case(10, KeyMode::Minor, "3A")]
    #[case(11, KeyMode::Minor, "10A")]
    fn camelot_minor_keys(#[case] root: u8, #[case] mode: KeyMode, #[case] expected: &str) {
        // Given a minor key.
        let key = Key { root, mode };

        // When formatting in Camelot notation.
        let formatted = key.format_with(KeyFormat::Camelot);

        // Then the expected Camelot code is produced.
        assert_eq!(formatted, expected);
    }

    #[test]
    fn cycle_toggles_between_formats() {
        // Given the default (Standard) format.
        let format = KeyFormat::Standard;

        // When cycling once.
        let next = format.cycle();

        // Then we get Camelot.
        assert_eq!(next, KeyFormat::Camelot);

        // When cycling again.
        let back = next.cycle();

        // Then we return to Standard.
        assert_eq!(back, KeyFormat::Standard);
    }

    #[test]
    fn display_delegates_to_standard_format() {
        // Given D# minor (root 3).
        let key = Key {
            root: 3,
            mode: KeyMode::Minor,
        };

        // When using Display.
        let displayed = key.to_string();

        // Then it matches the standard format with flats.
        assert_eq!(displayed, key.format_with(KeyFormat::Standard));
        assert_eq!(displayed, "Ebm");
    }

    fn key(root: u8, mode: KeyMode) -> Key {
        Key { root, mode }
    }

    #[test]
    fn identical_keys_have_zero_distance() {
        // Given two identical keys.
        let a = key(0, KeyMode::Major); // C = 8B
        let b = key(0, KeyMode::Major);

        // When calculating harmonic distance.
        let distance = a.harmonic_distance(&b);

        // Then the distance is zero.
        assert!((distance - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn relative_major_minor_has_small_distance() {
        // Given a key and its relative major (same Camelot number, different mode).
        let a = key(0, KeyMode::Major); // C = 8B
        let b = key(9, KeyMode::Minor); // Am = 8A

        // When calculating harmonic distance.
        let distance = a.harmonic_distance(&b);

        // Then the distance is small (same Camelot number, different mode).
        let expected = 0.5 / 6.5;
        assert!((distance - expected).abs() < f32::EPSILON);
    }

    #[test]
    fn adjacent_same_mode_has_unit_wheel_distance() {
        // Given two keys that are adjacent on the Camelot wheel with the same mode.
        let a = key(0, KeyMode::Major); // C = 8B
        let b = key(7, KeyMode::Major); // G = 9B

        // When calculating harmonic distance.
        let distance = a.harmonic_distance(&b);

        // Then the distance reflects one wheel step.
        let expected = 1.0 / 6.5;
        assert!((distance - expected).abs() < f32::EPSILON);
    }

    #[test]
    fn adjacent_different_mode_includes_mode_penalty() {
        // Given two keys that are one step apart on the wheel with different modes.
        let a = key(0, KeyMode::Major); // C = 8B
        let b = key(2, KeyMode::Minor); // Dm = 7A

        // When calculating harmonic distance.
        let distance = a.harmonic_distance(&b);

        // Then the distance includes the mode penalty.
        let expected = 1.5 / 6.5;
        assert!((distance - expected).abs() < f32::EPSILON);
    }

    #[test]
    fn opposite_side_of_wheel_has_large_distance() {
        // Given two keys on opposite sides of the Camelot wheel with the same mode.
        let a = key(0, KeyMode::Major); // C = 8B
        let b = key(6, KeyMode::Major); // F# = 2B

        // When calculating harmonic distance.
        let distance = a.harmonic_distance(&b);

        // Then the distance is close to maximum.
        let expected = 6.0 / 6.5;
        assert!((distance - expected).abs() < f32::EPSILON);
    }

    #[test]
    fn opposite_side_different_mode_is_exactly_one() {
        // Given two keys on opposite sides of the Camelot wheel with different modes.
        let a = key(0, KeyMode::Major); // C = 8B
        let b = key(3, KeyMode::Minor); // Ebm = 2A

        // When calculating harmonic distance.
        let distance = a.harmonic_distance(&b);

        // Then the distance is exactly 1.0.
        assert!((distance - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn distance_is_symmetric() {
        // Given two arbitrary keys.
        let a = key(3, KeyMode::Major); // D# = 5B
        let b = key(7, KeyMode::Minor); // Gm = 6A

        // When calculating distance in both directions.
        let d_ab = a.harmonic_distance(&b);
        let d_ba = b.harmonic_distance(&a);

        // Then the distances are equal.
        assert!((d_ab - d_ba).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_sharp_major_key() {
        // Given a sharp major key string.
        let input = "F#";

        // When parsing.
        let key = Key::parse(input);

        // Then the key is correct.
        assert_eq!(
            key,
            Some(Key {
                root: 6,
                mode: KeyMode::Major
            })
        );
    }

    #[test]
    fn parse_sharp_minor_key() {
        // Given a sharp minor key string.
        let input = "Cm";

        // When parsing.
        let key = Key::parse(input);

        // Then the key is correct.
        assert_eq!(
            key,
            Some(Key {
                root: 0,
                mode: KeyMode::Minor
            })
        );
    }

    #[test]
    fn parse_flat_minor_key() {
        // Given a flat minor key string.
        let input = "Bbm";

        // When parsing.
        let key = Key::parse(input);

        // Then the key is correct.
        assert_eq!(
            key,
            Some(Key {
                root: 10,
                mode: KeyMode::Minor
            })
        );
    }

    #[test]
    fn parse_all_flat_minor_keys() {
        // Given all flat-root minor key strings.
        let cases = [("Dbm", 1), ("Ebm", 3), ("Gbm", 6), ("Abm", 8), ("Bbm", 10)];

        for (input, expected_root) in cases {
            // When parsing.
            let key = Key::parse(input);

            // Then the root and mode are correct.
            assert_eq!(
                key,
                Some(Key {
                    root: expected_root,
                    mode: KeyMode::Minor
                }),
                "failed to parse {input}"
            );
        }
    }

    #[test]
    fn parse_natural_major_key() {
        // Given a natural major key string (no sharps or flats).
        let input = "G";

        // When parsing.
        let key = Key::parse(input);

        // Then the key is correct.
        assert_eq!(
            key,
            Some(Key {
                root: 7,
                mode: KeyMode::Major
            })
        );
    }

    #[test]
    fn parse_case_insensitive() {
        // Given a key string with mixed case.
        let input = "bbM";

        // When parsing.
        let key = Key::parse(input);

        // Then the key is correct.
        assert_eq!(
            key,
            Some(Key {
                root: 10,
                mode: KeyMode::Minor
            })
        );
    }

    #[test]
    fn parse_roundtrips_with_display() {
        // Given all 24 keys.
        for root in 0..12u8 {
            for mode in [KeyMode::Major, KeyMode::Minor] {
                let original = Key { root, mode };
                let displayed = original.to_string();

                // When parsing the displayed string back.
                let parsed = Key::parse(&displayed);

                // Then it round-trips correctly.
                assert_eq!(parsed, Some(original), "round-trip failed for {displayed}");
            }
        }
    }

    #[test]
    fn parse_invalid_input_returns_none() {
        // Given invalid key strings.
        let cases = ["", "Hm", "Cb", "X", "m", "C#m#"];

        for input in cases {
            // When parsing.
            let result = Key::parse(input);

            // Then the result is None.
            assert!(
                result.is_none(),
                "expected None for {input:?}, got {result:?}"
            );
        }
    }
}
