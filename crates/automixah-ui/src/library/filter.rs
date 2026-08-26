//! Library search filter: comma-separated fuzzy terms ANDed over
//! title/artist/path.
//!
//! The parser emits typed [`FilterTerm`] nodes so field filters (BPM,
//! key) can extend the grammar without redesign; today only text terms
//! exist. Matching is SkimMatcherV2 (case-ignored) per term over the
//! entry's fields — a term matches when *any* field matches, entries
//! match when *every* term matches. Highlight spans mirror that
//! semantics per field: [`spans`] collects, for each field, the union of
//! the indices of every term that matched it — a term matched via the
//! title highlights the title, one matched via the path highlights the
//! path, and terms splitting across fields highlight both. Indices
//! always address the same string that is painted (never
//! lowercase-and-index a different string).

use fuzzy_matcher::FuzzyMatcher as _;
use fuzzy_matcher::skim::SkimMatcherV2;

use crate::library::store::LibraryEntry;

/// One parsed search term.
///
/// The enum carries the extension point for field filters (`bpm:>128`,
/// `key:8a`): such inputs currently degrade to plain text (the parser
/// recognizes the shape, v1 does not implement the semantics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterTerm {
    /// Fuzzy text matched against title, artist, and path.
    Text(String),
    /// A `field:value` shape reserved for the follow-up filter task;
    /// matched as plain text for now.
    Field { field: String, value: String },
}

impl FilterTerm {
    /// The pattern string fed to the matcher (either variant).
    fn pattern(&self) -> &str {
        match self {
            Self::Text(text) => text,
            Self::Field { value, .. } => value,
        }
    }
}

/// Parses a raw query: comma-separated terms, trimmed, empties dropped.
#[must_use]
pub fn parse(query: &str) -> Vec<FilterTerm> {
    query
        .split(',')
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(|term| match term.split_once(':') {
            // Reserved shape: a known-prefix `field:value` term. Unknown
            // prefixes stay Text so plain titles with colons still match.
            Some((field, value)) if is_reserved_field(field) => FilterTerm::Field {
                field: field.to_owned(),
                value: value.to_owned(),
            },
            _ => FilterTerm::Text(term.to_owned()),
        })
        .collect()
}

/// Field names reserved for the follow-up typed-filter task.
const RESERVED_FIELDS: &[&str] = &["bpm", "key"];

fn is_reserved_field(field: &str) -> bool {
    RESERVED_FIELDS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(field))
}

/// Whether `entry` matches every term (AND); each term may match a
/// different field. An empty term list matches everything.
#[must_use]
pub fn matches(entry: &LibraryEntry, terms: &[FilterTerm]) -> bool {
    if terms.is_empty() {
        return true;
    }
    let matcher = SkimMatcherV2::default().ignore_case();
    terms.iter().all(|term| {
        let pattern = term.pattern();
        matcher.fuzzy_match(&entry.title, pattern).is_some()
            || matcher.fuzzy_match(&entry.artist, pattern).is_some()
            || matcher
                .fuzzy_match(&entry.rel_path.to_string_lossy(), pattern)
                .is_some()
    })
}

/// Which entry field a highlight belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// The title field.
    Title,
    /// The artist field.
    Artist,
    /// The path field.
    Path,
}

/// Highlight spans for every field some term matched.
///
/// Each field's indices are the union (sorted, deduplicated) of the
/// matched char indices of every term that matched *that field* —
/// exactly the matches that made [`matches`] accept the entry. Fields no
/// term matched have no span. This mirrors the filter gate's semantics:
/// a term satisfied by the title highlights the title even when another
/// term was satisfied by the artist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldSpans {
    /// Title match indices, when any term matched the title.
    pub title: Vec<usize>,
    /// Artist match indices, when any term matched the artist.
    pub artist: Vec<usize>,
    /// Path match indices, when any term matched the path.
    pub path: Vec<usize>,
}

impl FieldSpans {
    /// `true` when no field has a span.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.artist.is_empty() && self.path.is_empty()
    }
}

/// The per-field highlight spans of `entry` against `terms`.
///
/// Returns empty spans when `terms` is empty (nothing to highlight).
#[must_use]
pub fn spans(entry: &LibraryEntry, terms: &[FilterTerm]) -> FieldSpans {
    let mut out = FieldSpans::default();
    if terms.is_empty() {
        return out;
    }
    let matcher = SkimMatcherV2::default().ignore_case();
    let path = entry.rel_path.to_string_lossy();
    for (field, target) in [
        (&mut out.title, entry.title.as_str()),
        (&mut out.artist, entry.artist.as_str()),
        (&mut out.path, path.as_ref()),
    ] {
        for term in terms {
            if let Some((_, indices)) = matcher.fuzzy_indices(target, term.pattern()) {
                field.extend(indices);
            }
        }
        field.sort_unstable();
        field.dedup();
    }
    out
}

/// The display string of one field (the string the highlight indices
/// address).
#[must_use]
pub fn field_text<'a>(entry: &'a LibraryEntry, kind: FieldKind) -> std::borrow::Cow<'a, str> {
    match kind {
        FieldKind::Title => entry.title.as_str().into(),
        FieldKind::Artist => entry.artist.as_str().into(),
        FieldKind::Path => entry.rel_path.to_string_lossy(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(title: &str, artist: &str, rel: &str) -> LibraryEntry {
        LibraryEntry {
            root_id: 1,
            rel_path: PathBuf::from(rel),
            hash: automixah_engine::timeline::types::TrackHash("h".to_owned()),
            title: title.to_owned(),
            artist: artist.to_owned(),
            duration: None,
            mtime_secs: 0,
            size_bytes: 0,
        }
    }

    // Given a query with padded comma-separated terms.
    // When parsed.
    // Then each trimmed non-empty term becomes a Text node.
    #[test]
    fn parse_splits_trims_and_drops_empties() {
        let terms = parse("  foo , , bar ,");

        assert_eq!(
            terms,
            vec![
                FilterTerm::Text("foo".to_owned()),
                FilterTerm::Text("bar".to_owned()),
            ]
        );
    }

    // Given an empty or comma-only query.
    // When parsed.
    // Then no terms exist (everything matches).
    #[test]
    fn parse_of_empty_query_yields_no_terms() {
        assert!(parse("").is_empty());
        assert!(parse(" , ,, ").is_empty());
    }

    // Given a query using a reserved field prefix.
    // When parsed.
    // Then it becomes a Field term (the reserved extension shape).
    #[test]
    fn parse_recognizes_reserved_field_shape() {
        let terms = parse("bpm:>128");

        assert_eq!(
            terms,
            vec![FilterTerm::Field {
                field: "bpm".to_owned(),
                value: ">128".to_owned(),
            }]
        );
    }

    // Given a title containing a colon with a non-reserved prefix.
    // When parsed.
    // Then it stays a plain Text term.
    #[test]
    fn parse_keeps_unknown_colon_terms_as_text() {
        let terms = parse("chapter:one");

        assert_eq!(terms, vec![FilterTerm::Text("chapter:one".to_owned())]);
    }

    // Given terms matching different fields of one entry.
    // When matched.
    // Then the entry matches (each term satisfied by any field).
    #[test]
    fn matches_ands_terms_across_fields() {
        let e = entry("Night Drive", "Pulse", "sets/one.flac");
        let terms = parse("night, pulse");

        assert!(matches(&e, &terms), "title term + artist term");
    }

    // Given one term no field matches.
    // When matched.
    // Then the entry is excluded.
    #[test]
    fn matches_excludes_when_any_term_misses() {
        let e = entry("Night Drive", "Pulse", "sets/one.flac");
        let terms = parse("night, zzz");

        assert!(!matches(&e, &terms));
    }

    // Given an empty term list.
    // When matched.
    // Then every entry matches without running the matcher.
    #[test]
    fn matches_empty_terms_matches_all() {
        let e = entry("Anything", "", "x");
        assert!(matches(&e, &[]));
    }

    // Given scattered-glyph matching.
    // When matched.
    // Then a subsequence matches (fuzzy, not substring).
    #[test]
    fn matches_scattered_glyphs() {
        let e = entry("Nightdrive", "", "");
        let terms = parse("nd");

        assert!(matches(&e, &terms), "'n…d' subsequence");
    }

    // Given case-differing query and fields.
    // When matched.
    // Then matching is case-insensitive.
    #[test]
    fn matches_ignore_case() {
        let e = entry("NIGHT DRIVE", "pulse", "X");
        assert!(matches(&e, &parse("night")));
        assert!(matches(&e, &parse("PULSE")));
    }

    // Given terms splitting across fields (the comma-AND case).
    // When spans are derived.
    // Then each matched field highlights with its own indices.
    #[test]
    fn spans_highlight_each_term_on_its_matched_field() {
        let e = entry("Night Drive", "Pulse", "sets/one.flac");
        let s = spans(&e, &parse("night, pulse"));

        assert!(!s.title.is_empty(), "title term highlights the title");
        assert!(!s.artist.is_empty(), "artist term highlights the artist");
        assert!(s.path.is_empty(), "no term matched the path");

        let title: Vec<char> = e.title.chars().collect();
        for index in &s.title {
            let ch = title[*index].to_ascii_lowercase();
            assert!("night".contains(ch), "index {index} points at '{ch}'");
        }
        let artist: Vec<char> = e.artist.chars().collect();
        for index in &s.artist {
            let ch = artist[*index].to_ascii_lowercase();
            assert!("pulse".contains(ch), "index {index} points at '{ch}'");
        }
    }

    // Given a term only the path satisfies.
    // When spans are derived.
    // Then only the path highlights.
    #[test]
    fn spans_fall_through_to_path() {
        let e = entry("Unrelated", "Nope", "deep/sets/one.flac");
        let s = spans(&e, &parse("sets"));

        assert!(s.title.is_empty());
        assert!(s.artist.is_empty());
        assert!(!s.path.is_empty(), "the path match highlights");
    }

    // Given an empty term list.
    // When spans are derived.
    // Then nothing highlights.
    #[test]
    fn spans_of_empty_terms_is_empty() {
        let e = entry("Any", "Thing", "x");
        assert!(spans(&e, &[]).is_empty());
    }

    // Given one term matching several fields.
    // When spans are derived.
    // Then every matched field highlights.
    #[test]
    fn spans_cover_all_fields_one_term_matches() {
        let e = entry("Night Drive", "Nightwish", "Night/one.flac");
        let s = spans(&e, &parse("night"));

        assert!(!s.title.is_empty());
        assert!(!s.artist.is_empty());
        assert!(!s.path.is_empty());
    }
}
