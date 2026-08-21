//! Pure helpers for importing UTF-8 `.m3u` file lists.
//!
//! M3U syntax is parsed by the [`m3u`] crate. The importer accepts absolute
//! local paths and local `file://` URIs, while rejecting network URLs and
//! relative entries.

use std::io::BufReader;
use std::path::{Path, PathBuf};

/// Returns the absolute local filesystem paths listed by an M3U document.
///
/// The [`m3u`] reader skips blank lines and comments/metadata, and classifies
/// URI entries separately from filesystem paths. Local `file://` URIs are
/// converted to paths by the crate's URL implementation, including percent
/// decoding. Duplicate entries are retained here; content-hash deduplication
/// belongs to the import workflow.
#[must_use]
pub fn parse_entries(document: &str) -> Vec<PathBuf> {
    let mut reader = m3u::Reader::new(BufReader::new(document.as_bytes()));
    reader
        .entries()
        .filter_map(Result::ok)
        .filter_map(local_absolute_path)
        .collect()
}

fn local_absolute_path(entry: m3u::Entry) -> Option<PathBuf> {
    match entry {
        m3u::Entry::Path(path) if path.is_absolute() => Some(path),
        m3u::Entry::Path(path) => path
            .to_str()
            .filter(|value| value.starts_with("file://"))
            .and_then(|value| m3u::Url::parse(value).ok())
            .and_then(|url| url.to_file_path().ok())
            .filter(|path| path.is_absolute()),
        m3u::Entry::Url(url) if url.scheme() == "file" => {
            url.to_file_path().ok().filter(|path| path.is_absolute())
        }
        m3u::Entry::Url(_) => None,
    }
}

/// Returns whether `path` has the supported, case-sensitive `.m3u` extension.
#[must_use]
pub fn is_m3u_path(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("m3u")
}

/// Returns a non-empty playlist name derived from an `.m3u` filename.
///
/// The extension must be exactly `.m3u`; invalid or non-UTF-8 stems return
/// `None` rather than inventing a name.
#[must_use]
pub fn filename_stem(path: &Path) -> Option<String> {
    if !is_m3u_path(path) {
        return None;
    }
    let stem = path.file_stem()?.to_str()?.trim();
    (!stem.is_empty()).then(|| stem.to_owned())
}

/// Selects the lowest unused exact playlist name.
///
/// `base` is preferred, followed by `base(1)`, `base(2)`, and so on. Name
/// comparison is case-sensitive, matching the playlist store.
#[must_use]
pub fn lowest_unused_name<'a, I>(base: &str, existing: I) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    let existing: std::collections::HashSet<&str> = existing.into_iter().collect();
    if !existing.contains(base) {
        return base.to_owned();
    }
    for suffix in 1_u64.. {
        let candidate = format!("{base}({suffix})");
        if !existing.contains(candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!("u64 suffix range is finite but practically unexhaustible")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given an M3U document containing metadata, blanks, and path whitespace.
    // When its entries are parsed.
    // Then only absolute filesystem paths remain in source order.
    #[test]
    fn parse_entries_filters_lines_and_preserves_order() {
        let document = "\n#EXTM3U\n #EXTINF:1,Track\n /music/one.mp3 \nrelative.mp3\nhttps://example.test/two.mp3\n# comment\n/music/two.ogg\n";

        let entries = parse_entries(document);

        assert_eq!(
            entries,
            [
                PathBuf::from("/music/one.mp3"),
                PathBuf::from("/music/two.ogg")
            ]
        );
    }

    // Given a playlist containing local file URIs with percent-encoded names.
    // When its entries are parsed by the M3U reader.
    // Then the URIs become existing absolute filesystem paths.
    #[test]
    fn parse_entries_decodes_local_file_uris() {
        let document = "#EXTM3U\n#EXTINF:1,Track\nfile:///music/Six%20Senses%20%26%20Terk.mp3\n";

        let entries = parse_entries(document);

        assert_eq!(entries, [PathBuf::from("/music/Six Senses & Terk.mp3")]);
    }

    // Given a playlist containing a network URI and a relative path.
    // When its entries are parsed.
    // Then neither remote nor relative entries is accepted.
    #[test]
    fn parse_entries_rejects_remote_and_relative_entries() {
        let document = "https://example.test/track.mp3\nrelative.mp3\n";

        let entries = parse_entries(document);

        assert!(entries.is_empty());
    }

    // When the document is parsed.
    // Then duplicates are retained for the hash-based importer to decide.
    #[test]
    fn parse_entries_retains_duplicate_paths() {
        let entries = parse_entries("/music/a.mp3\n/music/a.mp3\n");

        assert_eq!(
            entries,
            [PathBuf::from("/music/a.mp3"), PathBuf::from("/music/a.mp3")]
        );
    }

    // Given a supported M3U filename.
    // When its stem is requested.
    // Then only the final extension is removed.
    #[test]
    fn filename_stem_removes_m3u_extension_only() {
        assert_eq!(
            filename_stem(Path::new("My.Mix.m3u")),
            Some("My.Mix".to_owned())
        );
    }

    // Given unsupported or nameless playlist paths.
    // When their stems are requested.
    // Then they are rejected.
    #[rstest::rstest]
    #[case("mix.m3u8")]
    #[case("mix.M3U")]
    #[case(".m3u")]
    #[case("   .m3u")]
    fn filename_stem_rejects_invalid_paths(#[case] path: &str) {
        assert_eq!(filename_stem(Path::new(path)), None);
    }

    // Given no playlist with the requested base name.
    // When a unique name is selected.
    // Then the base is used unchanged.
    #[test]
    fn lowest_unused_name_uses_base_when_available() {
        assert_eq!(lowest_unused_name("mix", ["other"].iter().copied()), "mix");
    }

    // Given a base and a later suffix but a free gap.
    // When a unique name is selected.
    // Then the lowest gap is reused.
    #[test]
    fn lowest_unused_name_reuses_lowest_gap() {
        assert_eq!(
            lowest_unused_name("mix", ["mix", "mix(2)"].iter().copied()),
            "mix(1)"
        );
    }

    // Given names differing only by case.
    // When a unique name is selected.
    // Then comparison remains case-sensitive.
    #[test]
    fn lowest_unused_name_is_case_sensitive() {
        assert_eq!(lowest_unused_name("mix", ["Mix"].iter().copied()), "mix");
    }
}
