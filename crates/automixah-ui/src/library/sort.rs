//! Pure library-table ordering: the sort-state model behind the
//! entries-column headers plus the column comparators and placeholder
//! cell text.
//!
//! All decision logic lives here, gesture-free, so egui code only
//! classifies clicks and paints; every ordering rule (defaults,
//! direction flips, empty-value pinning, Camelot numerics) is testable
//! without a UI context.

use crate::library::filter;
use crate::library::store::LibraryEntry;
use djcore::key::{Key, KeyFormat};

/// A sortable table column. Order fixed by construction — drag-reorder
/// is intentionally not supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    /// Tag artist.
    Artist,
    /// Tag title (filename fallback included).
    Title,
    /// Joined analysis BPM.
    Bpm,
    /// Joined analysis key.
    Key,
    /// Container-probed duration.
    Duration,
    /// Parent directory of the indexed path.
    Folder,
}

impl SortColumn {
    /// Header label painted in the column's sort cell.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Artist => "Artist",
            Self::Title => "Title",
            Self::Bpm => "BPM",
            Self::Key => "Key",
            Self::Duration => "Duration",
            Self::Folder => "Folder",
        }
    }

    /// Every column in left-to-right header order.
    #[must_use]
    pub fn all() -> [Self; 6] {
        [
            Self::Artist,
            Self::Title,
            Self::Bpm,
            Self::Key,
            Self::Duration,
            Self::Folder,
        ]
    }
}

/// Library-table sort state owned by the app beside the filter buffer.
///
/// The default (`none`) means "no explicit sort" — rows render in the
/// pre-existing artist→title→path-with-fallback order. An explicit
/// column starts ascending and toggles on re-click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortState {
    /// Column whose header was clicked; `None` until any click.
    column: Option<SortColumn>,
    /// Direction of an active column sort (ignored while `column` is
    /// `None`).
    descending: bool,
}

impl Default for SortState {
    fn default() -> Self {
        Self::none()
    }
}

impl SortState {
    /// No explicit sort: the historical default ordering applies.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            column: None,
            descending: false,
        }
    }

    /// The actively sorted column, if any.
    #[must_use]
    pub const fn column(self) -> Option<SortColumn> {
        self.column
    }

    /// Direction indicator for an active column sort.
    #[must_use]
    pub const fn descending(self) -> bool {
        self.descending
    }

    /// Left-click on `column`'s header: activating an unsorted column
    /// sorts it ascending; clicking the active column toggles direction.
    #[must_use]
    pub fn on_click(self, column: SortColumn) -> Self {
        match self.column {
            Some(active) if active == column => Self {
                column: Some(column),
                descending: !self.descending,
            },
            _ => Self {
                column: Some(column),
                descending: false,
            },
        }
    }

    /// Right-click on any header: restore the default ordering.
    #[must_use]
    pub const fn on_right_click(self) -> Self {
        Self::none()
    }

    /// Indicator glyph for `column`'s header cell: `▲` ascending, `▼`
    /// descending, or `""` when that column is inactive.
    #[must_use]
    pub fn arrow(self, column: SortColumn) -> &'static str {
        if self.column == Some(column) && !self.descending {
            "▲"
        } else if self.column == Some(column) {
            "▼"
        } else {
            ""
        }
    }
}

/// Visible row indexes into `entries`: filter applied, then ordered by
/// `sort`.
#[must_use]
pub fn visible_entries(
    entries: &[LibraryEntry],
    terms: &[filter::FilterTerm],
    sort: &SortState,
) -> Vec<usize> {
    let mut visible: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| filter::matches(entry, terms))
        .map(|(index, _)| index)
        .collect();
    visible.sort_by(|&a, &b| compare_entries(&entries[a], &entries[b], sort));
    visible
}

/// Orders two entries under `sort`.
///
/// Empty values (never-analyzed columns, blank artist) are pinned after
/// every populated value in *both* directions — only the populated runs
/// flip with the sort arrow. Ties resolve through the default ordering
/// so rows render deterministically.
#[must_use]
pub fn compare_entries(a: &LibraryEntry, b: &LibraryEntry, sort: &SortState) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let Some(column) = sort.column else {
        return default_key(a).cmp(&default_key(b));
    };
    let emptiness = empty_rank(a, column).cmp(&empty_rank(b, column));
    if emptiness != Ordering::Equal {
        return emptiness;
    }
    let flipped = |ordering: Ordering| {
        if sort.descending {
            ordering.reverse()
        } else {
            ordering
        }
    };
    flipped(value_ordering(a, b, column)).then_with(|| default_key(a).cmp(&default_key(b)))
}

/// Missing-value rank: empty rows are `1` and always lose; folders
/// always exist, so [`SortColumn::Folder`] rows are all `0`.
fn empty_rank(entry: &LibraryEntry, column: SortColumn) -> u8 {
    match column {
        SortColumn::Artist => u8::from(entry.artist.trim().is_empty()),
        SortColumn::Title => u8::from(entry.title.trim().is_empty()),
        SortColumn::Bpm => u8::from(entry.bpm.is_none()),
        SortColumn::Key => u8::from(entry.key.is_none()),
        SortColumn::Duration => u8::from(entry.duration.is_none()),
        SortColumn::Folder => 0,
    }
}

/// Collated value comparison for one column. Both entries are
/// guaranteed populated (`empty_rank` equal, callers checked).
///
/// Beware for text columns: an empty string still compares — unreachable
/// for ever-present fields because `empty_rank` filtered them above.
fn value_ordering(a: &LibraryEntry, b: &LibraryEntry, column: SortColumn) -> std::cmp::Ordering {
    match column {
        SortColumn::Artist => a
            .artist
            .trim()
            .to_lowercase()
            .cmp(&b.artist.trim().to_lowercase()),
        SortColumn::Title => a
            .title
            .trim()
            .to_lowercase()
            .cmp(&b.title.trim().to_lowercase()),
        SortColumn::Bpm => f64_from(a.bpm).total_cmp(&f64_from(b.bpm)),
        SortColumn::Key => camelot_rank(a.key.as_ref()).cmp(&camelot_rank(b.key.as_ref())),
        SortColumn::Duration => f64_from(a.duration).total_cmp(&f64_from(b.duration)),
        SortColumn::Folder => folder_text(a).cmp(&folder_text(b)),
    }
}

/// Unwraps an optional float after the caller's emptiness check.
fn f64_from(value: Option<f64>) -> f64 {
    value.unwrap_or_default()
}

/// Folder display text for a path (root-level files show nothing).
fn folder_text(entry: &LibraryEntry) -> String {
    entry
        .rel_path
        .parent()
        .map_or_else(String::new, |p| p.display().to_string())
        .to_lowercase()
}

/// Historical default ordering: known artists first, then
/// artist→title→path; unknowns trail sorted by path alone.
fn default_key(entry: &LibraryEntry) -> (u8, String, String, String) {
    let artist = entry.artist.trim().to_lowercase();
    let title = entry.title.trim().to_lowercase();
    let path = entry.rel_path.to_string_lossy().to_lowercase();
    if artist.is_empty() {
        (1, String::new(), String::new(), path)
    } else {
        (0, artist, title, path)
    }
}

/// Camelot-wheel sort rank: wheel number (numeric, not lexicographic —
/// `8A` precedes `10A`) with minor ('A') before major ('B').
///
/// Derived by parsing this crate's Camelot *formatting* so the wheel
/// assignment stays djcore's single source of truth.
fn camelot_rank(key: Option<&Key>) -> (u8, u8) {
    let Some(key) = key else {
        return (u8::MAX, u8::MAX);
    };
    let text = key.format_with(KeyFormat::Camelot);
    let split = text
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(text.len());
    let number = text[..split].parse::<u8>().unwrap_or(u8::MAX);
    let mode_rank = match text[split..].chars().next() {
        Some('A') => 0,
        _ => 1,
    };
    (number, mode_rank)
}

/// Cell text for the BPM column: rounded whole number or muted `--`.
#[must_use]
pub fn bpm_text(bpm: Option<f64>) -> String {
    bpm.map_or_else(|| "--".to_owned(), |bpm| format!("{bpm:.0}"))
}

/// Cell text for the key column: Camelot notation or muted `--`.
#[must_use]
pub fn key_text(key: Option<&Key>) -> String {
    key.map_or_else(
        || "--".to_owned(),
        |key| key.format_with(KeyFormat::Camelot),
    )
}

/// Cell text for the duration column: `m:ss` or muted `---`.
#[must_use]
pub fn duration_text(duration: Option<f64>) -> String {
    duration.map_or_else(
        || "---".to_owned(),
        |d| {
            #[expect(clippy::cast_possible_truncation, reason = "display seconds")]
            #[expect(clippy::cast_sign_loss, reason = "probed durations are positive")]
            {
                format!("{:}:{:02}", (d / 60.0) as u32, (d % 60.0) as u32)
            }
        },
    )
}

#[cfg(test)]
pub(crate) mod tests_support {
    //! A minimal entry constructor shared by sibling modules' tests.

    use super::*;
    use automixah_engine::timeline::types::TrackHash;
    use std::path::PathBuf;

    /// A sample library entry with no analysis facts.
    #[must_use]
    pub fn sample_entry() -> LibraryEntry {
        LibraryEntry {
            root_id: 1,
            rel_path: PathBuf::from("sets/one.flac"),
            hash: TrackHash("h1".to_owned()),
            title: "One".to_owned(),
            artist: "Artist".to_owned(),
            duration: Some(61.0),
            bpm: None,
            key: None,
            mtime_secs: 0,
            size_bytes: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use automixah_engine::timeline::types::TrackHash;
    use djcore::key::KeyMode;
    use std::path::PathBuf;

    // Builder without analysis facts.
    fn entry(title: &str, artist: &str, rel: &str, hash: &str) -> LibraryEntry {
        LibraryEntry {
            root_id: 1,
            rel_path: PathBuf::from(rel),
            hash: TrackHash(hash.to_owned()),
            title: title.to_owned(),
            artist: artist.to_owned(),
            duration: Some(61.0),
            bpm: None,
            key: None,
            mtime_secs: 0,
            size_bytes: 0,
        }
    }

    fn analyzed(entry: LibraryEntry, bpm: Option<f64>, key: Option<Key>) -> LibraryEntry {
        LibraryEntry { bpm, key, ..entry }
    }

    fn titled_titles<'a>(entries: &'a [LibraryEntry], visible: &[usize]) -> Vec<&'a str> {
        visible.iter().map(|&i| entries[i].title.as_str()).collect()
    }

    fn minor_key(camelot_number: u8) -> Key {
        // djcore's private CAMELOT_MINOR: root index → wheel number; used
        // here only to name test keys ("8A" and friends).
        const CAMELOT_MINOR_ROOT_TO_NUMBER: [u8; 12] = [5, 12, 7, 2, 9, 4, 11, 6, 1, 8, 3, 10];
        let root = CAMELOT_MINOR_ROOT_TO_NUMBER
            .into_iter()
            .position(|n| n == camelot_number)
            .expect("wheel number exists");
        Key {
            root: root as u8,
            mode: KeyMode::Minor,
        }
    }

    // Given the default sort state.
    // When inspected.
    // Then no column is actively sorted.
    #[test]
    fn default_sort_state_has_no_column() {
        let state = SortState::default();

        assert_eq!(state.column(), None);
        assert_eq!(state, SortState::none());
    }

    // Given no active sort.
    // When a column header is clicked.
    // Then that column activates ascending.
    #[test]
    fn first_click_sorts_new_column_ascending() {
        let next = SortState::none().on_click(SortColumn::Bpm);

        assert_eq!(next.column(), Some(SortColumn::Bpm));
        assert!(!next.descending());
    }

    // Given an ascending column sort.
    // When the same header is clicked again.
    // Then the direction toggles to descending.
    #[test]
    fn second_click_toggles_active_column_descending() {
        let next = SortState::none()
            .on_click(SortColumn::Bpm)
            .on_click(SortColumn::Bpm);

        assert_eq!(next.column(), Some(SortColumn::Bpm));
        assert!(next.descending());
    }

    // Given a descending column sort.
    // When the same header is clicked once more.
    // Then the sort wraps back to ascending.
    #[test]
    fn third_click_returns_to_ascending() {
        let next = SortState::none()
            .on_click(SortColumn::Key)
            .on_click(SortColumn::Key)
            .on_click(SortColumn::Key);

        assert_eq!(next.column(), Some(SortColumn::Key));
        assert!(!next.descending());
    }

    // Given a descending sort on one column.
    // When a different header is clicked.
    // Then the new column activates ascending.
    #[test]
    fn clicking_other_column_starts_ascending_from_descending_state() {
        let bpm_descending = SortState::none()
            .on_click(SortColumn::Bpm)
            .on_click(SortColumn::Bpm);

        let next = bpm_descending.on_click(SortColumn::Artist);

        assert_eq!(next.column(), Some(SortColumn::Artist));
        assert!(!next.descending());
    }

    // Given an explicit sort.
    // When any header is right-clicked.
    // Then the state resets to the no-sort default.
    #[test]
    fn right_click_restores_default_state() {
        let sorted = SortState::none()
            .on_click(SortColumn::Duration)
            .on_click(SortColumn::Duration)
            .on_click(SortColumn::Duration);

        assert_eq!(sorted.on_right_click(), SortState::none());
    }

    // Given an active column.
    // When arrows are read.
    // Then only the active column shows a glyph and it matches direction.
    #[test]
    fn arrow_marks_only_active_column() {
        let ascending = SortState::none().on_click(SortColumn::Bpm);
        assert_eq!(ascending.arrow(SortColumn::Bpm), "▲");
        assert_eq!(ascending.arrow(SortColumn::Key), "");

        let descending = ascending.on_click(SortColumn::Bpm);
        assert_eq!(descending.arrow(SortColumn::Bpm), "▼");
        assert_eq!(descending.arrow(SortColumn::Key), "");
    }

    // Given entries in arbitrary order.
    // When the visible set is derived with no explicit sort.
    // Then the historical artist→title ordering applies, unknowns last.
    #[test]
    fn visible_entries_default_keeps_artist_then_title_unknowns_last() {
        let entries = vec![
            entry("Solo File", "", "z/untitled.flac", "h3"),
            entry("Beta", "A-List", "x/beta.flac", "h2"),
            entry("Alpha", "A-List", "x/alpha.flac", "h1"),
        ];

        let visible = visible_entries(&entries, &[], &SortState::none());

        assert_eq!(
            titled_titles(&entries, &visible),
            vec!["Alpha", "Beta", "Solo File"]
        );
    }

    // Given entries and a filter matching one.
    // When the visible set is derived.
    // Then only matching entries remain, still sorted.
    #[test]
    fn visible_entries_applies_filter_before_sort() {
        let entries = vec![
            entry("Alpha", "Artist", "a", "h1"),
            entry("Beta", "Artist", "b", "h2"),
            entry("Gamma", "Other", "c", "h3"),
        ];
        let terms = filter::parse("beta, artist");

        let visible = visible_entries(&entries, &terms, &SortState::default());

        assert_eq!(visible, vec![1], "only Beta matches both terms");
    }

    // Given an empty entry list.
    // When the visible set is derived.
    // Then it is empty.
    #[test]
    fn visible_entries_empty_library_is_empty() {
        assert!(visible_entries(&[], &[], &SortState::none()).is_empty());
    }

    // Given entries carrying different BPM values out of order.
    // When sorted by BPM ascending.
    // Then rows order numerically, fractions included.
    #[test]
    fn bpm_sort_orders_numerically_with_fractions() {
        let entries = vec![
            analyzed(entry("Cee", "A", "c.flac", "h3"), Some(174.5), None),
            analyzed(entry("Bee", "A", "b.flac", "h2"), Some(128.0), None),
            analyzed(entry("Ay", "A", "a.flac", "h1"), Some(99.0), None),
        ];
        let sort = SortState::none().on_click(SortColumn::Bpm);

        let visible = visible_entries(&entries, &[], &sort);

        assert_eq!(
            titled_titles(&entries, &visible),
            vec!["Ay", "Bee", "Cee"],
            "99 < 128 < 174.5"
        );
    }

    // Given entries whose keys are 10A, 2A and 8A.
    // When sorted by key.
    // Then the wheel numbers compare numerically, not lexicographically.
    #[test]
    fn key_sort_orders_camelot_numbers_numerically() {
        let entries = vec![
            analyzed(entry("Ten", "A", "t.flac", "h1"), None, Some(minor_key(10))),
            analyzed(entry("Two", "A", "w.flac", "h2"), None, Some(minor_key(2))),
            analyzed(
                entry("Eight", "A", "e.flac", "h3"),
                None,
                Some(minor_key(8)),
            ),
        ];
        let sort = SortState::none().on_click(SortColumn::Key);

        let visible = visible_entries(&entries, &[], &sort);

        assert_eq!(
            titled_titles(&entries, &visible),
            vec!["Two", "Eight", "Ten"],
            "2A < 8A < 10A — a string sort would put '10' first"
        );
    }

    // Given entries sharing one wheel number across modes.
    // When sorted by key ascending.
    // Then the A mode precedes the B mode deterministically.
    #[test]
    fn key_sort_breaks_same_wheel_number_by_mode() {
        let major_root_for_8 = 0_u8; // C major → 8B
        let entries = vec![
            analyzed(
                entry("Major Side", "A", "maj.flac", "h1"),
                None,
                Some(Key {
                    root: major_root_for_8,
                    mode: KeyMode::Major,
                }),
            ),
            analyzed(
                entry("Minor Side", "A", "min.flac", "h2"),
                None,
                Some(minor_key(8)),
            ),
        ];
        let sort = SortState::none().on_click(SortColumn::Key);

        let visible = visible_entries(&entries, &[], &sort);

        assert_eq!(
            titled_titles(&entries, &visible),
            vec!["Minor Side", "Major Side"]
        );
    }

    // Given populated and unanalyzed rows sorted ascending.
    // When derived.
    // Then unanalyzed rows trail every populated row.
    #[test]
    fn bpm_sort_pins_missing_last_ascending() {
        let entries = vec![
            analyzed(entry("None", "A", "n.flac", "h1"), None, None),
            analyzed(entry("High", "A", "h.flac", "h2"), Some(180.0), None),
            analyzed(entry("Low", "A", "l.flac", "h3"), Some(90.0), None),
        ];
        let sort = SortState::none().on_click(SortColumn::Bpm);

        let visible = visible_entries(&entries, &[], &sort);

        assert_eq!(
            titled_titles(&entries, &visible),
            vec!["Low", "High", "None"]
        );
    }

    // Given populated and unanalyzed rows sorted descending.
    // When derived.
    // Then unanalyzed rows still trail — only populated values flip.
    #[test]
    fn bpm_sort_pins_missing_last_descending() {
        let entries = vec![
            analyzed(entry("None", "A", "n.flac", "h1"), None, None),
            analyzed(entry("High", "A", "h.flac", "h2"), Some(180.0), None),
            analyzed(entry("Low", "A", "l.flac", "h3"), Some(90.0), None),
        ];
        let sort = SortState::none()
            .on_click(SortColumn::Bpm)
            .on_click(SortColumn::Bpm);

        let visible = visible_entries(&entries, &[], &sort);

        assert_eq!(
            titled_titles(&entries, &visible),
            vec!["High", "Low", "None"]
        );
    }

    // Given an explicitly sorted list of fully populated rows.
    // When compared against the default ordering.
    // Then ties resolve through the default artist→title rule.
    #[test]
    fn explicit_sort_resolves_ties_through_default_key() {
        let entries = vec![
            analyzed(entry("Second", "Same", "y.flac", "h1"), Some(120.0), None),
            analyzed(entry("First", "Same", "x.flac", "h2"), Some(120.0), None),
        ];
        let sort = SortState::none().on_click(SortColumn::Bpm);

        let visible = visible_entries(&entries, &[], &sort);

        assert_eq!(titled_titles(&entries, &visible), vec!["First", "Second"]);
    }

    // Given rows sorted by duration.
    // When values differ only slightly.
    // Then float comparison stays total (no NaN panics, stable order).
    #[test]
    fn duration_sort_compares_totally() {
        let longer = analyzed(entry("Long", "A", "l.flac", "h1"), None, None);
        let longer = LibraryEntry {
            duration: Some(61.5),
            ..longer
        };
        let shorter = LibraryEntry {
            duration: Some(61.0),
            ..longer.clone()
        };
        let sort = SortState::none().on_click(SortColumn::Duration);

        assert_eq!(
            compare_entries(&shorter, &longer, &sort),
            std::cmp::Ordering::Less
        );
    }

    // Given a folder sort.
    // When paths share a directory.
    // Then folders compare case-insensitively by display path.
    #[test]
    fn folder_sort_groups_by_directory_case_insensitively() {
        let entries = vec![
            entry("One", "A", "ZED/one.flac", "h1"),
            entry("Two", "A", "abc/two.flac", "h2"),
            entry("Three", "A", "ABC/three.flac", "h3"),
        ];
        let sort = SortState::none().on_click(SortColumn::Folder);

        let visible = visible_entries(&entries, &[], &sort);

        assert_eq!(
            titled_titles(&entries, &visible),
            vec!["Three", "Two", "One"],
            "abc < ZED case-insensitively; ties keep default (artist, then path) order"
        );
    }

    // Given a BPM value.
    // When rendered as cell text.
    // Then it rounds to the nearest whole number; absence renders a
    // placeholder.
    #[rstest::rstest]
    #[case(Some(174.4), "174")]
    #[case(Some(174.6), "175")]
    #[case(None, "--")]
    fn bpm_text_rounds_and_places_holds(#[case] bpm: Option<f64>, #[case] expected: &str) {
        assert_eq!(bpm_text(bpm), expected);
    }

    // Given a key.
    // When rendered as cell text.
    // Then Camelot notation is used; absence renders `--`.
    #[test]
    fn key_text_formats_camelot_or_placeholder() {
        assert_eq!(key_text(Some(&minor_key(5))), "5A");
        assert_eq!(key_text(None), "--");
    }

    // Given a duration.
    // When rendered as cell text.
    // Then clock notation is used; absence renders `---`.
    #[rstest::rstest]
    #[case(Some(61.0), "1:01")]
    #[case(Some(0.0), "0:00")]
    #[case(None, "---")]
    fn duration_text_formats_clock_or_placeholder(
        #[case] duration: Option<f64>,
        #[case] expected: &str,
    ) {
        assert_eq!(duration_text(duration), expected);
    }
}
