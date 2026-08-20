//! Playlist feature: ordered track lists with load-on-demand contents.
//!
//! The module owns the playlist-ordering state ([`PlaylistState`],
//! [`Contents`]) and its event appliers — the only code that mutates
//! playlist ordering. Row identity is the content hash: contents are
//! ordered hash lists, and store rowids never reach the frontend. All
//! per-track display state lives in the track database
//! ([`crate::tracks`]) and is derived at render time; the appliers here
//! touch ordering only. Background tasks send [`crate::bus::Event`]s;
//! the app drains them each frame and applies them here. Contents load
//! on selection (clear → spinner → one replace event); every mutation
//! is a delta event. Persistence lives in [`store`]; the analysis
//! worker in [`queue`].

pub mod queue;
pub mod store;
pub mod view;

use automixah_engine::timeline::types::TrackHash;

use crate::bus::Event;
use crate::playlist::store::PlaylistSummary;

/// Load state of the selected playlist's contents.
///
/// The waiting semantics the app displays: selecting clears and shows a
/// spinner until the load event replaces the contents.
#[derive(Debug, Default)]
pub enum Contents {
    /// Nothing selected.
    #[default]
    None,
    /// A contents fetch is in flight (spinner).
    Loading,
    /// Content hashes of the selected playlist, position-ordered.
    Loaded(Vec<TrackHash>),
    /// The fetch failed; the message shows inline.
    Failed(String),
}

/// The playlist section's state: playlist list + selected contents.
#[derive(Debug, Default)]
pub struct PlaylistState {
    /// Known playlists, name-ordered (store order).
    pub playlists: Vec<PlaylistSummary>,
    /// Selected playlist id (`None` before first selection).
    pub selected: Option<i64>,
    /// Load state of the selected playlist's contents.
    pub contents: Contents,
    /// Outstanding add-track tasks (Add busy indicator; each terminal
    /// outcome — added, duplicate-skipped, failed — decrements).
    pub adds_in_flight: usize,
    /// Inline rename editor (context menu).
    pub rename: RenameEditor,
}

/// Inline rename editor: swaps a playlist row for an in-place field.
#[derive(Debug, Default)]
pub struct RenameEditor {
    /// The playlist being renamed (`None` when idle).
    pub id: Option<i64>,
    /// The in-progress name.
    pub buffer: String,
    /// `true` for one frame after `begin`: the row seeds focus and
    /// select-all, then clears it.
    pub pending_focus: bool,
    /// Inline rejection hint shown while editing (`None` normally).
    pub hint: Option<&'static str>,
}

impl RenameEditor {
    /// Starts editing `id`, seeded with its current name.
    pub fn begin(&mut self, id: i64, name: &str) {
        self.id = Some(id);
        self.buffer = name.to_owned();
        self.pending_focus = true;
        self.hint = None;
    }

    /// `true` when the editor targets `id`.
    #[must_use]
    pub fn matches(&self, id: i64) -> bool {
        self.id == Some(id)
    }

    /// Ends editing.
    pub fn clear(&mut self) {
        self.id = None;
        self.buffer.clear();
        self.pending_focus = false;
        self.hint = None;
    }
}

/// What the rename editor should do with its buffer on a commit
/// attempt (Enter or focus loss).
#[derive(Debug, PartialEq)]
pub(crate) enum RenameOutcome {
    /// Commit this trimmed name.
    Submit(String),
    /// Empty/whitespace input: close the editor, emit nothing.
    Revert,
    /// Name already owned by another playlist: keep editing, show hint.
    RejectDuplicate,
}

/// Decides the commit outcome for the editor against the known
/// playlists.
///
/// Duplicate detection is case-sensitive byte equality, mirroring
/// the store's `ORDER BY name` / UNIQUE (BINARY collation).
/// Renaming to the playlist's own current name is allowed.
#[must_use]
pub(crate) fn rename_outcome(
    editor: &RenameEditor,
    playlists: &[PlaylistSummary],
) -> RenameOutcome {
    let Some(id) = editor.id else {
        return RenameOutcome::Revert;
    };
    let trimmed = editor.buffer.trim();
    if trimmed.is_empty() {
        return RenameOutcome::Revert;
    }
    if playlists.iter().any(|p| p.id != id && p.name == trimmed) {
        return RenameOutcome::RejectDuplicate;
    }
    RenameOutcome::Submit(trimmed.to_owned())
}

impl PlaylistState {
    /// `true` while any shown content hash is queued or analyzing
    /// (drives repaint).
    #[must_use]
    pub fn any_pending(&self, tracks: &crate::tracks::Tracks) -> bool {
        matches!(&self.contents, Contents::Loaded(hashes) if hashes
            .iter()
            .any(|h| tracks
                .get(h)
                .is_none_or(|r| matches!(r.analysis, crate::tracks::AnalysisState::Queued
                    | crate::tracks::AnalysisState::Analyzing))))
    }

    /// Applies a bus event to the playlist ordering state.
    ///
    /// Track-record mutations (tags, analysis) belong to the track
    /// database appliers, not here. Stale events (playlists that no
    /// longer apply to the current selection) are dropped silently —
    /// the immediate-mode model only ever renders confirmed state.
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::PlaylistsLoaded(playlists) => self.playlists = playlists.clone(),
            Event::PlaylistCreated(summary) => self.playlists.push(summary.clone()),
            Event::PlaylistRenamed { id, name } => {
                if let Some(p) = self.playlists.iter_mut().find(|p| p.id == *id) {
                    p.name = name.clone();
                }
                // The store lists name-ordered; keep the list matching.
                self.playlists.sort_by(|a, b| a.name.cmp(&b.name));
            }
            Event::PlaylistDeleted(id) => {
                if self.selected == Some(*id) {
                    self.selected = None;
                    self.contents = Contents::None;
                }
                self.playlists.retain(|p| p.id != *id);
            }
            Event::RowsLoaded {
                playlist_id,
                hashes,
                ..
            } => {
                if self.selected == Some(*playlist_id) {
                    self.contents = Contents::Loaded(hashes.clone());
                }
            }
            Event::RowsLoadFailed { playlist_id, .. } => {
                if self.selected == Some(*playlist_id) {
                    self.contents = Contents::Failed("load failed".to_owned());
                }
            }
            Event::RowAdded { playlist_id, hash } => {
                if self.selected == Some(*playlist_id)
                    && let Contents::Loaded(hashes) = &mut self.contents
                    && !hashes.contains(hash)
                {
                    hashes.push(hash.clone());
                }
                self.adds_in_flight = self.adds_in_flight.saturating_sub(1);
            }
            Event::RowRemoved { playlist_id, hash } => {
                if self.selected == Some(*playlist_id)
                    && let Contents::Loaded(hashes) = &mut self.contents
                {
                    hashes.retain(|h| h != hash);
                }
            }
            Event::RowsReordered {
                playlist_id,
                hashes: order,
            } => {
                if self.selected == Some(*playlist_id)
                    && let Contents::Loaded(hashes) = &mut self.contents
                {
                    reorder_by_hashes(hashes, order);
                }
            }
            Event::DuplicateSkipped { .. } => {
                self.adds_in_flight = self.adds_in_flight.saturating_sub(1);
            }
            Event::AddStarted { count } => {
                self.adds_in_flight = self.adds_in_flight.saturating_add(*count);
            }
            Event::AddFailed { .. } => {
                self.adds_in_flight = self.adds_in_flight.saturating_sub(1);
            }
            // Track-record events and editor events are applied
            // elsewhere; ordering has nothing to do with them.
            Event::TagsResolved { .. }
            | Event::AnalysisStarted { .. }
            | Event::AnalysisDone { .. }
            | Event::AnalysisFailed { .. }
            | Event::LoadStage(_)
            | Event::LoadDone(_)
            | Event::GridSaved { .. }
            | Event::GridSaveFailed(_)
            | Event::CommandFailed(_) => {}
        }
    }
}

/// Reorders contents into the given hash order (the store's confirmed
/// order). Unknown hashes (stale confirmations) are ignored; the set is
/// unchanged.
fn reorder_by_hashes(hashes: &mut Vec<TrackHash>, order: &[TrackHash]) {
    let mut next = Vec::with_capacity(hashes.len());
    for hash in order {
        if hashes.contains(hash) {
            next.push(hash.clone());
        }
    }
    if next.len() == hashes.len() {
        *hashes = next;
    }
}

/// Splices a hash to a new index in the contents.
///
/// Returns the moved entry's new index; `None` when either hash is
/// unknown (stale drag) or the indices coincide.
pub fn move_row(state: &mut PlaylistState, from: &TrackHash, to: &TrackHash) -> Option<usize> {
    let Contents::Loaded(hashes) = &mut state.contents else {
        return None;
    };
    let from_idx = hashes.iter().position(|h| h == from)?;
    let to_idx = hashes.iter().position(|h| h == to)?;
    if from_idx == to_idx {
        return Some(from_idx);
    }
    let hash = hashes.remove(from_idx);
    hashes.insert(to_idx, hash);
    Some(to_idx)
}

/// Removes a hash from the contents.
///
/// Returns the removed hash's index for the store's position-addressed
/// remove; `None` when the hash is unknown.
pub fn remove_row(state: &mut PlaylistState, hash: &TrackHash) -> Option<usize> {
    let Contents::Loaded(hashes) = &mut state.contents else {
        return None;
    };
    let idx = hashes.iter().position(|h| h == hash)?;
    hashes.remove(idx);
    Some(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(id: u32) -> TrackHash {
        TrackHash(format!("h{id}"))
    }

    fn loaded(hashes: Vec<TrackHash>) -> PlaylistState {
        PlaylistState {
            selected: Some(7),
            contents: Contents::Loaded(hashes),
            ..Default::default()
        }
    }

    // Given a selected playlist.
    // When a rows-loaded event arrives for it.
    // Then the contents replace whatever was showing.
    #[test]
    fn rows_loaded_replaces_contents_for_selected_playlist() {
        let mut state = PlaylistState {
            contents: Contents::Loading,
            ..Default::default()
        };
        state.selected = Some(3);

        state.apply(&Event::RowsLoaded {
            playlist_id: 3,
            hashes: vec![hash(1)],
            records: Vec::new(),
        });

        assert!(matches!(&state.contents, Contents::Loaded(h) if h.len() == 1));
    }

    // Given a selected playlist.
    // When a rows-loaded event arrives for another playlist.
    // Then it is dropped (stale).
    #[test]
    fn stale_rows_loaded_is_dropped() {
        let mut state = PlaylistState {
            selected: Some(3),
            contents: Contents::Loading,
            ..Default::default()
        };

        state.apply(&Event::RowsLoaded {
            playlist_id: 4,
            hashes: vec![hash(1)],
            records: Vec::new(),
        });

        assert!(matches!(state.contents, Contents::Loading));
    }

    // Given a loaded playlist.
    // When a row-added event carries a new hash.
    // Then the hash appends exactly once.
    #[test]
    fn row_added_appends_hash_once() {
        let mut state = loaded(vec![hash(1), hash(2)]);

        state.apply(&Event::RowAdded {
            playlist_id: 7,
            hash: hash(3),
        });
        state.apply(&Event::RowAdded {
            playlist_id: 7,
            hash: hash(3),
        });

        let Contents::Loaded(hashes) = state.contents else {
            panic!("loaded");
        };
        assert_eq!(hashes.len(), 3, "duplicate add is a no-op on contents");
    }

    fn rename_editor(id: i64, buffer: &str) -> RenameEditor {
        let mut editor = RenameEditor::default();
        editor.begin(id, "seed");
        editor.buffer = buffer.to_owned();
        editor
    }

    // Given an editor with a padded non-empty buffer.
    // When deciding the rename outcome.
    // Then it submits the trimmed name.
    #[test]
    fn rename_decision_trims_nonempty_buffer() {
        let editor = rename_editor(1, "  house  ");

        let outcome = rename_outcome(&editor, &[]);

        assert_eq!(outcome, RenameOutcome::Submit("house".to_owned()));
    }

    // Given an editor whose buffer is empty or whitespace-only.
    // When deciding the rename outcome.
    // Then it reverts (nothing to submit).
    #[rstest::rstest]
    #[case("")]
    #[case("   ")]
    fn rename_decision_whitespace_reverts(#[case] buffer: &str) {
        let editor = rename_editor(1, buffer);

        let outcome = rename_outcome(&editor, &[]);

        assert_eq!(outcome, RenameOutcome::Revert);
    }

    // Given an editor targeting a name another playlist already has.
    // When deciding the rename outcome.
    // Then the duplicate is rejected (editing continues with a hint).
    #[test]
    fn rename_duplicate_name_keeps_editing() {
        let editor = rename_editor(1, "taken");
        let playlists = [PlaylistSummary {
            id: 2,
            name: "taken".to_owned(),
        }];

        let outcome = rename_outcome(&editor, &playlists);

        assert_eq!(outcome, RenameOutcome::RejectDuplicate);
    }

    // Given an editor whose buffer equals its own playlist's name.
    // When deciding the rename outcome.
    // Then it is not a duplicate and submits.
    #[test]
    fn rename_same_name_is_not_duplicate() {
        let editor = rename_editor(1, "mine");
        let playlists = [PlaylistSummary {
            id: 1,
            name: "mine".to_owned(),
        }];

        let outcome = rename_outcome(&editor, &playlists);

        assert_eq!(outcome, RenameOutcome::Submit("mine".to_owned()));
    }

    // Given two playlists where renaming moves one between the others.
    // When the rename event applies.
    // Then the list re-sorts into name order (the store's order).
    #[test]
    fn playlist_renamed_resorts_list() {
        let mut state = PlaylistState {
            playlists: vec![
                PlaylistSummary {
                    id: 1,
                    name: "A".to_owned(),
                },
                PlaylistSummary {
                    id: 2,
                    name: "C".to_owned(),
                },
            ],
            ..Default::default()
        };

        state.apply(&Event::PlaylistRenamed {
            id: 2,
            name: "B".to_owned(),
        });

        let names: Vec<&str> = state.playlists.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["A", "B"], "sorted by name");
    }

    // Given the selected playlist is deleted.
    // When the delete event applies.
    // Then selection clears and contents reset.
    #[test]
    fn playlist_deleted_clears_selection_when_selected() {
        let mut state = loaded(vec![hash(1)]);
        state.playlists = vec![PlaylistSummary {
            id: 7,
            name: "p".to_owned(),
        }];

        state.apply(&Event::PlaylistDeleted(7));

        assert_eq!(state.selected, None);
        assert!(matches!(state.contents, Contents::None));
    }

    // Given three rows.
    // When the first is moved onto the third.
    // Then order becomes 2,3,1.
    #[test]
    fn move_row_splices_by_hash() {
        let mut state = loaded(vec![hash(1), hash(2), hash(3)]);

        let _ = move_row(&mut state, &hash(1), &hash(3));

        let Contents::Loaded(hashes) = state.contents else {
            panic!("loaded");
        };
        assert_eq!(hashes, vec![hash(2), hash(3), hash(1)], "spliced order");
    }

    // Given rows and an unknown drag source.
    // When moved.
    // Then nothing changes and None returns.
    #[test]
    fn move_row_with_unknown_hash_is_ignored() {
        let mut state = loaded(vec![hash(1), hash(2)]);

        let result = move_row(&mut state, &hash(99), &hash(1));

        assert!(result.is_none());
        let Contents::Loaded(hashes) = state.contents else {
            panic!("loaded");
        };
        assert_eq!(hashes.len(), 2, "untouched");
    }

    // Given three rows.
    // When the middle one is removed.
    // Then the store tuple (index) returns and the hash is gone.
    #[test]
    fn remove_row_returns_index_and_splices() {
        let mut state = loaded(vec![hash(1), hash(2), hash(3)]);

        let idx = remove_row(&mut state, &hash(2)).expect("remove");

        assert_eq!(idx, 1, "stored position of the removed row");
        let Contents::Loaded(hashes) = state.contents else {
            panic!("loaded");
        };
        assert_eq!(hashes, vec![hash(1), hash(3)]);
    }

    // Given a reorder confirmation carrying the new hash order.
    // When applied.
    // Then contents follow it exactly.
    #[test]
    fn rows_reordered_follows_confirmed_order() {
        let mut state = loaded(vec![hash(1), hash(2), hash(3)]);

        state.apply(&Event::RowsReordered {
            playlist_id: 7,
            hashes: vec![hash(3), hash(1), hash(2)],
        });

        let Contents::Loaded(hashes) = state.contents else {
            panic!("loaded");
        };
        assert_eq!(hashes, vec![hash(3), hash(1), hash(2)]);
    }

    // Given a batch of adds where every task succeeds.
    // When the last row-added applies.
    // Then the in-flight count reaches zero (spinner cleared).
    #[test]
    fn successful_add_batch_reaches_zero_in_flight() {
        let mut state = PlaylistState::default();
        state.apply(&Event::AddStarted { count: 3 });

        for id in 1..=3 {
            state.apply(&Event::RowAdded {
                playlist_id: 1,
                hash: hash(id),
            });
        }

        assert_eq!(state.adds_in_flight, 0, "spinner cleared");
    }

    // Given a batch with one failure and one duplicate skip.
    // When the terminal events apply.
    // Then the count still reaches zero.
    #[test]
    fn mixed_add_outcomes_reach_zero_in_flight() {
        let mut state = PlaylistState::default();
        state.apply(&Event::AddStarted { count: 3 });
        state.apply(&Event::RowAdded {
            playlist_id: 1,
            hash: hash(1),
        });
        state.apply(&Event::DuplicateSkipped {
            playlist_id: 1,
            path: "/x".to_owned(),
        });
        state.apply(&Event::AddFailed {
            message: "read failed".to_owned(),
        });

        assert_eq!(state.adds_in_flight, 0, "every terminal decremented");
    }

    // Given an unrelated command failure while adds are in flight.
    // When it applies.
    // Then the in-flight count is untouched.
    #[test]
    fn command_failed_never_touches_add_count() {
        let mut state = PlaylistState::default();
        state.apply(&Event::AddStarted { count: 2 });

        state.apply(&Event::CommandFailed("create playlist: boom".to_owned()));

        assert_eq!(state.adds_in_flight, 2, "unrelated failure ignored");
    }
}
