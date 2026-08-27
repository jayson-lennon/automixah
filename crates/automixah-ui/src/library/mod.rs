//! The track library: a scanned, persisted index of the user's music
//! directories.
//!
//! Roots and their indexed files persist through the [`store::LibraryStore`]
//! trait; the scanner (`scan`) refreshes the index from disk; the
//! frontend state ([`LibraryState`]) mutates only in the app's event
//! applier like every other UI-owned fact. The visible/filtered view is
//! derived at render time, never stored.

pub mod filter;
pub mod scan;
pub mod sort;
pub mod store;
pub mod view;

use crate::bus::Event;
pub use store::{
    IndexedFile, LibraryEntry, LibraryRoot, LibraryStore, LibraryStoreError, LibraryStoreService,
};

/// The library section's state: roots, indexed entries, scan lifecycle.
///
/// Mutated only when applying bus events (`apply`); the filter buffer is
/// view-owned (like the render path field) and lives on the app, not
/// here.
#[derive(Debug, Default)]
pub struct LibraryState {
    /// Known roots, store order (path).
    pub roots: Vec<LibraryRoot>,
    /// Indexed files, store order `(root_id, rel_path)`.
    pub entries: Vec<LibraryEntry>,
    /// `true` from `LibraryScanStarted` until a terminal scan event.
    pub scanning: bool,
    /// Latest progress echo while scanning.
    pub progress: Option<ScanProgress>,
}

/// Echo of one `LibraryScanProgress` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanProgress {
    /// Supported-audio files examined so far.
    pub files_done: usize,
    /// Supported-audio files discovered so far (the walk discovers
    /// incrementally; not a final total).
    pub files_seen: usize,
}

impl LibraryState {
    /// Applies a bus event to the library state.
    ///
    /// Non-library events are ignored. Stale or duplicate events are
    /// absorbed naturally: `LibraryLoaded` replaces wholesale, and the
    /// scan flags are idempotent.
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::LibraryLoaded { roots, entries, .. } => {
                self.roots = roots.clone();
                self.entries = entries.clone();
                // The `analyzed` set belongs to the enqueue derivation
                // (app layer), not to display state.
            }
            Event::LibraryRootAdded(root) => {
                if !self.roots.iter().any(|known| known.id == root.id) {
                    self.roots.push(root.clone());
                    self.roots.sort_by(|a, b| a.path.cmp(&b.path));
                }
            }
            Event::LibraryRootRemoved(id) => {
                self.roots.retain(|root| root.id != *id);
                self.entries.retain(|entry| entry.root_id != *id);
            }
            Event::LibraryScanStarted => self.scanning = true,
            Event::LibraryScanProgress {
                files_done,
                files_seen,
            } => {
                self.progress = Some(ScanProgress {
                    files_done: *files_done,
                    files_seen: *files_seen,
                });
            }
            // Terminal outcomes; `LibraryLoaded` (sent right after Done)
            // refreshes the entries.
            Event::LibraryScanDone { .. } | Event::LibraryScanFailed { .. } => {
                self.scanning = false;
                self.progress = None;
            }
            _ => {}
        }
    }

    /// `true` while a scan is running (drives repaint + disabled
    /// buttons).
    #[must_use]
    pub fn is_scanning(&self) -> bool {
        self.scanning
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root(id: i64, path: &str) -> LibraryRoot {
        LibraryRoot {
            id,
            path: PathBuf::from(path),
        }
    }

    fn entry(root_id: i64, rel: &str) -> LibraryEntry {
        LibraryEntry {
            root_id,
            rel_path: PathBuf::from(rel),
            hash: automixah_engine::timeline::types::TrackHash(rel.to_owned()),
            title: rel.to_owned(),
            artist: String::new(),
            duration: None,
            bpm: None,
            key: None,
            mtime_secs: 0,
            size_bytes: 0,
        }
    }

    // Given a default state.
    // When a LibraryLoaded event applies.
    // Then roots and entries replace wholesale.
    #[test]
    fn library_loaded_replaces_state() {
        let mut state = LibraryState::default();
        state.roots.push(root(1, "/old"));
        state.entries.push(entry(1, "old.flac"));

        state.apply(&Event::LibraryLoaded {
            roots: vec![root(2, "/new")],
            entries: vec![entry(2, "new.flac")],
            analyzed: std::collections::HashSet::new(),
        });

        assert_eq!(state.roots, vec![root(2, "/new")]);
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].root_id, 2);
    }

    // Given a state with one root.
    // When a different root is added by event.
    // Then the list grows sorted by path.
    #[test]
    fn root_added_appends_sorted() {
        let mut state = LibraryState::default();
        state.roots.push(root(1, "/b"));

        state.apply(&Event::LibraryRootAdded(root(2, "/a")));

        assert_eq!(state.roots.len(), 2);
        assert_eq!(state.roots[0].path, PathBuf::from("/a"), "sorted by path");
    }

    // Given two roots with entries.
    // When one root is removed by event.
    // Then only the other root's entries survive.
    #[test]
    fn root_removed_retains_other_roots_entries() {
        let mut state = LibraryState {
            roots: vec![root(1, "/a"), root(2, "/b")],
            entries: vec![entry(1, "one.flac"), entry(2, "two.flac")],
            ..LibraryState::default()
        };

        state.apply(&Event::LibraryRootRemoved(1));

        assert_eq!(state.roots, vec![root(2, "/b")]);
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].root_id, 2);
    }

    // Given an idle state.
    // When scan lifecycle events apply in order.
    // Then scanning toggles true then false, progress echoes.
    #[test]
    fn scan_lifecycle_toggles_scanning() {
        let mut state = LibraryState::default();

        state.apply(&Event::LibraryScanStarted);
        assert!(state.is_scanning());

        state.apply(&Event::LibraryScanProgress {
            files_done: 3,
            files_seen: 4,
        });
        assert_eq!(
            state.progress,
            Some(ScanProgress {
                files_done: 3,
                files_seen: 4
            })
        );

        state.apply(&Event::LibraryScanDone {
            added: 1,
            updated: 0,
            pruned: 0,
        });
        assert!(!state.is_scanning());
        assert!(state.progress.is_none());
    }

    // Given a scanning state.
    // When the scan fails.
    // Then scanning clears as well.
    #[test]
    fn scan_failed_clears_scanning() {
        let mut state = LibraryState::default();
        state.apply(&Event::LibraryScanStarted);

        state.apply(&Event::LibraryScanFailed {
            message: "boom".to_owned(),
        });

        assert!(!state.is_scanning());
    }
}
