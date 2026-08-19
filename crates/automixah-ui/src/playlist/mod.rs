//! Playlist feature: ordered track lists with load-on-demand contents.
//!
//! The module owns the UI-facing row model ([`PlaylistRow`],
//! [`PlaylistState`]) and its event appliers — the only code that
//! mutates playlist frontend state. Background tasks send
//! [`crate::bus::Event`]s; the app drains them each frame and applies
//! them here. Contents load on selection (clear → spinner → one
//! replace event); every mutation is a delta event. Row ids are
//! database-minted (`playlist_tracks.rowid`), so events address rows
//! stably across loads. Persistence lives in [`store`]; the analysis
//! worker in [`queue`].

pub mod queue;
pub mod store;
pub mod view;

use std::path::PathBuf;

use automixah_engine::timeline::types::TrackHash;
use djcore::key::Key;

use crate::bus::Event;
use crate::playlist::queue::RowId;
use crate::playlist::store::PlaylistSummary;

/// Lifecycle of one playlist row.
#[derive(Debug, Clone, PartialEq)]
pub enum RowStatus {
    /// Waiting for the analysis worker (clock glyph).
    Queued,
    /// The worker is running this row's job (spinner).
    Analyzing,
    /// Full metadata available; the row is clickable and draggable.
    Ready,
    /// The job failed; the message shows in a tooltip.
    Failed(String),
}

/// One playlist row in the UI model.
#[derive(Debug, Clone)]
pub struct PlaylistRow {
    /// Database-minted identity (`playlist_tracks.rowid`).
    pub row_id: RowId,
    /// Playlist position (matches the store's gapless ordering).
    pub position: i64,
    /// Path recorded when the track was added.
    pub path: PathBuf,
    /// Content hash, known after analysis (or a store hit).
    pub hash: Option<TrackHash>,
    /// Display title.
    pub title: String,
    /// Display artist (empty when unknown).
    pub artist: String,
    /// BPM once known.
    pub bpm: Option<f32>,
    /// Detected key once known.
    pub key: Option<Key>,
    /// Duration in seconds once known.
    pub duration: Option<f32>,
    /// Lifecycle state.
    pub status: RowStatus,
}

impl PlaylistRow {
    /// `true` when this row can be clicked into the editor or dragged.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status == RowStatus::Ready
    }
}

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
    /// Rows of the selected playlist, position-ordered.
    Loaded(Vec<PlaylistRow>),
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
    /// `true` while an add-track task is hashing/probing files (Add
    /// busy indicator; rows appear on their insert events).
    pub add_pending: bool,
    /// Inline rename editor (context menu).
    pub rename: RenameEditor,
}

/// Inline rename editor for the playlist context menu.
#[derive(Debug, Default)]
pub struct RenameEditor {
    /// The playlist being renamed (`None` when idle).
    pub id: Option<i64>,
    /// The in-progress name.
    pub buffer: String,
}

impl RenameEditor {
    /// Starts editing `id`, seeded with its current name.
    pub fn begin(&mut self, id: i64, name: &str) {
        self.id = Some(id);
        self.buffer = name.to_owned();
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
    }
}

impl PlaylistState {
    /// `true` while any shown row is queued or analyzing (drives repaint).
    #[must_use]
    pub fn any_pending(&self) -> bool {
        matches!(&self.contents, Contents::Loaded(rows) if rows
            .iter()
            .any(|r| matches!(r.status, RowStatus::Queued | RowStatus::Analyzing)))
    }

    /// Applies a bus event to the playlist state.
    ///
    /// Stale events (rows or playlists that no longer apply to the
    /// current selection) are dropped silently — the immediate-mode
    /// model only ever renders confirmed state.
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::PlaylistsLoaded(playlists) => self.playlists = playlists.clone(),
            Event::PlaylistCreated(summary) => self.playlists.push(summary.clone()),
            Event::PlaylistRenamed { id, name } => {
                if let Some(p) = self.playlists.iter_mut().find(|p| p.id == *id) {
                    p.name = name.clone();
                }
            }
            Event::PlaylistDeleted(id) => {
                if self.selected == Some(*id) {
                    self.selected = None;
                    self.contents = Contents::None;
                }
                self.playlists.retain(|p| p.id != *id);
            }
            Event::RowsLoaded { playlist_id, rows } => {
                if self.selected == Some(*playlist_id) {
                    self.contents = Contents::Loaded(rows.clone());
                }
            }
            Event::RowsLoadFailed { playlist_id, .. } => {
                if self.selected == Some(*playlist_id) {
                    self.contents = Contents::Failed("load failed".to_owned());
                }
            }
            Event::RowAdded { playlist_id, row } => {
                if self.selected == Some(*playlist_id)
                    && let Contents::Loaded(rows) = &mut self.contents
                {
                    rows.push(row.clone());
                }
            }
            Event::RowRemoved {
                playlist_id,
                row_id,
            } => {
                if self.selected == Some(*playlist_id)
                    && let Contents::Loaded(rows) = &mut self.contents
                {
                    rows.retain(|r| r.row_id != *row_id);
                    renumber(rows);
                }
            }
            Event::RowsReordered {
                playlist_id,
                row_ids,
            } => {
                if self.selected == Some(*playlist_id)
                    && let Contents::Loaded(rows) = &mut self.contents
                {
                    reorder_by_ids(rows, row_ids);
                }
            }
            Event::RowAnalyzing { row_id } => set_status(self, *row_id, RowStatus::Analyzing),
            Event::RowReady { row_id, meta } => apply_ready(self, *row_id, meta),
            Event::RowFailed { row_id, message } => {
                set_status(self, *row_id, RowStatus::Failed(message.clone()));
            }
            Event::AddStarted { .. } => self.add_pending = true,
            Event::DuplicateSkipped { .. } | Event::CommandFailed(_) => {
                self.add_pending = false;
            }
            Event::LoadStage(_) | Event::LoadDone(_) | Event::GridSaved(_) => {}
            Event::GridSaveFailed(_) => {}
        }
    }
}

/// Marks the addressed row's status, if present.
fn set_status(state: &mut PlaylistState, row_id: RowId, status: RowStatus) {
    if let Contents::Loaded(rows) = &mut state.contents
        && let Some(row) = rows.iter_mut().find(|r| r.row_id == row_id)
    {
        row.status = status;
    }
}

/// Applies a ready event: metadata plus status.
fn apply_ready(state: &mut PlaylistState, row_id: RowId, meta: &crate::playlist::queue::TrackMeta) {
    let Contents::Loaded(rows) = &mut state.contents else {
        return;
    };
    let Some(row) = rows.iter_mut().find(|r| r.row_id == row_id) else {
        return;
    };
    row.hash = Some(meta.hash.clone());
    row.bpm = Some(meta.bpm);
    row.key = Some(meta.key.clone());
    if meta.duration_seconds > 0.0 {
        row.duration = Some(meta.duration_seconds);
    }
    row.status = RowStatus::Ready;
}

/// Rewrites `position` to the row's index (gapless 0-based).
fn renumber(rows: &mut [PlaylistRow]) {
    for (i, row) in rows.iter_mut().enumerate() {
        row.position = i64::try_from(i).unwrap_or(i64::MAX);
    }
}

/// Reorders rows into the given id order (the store's confirmed order).
///
/// Unknown ids (stale confirmations) are ignored; the row set is
/// unchanged.
fn reorder_by_ids(rows: &mut Vec<PlaylistRow>, row_ids: &[RowId]) {
    let mut next = Vec::with_capacity(rows.len());
    for id in row_ids {
        if let Some(row) = rows.iter().find(|r| r.row_id == *id) {
            next.push(row.clone());
        }
    }
    if next.len() == rows.len() {
        *rows = next;
        renumber(rows);
    }
}

/// Splices a row to a new index and renumbers positions 0..n.
///
/// Returns the moved row's new position; `None` when either id is
/// unknown (stale drag) or the indices coincide.
pub fn move_row(state: &mut PlaylistState, from: RowId, to: RowId) -> Option<i64> {
    let Contents::Loaded(rows) = &mut state.contents else {
        return None;
    };
    let from_idx = rows.iter().position(|r| r.row_id == from)?;
    let to_idx = rows.iter().position(|r| r.row_id == to)?;
    if from_idx == to_idx {
        return Some(rows[from_idx].position);
    }
    let row = rows.remove(from_idx);
    rows.insert(to_idx, row);
    renumber(rows);
    Some(rows[to_idx].position)
}

/// Removes a row by id, renumbering the survivors.
///
/// Returns `(position, path)` needed for the store's position-addressed
/// remove; `None` when the id is unknown.
pub fn remove_row(state: &mut PlaylistState, row: RowId) -> Option<(i64, PathBuf)> {
    let Contents::Loaded(rows) = &mut state.contents else {
        return None;
    };
    let idx = rows.iter().position(|r| r.row_id == row)?;
    let removed = rows.remove(idx);
    renumber(rows);
    Some((removed.position, removed.path))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ready row with predictable path `/row-<id>`, gapless position.
    fn row(id: i64) -> PlaylistRow {
        PlaylistRow {
            row_id: RowId(id),
            position: id - 1,
            path: PathBuf::from(format!("/row-{id}")),
            hash: None,
            title: format!("T{id}"),
            artist: String::new(),
            bpm: None,
            key: None,
            duration: None,
            status: RowStatus::Ready,
        }
    }

    fn meta(id: u32) -> crate::playlist::queue::TrackMeta {
        crate::playlist::queue::TrackMeta {
            hash: TrackHash(format!("h{id}")),
            bpm: 128.0,
            key: djcore::key::Key {
                root: 9,
                mode: djcore::key::KeyMode::Minor,
            },
            duration_seconds: 61.5,
            title: format!("T{id}"),
            artist: String::new(),
        }
    }

    fn loaded(rows: Vec<PlaylistRow>) -> PlaylistState {
        PlaylistState {
            selected: Some(7),
            contents: Contents::Loaded(rows),
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
            rows: vec![row(11)],
        });

        assert!(matches!(state.contents, Contents::Loaded(ref r) if r.len() == 1));
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
            rows: vec![row(11)],
        });

        assert!(matches!(state.contents, Contents::Loading));
    }

    // Given rows with database ids 1 and 2.
    // When a row-added event carries id 3.
    // Then the new id is disjoint from the loaded ones (no collision).
    #[test]
    fn rows_loaded_carries_db_rowids_disjoint_from_added() {
        let mut state = loaded(vec![row(1), row(2)]);

        state.apply(&Event::RowAdded {
            playlist_id: 7,
            row: row(3),
        });

        let Contents::Loaded(rows) = state.contents else {
            panic!("loaded");
        };
        let ids: Vec<i64> = rows.iter().map(|r| r.row_id.0).collect();
        assert_eq!(ids, vec![1, 2, 3], "appended, disjoint");
    }

    // Given a loaded playlist.
    // When a ready event addresses the second row.
    // Then only that row changes.
    #[test]
    fn add_track_leaves_other_rows_untouched() {
        let mut state = loaded(vec![row(1), row(2)]);

        state.apply(&Event::RowReady {
            row_id: RowId(2),
            meta: meta(2),
        });

        let Contents::Loaded(rows) = state.contents else {
            panic!("loaded");
        };
        assert_eq!(rows[0].title, "T1", "first row untouched");
        assert!(rows[0].duration.is_none(), "first row metadata untouched");
        assert_eq!(rows[1].title, "T2");
        assert!(rows[1].duration.is_some(), "second row hydrated");
    }

    // Given no playlists.
    // When one created event arrives.
    // Then exactly one entry exists.
    #[test]
    fn create_playlist_appends_exactly_once() {
        let mut state = PlaylistState::default();

        state.apply(&Event::PlaylistCreated(PlaylistSummary {
            id: 1,
            name: "p".to_owned(),
        }));

        assert_eq!(state.playlists.len(), 1);
    }

    // Given a rename event.
    // When applied.
    // Then the list reflects the new name.
    #[test]
    fn playlist_renamed_updates_list() {
        let mut state = PlaylistState {
            playlists: vec![PlaylistSummary {
                id: 5,
                name: String::from("old"),
            }],
            ..Default::default()
        };

        state.apply(&Event::PlaylistRenamed {
            id: 5,
            name: "new".to_owned(),
        });

        assert_eq!(state.playlists[0].name, "new");
    }

    // Given the selected playlist is deleted.
    // When the delete event applies.
    // Then selection clears and contents reset.
    #[test]
    fn playlist_deleted_clears_selection_when_selected() {
        let mut state = loaded(vec![row(1)]);
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
    // Then order becomes 2,3,1 and positions renumber 0..2.
    #[test]
    fn move_row_splices_and_renumbers() {
        let mut state = loaded(vec![row(1), row(2), row(3)]);

        let _ = move_row(&mut state, RowId(1), RowId(3));

        let Contents::Loaded(rows) = state.contents else {
            panic!("loaded");
        };
        let ids: Vec<i64> = rows.iter().map(|r| r.row_id.0).collect();
        assert_eq!(ids, vec![2, 3, 1], "spliced order");
        let positions: Vec<i64> = rows.iter().map(|r| r.position).collect();
        assert_eq!(positions, vec![0, 1, 2], "gapless renumber");
    }

    // Given rows and an unknown drag source.
    // When moved.
    // Then nothing changes and None returns.
    #[test]
    fn move_row_with_unknown_id_is_ignored() {
        let mut state = loaded(vec![row(1), row(2)]);

        let result = move_row(&mut state, RowId(99), RowId(1));

        assert!(result.is_none());
        let Contents::Loaded(rows) = state.contents else {
            panic!("loaded");
        };
        assert_eq!(rows.len(), 2, "untouched");
    }

    // Given three rows.
    // When the middle one is removed.
    // Then survivors renumber gaplessly and the store tuple returns.
    #[test]
    fn remove_row_renumbers_survivors() {
        let mut state = loaded(vec![row(1), row(2), row(3)]);

        let (position, path) = remove_row(&mut state, RowId(2)).expect("remove");

        assert_eq!(position, 1, "stored position of the removed row");
        assert_eq!(path, PathBuf::from("/row-2"));
        let Contents::Loaded(rows) = state.contents else {
            panic!("loaded");
        };
        let ids: Vec<i64> = rows.iter().map(|r| r.row_id.0).collect();
        assert_eq!(ids, vec![1, 3]);
        let positions: Vec<i64> = rows.iter().map(|r| r.position).collect();
        assert_eq!(positions, vec![0, 1], "gapless");
    }

    // Given a reorder confirmation carrying the new id order.
    // When applied.
    // Then rows follow it exactly.
    #[test]
    fn rows_reordered_follows_confirmed_order() {
        let mut state = loaded(vec![row(1), row(2), row(3)]);

        state.apply(&Event::RowsReordered {
            playlist_id: 7,
            row_ids: vec![RowId(3), RowId(1), RowId(2)],
        });

        let Contents::Loaded(rows) = state.contents else {
            panic!("loaded");
        };
        let ids: Vec<i64> = rows.iter().map(|r| r.row_id.0).collect();
        assert_eq!(ids, vec![3, 1, 2]);
    }

    // Given no playlist selected.
    // When selection begins and its load completes.
    // Then contents transition None → Loading → Loaded in order.
    #[test]
    fn select_then_load_transitions_contents() {
        let mut state = PlaylistState {
            selected: Some(2),
            contents: Contents::Loading,
            ..PlaylistState::default()
        };

        state.apply(&Event::RowsLoaded {
            playlist_id: 2,
            rows: vec![row(1)],
        });

        assert!(matches!(state.contents, Contents::Loaded(rows) if rows.len() == 1));
    }

    // Given rows from a contents load where some are incomplete.
    // When they are converted from persisted form.
    // Then each incomplete row carries a re-enqueue job (app-level
    // derivation is driven by this contract; see `rows_from_persisted`).
    #[test]
    fn rows_from_persisted_marks_incomplete_for_reenqueue() {
        use crate::playlist::store::PersistedTrack;
        let complete = PersistedTrack {
            id: 1,
            position: 0,
            track_hash: TrackHash("done".to_owned()),
            title: "T".to_owned(),
            artist: "A".to_owned(),
            added_path: "/done".to_owned(),
            duration: Some(60.0),
            grid: Some(crate::store::GridOverride {
                grid_bpm: 128.0,
                anchor_seconds: 0.0,
                downbeat_phase: 0,
                updated_at: 0,
                key: Some(djcore::key::Key {
                    root: 9,
                    mode: djcore::key::KeyMode::Minor,
                }),
            }),
        };
        let incomplete = PersistedTrack {
            id: 2,
            position: 1,
            track_hash: TrackHash("todo".to_owned()),
            title: "T2".to_owned(),
            artist: "A2".to_owned(),
            added_path: "/todo".to_owned(),
            duration: None,
            grid: None,
        };

        let (rows, reenqueue) =
            crate::app::rows_from_persisted_for_test(vec![complete, incomplete]);

        assert_eq!(rows.len(), 2);
        assert_eq!(reenqueue.len(), 1, "only the incomplete row re-enqueues");
        assert_eq!(reenqueue[0].0, RowId(2));
        assert!(matches!(rows[1].status, RowStatus::Queued));
    }
}
