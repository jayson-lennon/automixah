//! The frontend track database: content hash → one complete record.
//!
//! Every fact the UI knows about a track lives here — tags and analysis
//! state in one place, keyed by the stable identity (content hash) that
//! the stores, playlists, and events all share. Playlist rows are ordered
//! hash references; all row display state derives from these records at
//! render time. The database is UI-owned and single-threaded by design:
//! mutated only when applying bus events (plus the enqueue derivation's
//! `Queued` insert), never by background tasks.

use std::collections::HashMap;
use std::path::PathBuf;

use automixah_engine::timeline::types::{CuePoints, TrackHash};
use djcore::analyzer::BeatGrid;
use djcore::key::Key;

/// Store-minted display facts for a track's source file.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackTags {
    /// Display title.
    pub title: String,
    /// Display artist (empty when unknown).
    pub artist: String,
    /// Source path recorded when the track was added.
    pub path: PathBuf,
}

/// Everything analysis produces for one track — the single package.
#[derive(Debug, Clone)]
pub struct Analysis {
    /// Effective (stored) grid: detected or the stored override, with the
    /// beats/downbeats/bars projections materialized.
    pub grid: BeatGrid,
    /// BPM from the grid.
    pub bpm: f32,
    /// Detected key.
    pub key: Key,
    /// Duration in seconds (source time).
    pub duration_seconds: f32,
    /// User-authored cue points (source frames, snapped in the UI).
    pub cues: CuePoints,
}

/// Analysis lifecycle for one content hash.
///
/// `Queued` means "no analysis knowledge" — the enqueue derivation's
/// marker. Duplicate jobs for a `Queued` hash are harmless: the worker's
/// store fast path turns the second run into a metadata-only event.
#[derive(Debug, Clone)]
pub enum AnalysisState {
    /// No analysis known; analysis is wanted.
    Queued,
    /// Analysis is running (worker or editor pipeline).
    Analyzing,
    /// Analysis known; the package is cached for rendering.
    Ready(Analysis),
    /// Terminal failure; the message shows in the row tooltip.
    Failed(String),
}

impl AnalysisState {
    /// `true` when analysis data is known and renderable.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    /// `true` while a job is enqueued or running (a pending row).
    #[must_use]
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Queued | Self::Analyzing)
    }
}

/// One track's complete frontend record — concrete, zero `Option` fields.
#[derive(Debug, Clone)]
pub struct TrackRecord {
    /// Identity (also the map key).
    pub hash: TrackHash,
    /// Source-file display facts.
    pub tags: TrackTags,
    /// Analysis lifecycle state.
    pub analysis: AnalysisState,
}

/// The track database: content hash → record.
///
/// Written only in the event-apply path and the enqueue derivation.
#[derive(Debug, Default)]
pub struct Tracks {
    by_hash: HashMap<TrackHash, TrackRecord>,
}

impl Tracks {
    /// The record for `hash`, if the database knows the track at all.
    #[must_use]
    pub fn get(&self, hash: &TrackHash) -> Option<&TrackRecord> {
        self.by_hash.get(hash)
    }

    /// `true` when the track is known AND analysis is renderable.
    #[must_use]
    pub fn is_ready(&self, hash: &TrackHash) -> bool {
        self.get(hash).is_some_and(|r| r.analysis.is_ready())
    }

    /// `true` when a queue job should be enqueued for `hash`: the track
    /// is unknown or carries no analysis knowledge (`Queued`).
    #[must_use]
    pub fn needs_job(&self, hash: &TrackHash) -> bool {
        self.get(hash)
            .is_none_or(|r| matches!(r.analysis, AnalysisState::Queued))
    }

    /// Inserts or overwrites `record` under the merge policy: incoming
    /// tags always win; analysis knowledge fills only a `Queued` (no
    /// knowledge) entry — in-session/in-flight truth wins otherwise.
    ///
    /// Hydration (contents loads) and add-task results may know less than
    /// the session already does (e.g. a record mid-re-analysis), so a
    /// stale store read never overwrites live state.
    pub fn upsert(&mut self, record: TrackRecord) {
        let entry = self.entry_or_placeholder(&record.hash);
        entry.tags = record.tags;
        if matches!(entry.analysis, AnalysisState::Queued) {
            entry.analysis = record.analysis;
        }
    }

    /// Marks `hash` `Queued` (the enqueue derivation's insert).
    pub fn mark_queued(&mut self, hash: &TrackHash) {
        self.entry_or_placeholder(hash).analysis = AnalysisState::Queued;
    }

    /// Sets the analysis state for `hash`, creating the record when absent.
    pub fn set_analysis(&mut self, hash: &TrackHash, state: AnalysisState) {
        self.entry_or_placeholder(hash).analysis = state;
    }

    /// Drops the analysis knowledge for `hash` (re-analyze step 1). Rows
    /// referencing the hash derive "needs analysis" on the next frame.
    pub fn clear_analysis(&mut self, hash: &TrackHash) {
        if let Some(record) = self.by_hash.get_mut(hash) {
            record.analysis = AnalysisState::Queued;
        }
    }

    /// Clears a terminal `Failed` state back to `Queued` (retry semantics
    /// on contents reload, for hashes the hydration found incomplete).
    pub fn retry_failed(&mut self, hashes: &[TrackHash]) {
        for hash in hashes {
            if let Some(record) = self.by_hash.get_mut(hash)
                && matches!(record.analysis, AnalysisState::Failed(_))
            {
                record.analysis = AnalysisState::Queued;
            }
        }
    }

    /// Refreshes the effective grid of a `Ready` record after a manual
    /// grid save, so derived BPM matches the edit immediately.
    pub fn refresh_grid(&mut self, hash: &TrackHash, grid: &crate::grid::EditableGrid) {
        if let Some(record) = self.by_hash.get_mut(hash)
            && let AnalysisState::Ready(analysis) = &mut record.analysis
        {
            analysis.grid = grid.project();
            analysis.bpm = grid.grid_bpm;
        }
    }

    /// Refreshes a `Ready` record's persisted cue points after a flush.
    pub fn refresh_cues(&mut self, hash: &TrackHash, cues: &CuePoints) {
        if let Some(record) = self.by_hash.get_mut(hash)
            && let AnalysisState::Ready(analysis) = &mut record.analysis
        {
            analysis.cues = *cues;
        }
    }

    /// Source path for `hash` (the queue job's file), if known.
    #[must_use]
    pub fn path_of(&self, hash: &TrackHash) -> Option<&PathBuf> {
        self.get(hash).map(|r| &r.tags.path)
    }

    /// The record for `hash`, inserting a `Queued` placeholder when the
    /// hash is unknown (tags arrive on the next hydration/add event).
    fn entry_or_placeholder(&mut self, hash: &TrackHash) -> &mut TrackRecord {
        self.by_hash
            .entry(hash.clone())
            .or_insert_with(|| TrackRecord {
                hash: hash.clone(),
                tags: TrackTags {
                    title: String::new(),
                    artist: String::new(),
                    path: PathBuf::new(),
                },
                analysis: AnalysisState::Queued,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use automixah_engine::timeline::types::CueKind;

    fn hash(id: u32) -> TrackHash {
        TrackHash(format!("h{id}"))
    }

    fn tags(id: u32) -> TrackTags {
        TrackTags {
            title: format!("T{id}"),
            artist: String::new(),
            path: PathBuf::from(format!("/t{id}")),
        }
    }

    fn analysis(bpm: f32) -> Analysis {
        Analysis {
            grid: BeatGrid {
                grid_bpm: bpm,
                ..BeatGrid::default()
            },
            bpm,
            key: djcore::key::Key {
                root: 9,
                mode: djcore::key::KeyMode::Minor,
            },
            duration_seconds: 61.0,
            cues: CuePoints::default(),
        }
    }

    fn record(id: u32, state: AnalysisState) -> TrackRecord {
        TrackRecord {
            hash: hash(id),
            tags: tags(id),
            analysis: state,
        }
    }

    // Given an empty database.
    // When looking up any hash.
    // Then nothing is found.
    #[test]
    fn missing_hash_is_none() {
        let tracks = Tracks::default();
        assert!(tracks.get(&hash(1)).is_none());
    }

    // Given a record upserted with a ready analysis.
    // When looking it up.
    // Then the same package comes back.
    #[test]
    fn upsert_then_get_round_trips() {
        let mut tracks = Tracks::default();
        tracks.upsert(record(1, AnalysisState::Ready(analysis(138.0))));

        let AnalysisState::Ready(a) = &tracks.get(&hash(1)).expect("record").analysis else {
            panic!("ready");
        };
        assert!((a.bpm - 138.0).abs() < f32::EPSILON);
    }

    // Given a record whose analysis is in flight.
    // When a hydration upsert arrives carrying a ready state.
    // Then the in-session state wins (no overwrite).
    #[test]
    fn upsert_in_flight_state_wins_over_incoming() {
        let mut tracks = Tracks::default();
        tracks.upsert(record(1, AnalysisState::Analyzing));

        tracks.upsert(record(1, AnalysisState::Ready(analysis(140.0))));

        assert!(
            matches!(
                tracks.get(&hash(1)).map(|r| &r.analysis),
                Some(AnalysisState::Analyzing)
            ),
            "live state preserved"
        );
    }

    // Given a queued (no-knowledge) record.
    // When a hydration upsert arrives carrying a ready state.
    // Then the knowledge fills the empty slot.
    #[test]
    fn upsert_fills_queued_entry_with_incoming_knowledge() {
        let mut tracks = Tracks::default();
        tracks.upsert(record(1, AnalysisState::Queued));

        tracks.upsert(record(1, AnalysisState::Ready(analysis(140.0))));

        assert!(tracks.is_ready(&hash(1)), "store knowledge fills the gap");
    }

    // Given a hydrated record.
    // When a later upsert arrives with different tags.
    // Then the incoming tags win.
    #[test]
    fn upsert_incoming_tags_win() {
        let mut tracks = Tracks::default();
        tracks.upsert(record(1, AnalysisState::Ready(analysis(138.0))));

        let mut newer = record(1, AnalysisState::Queued);
        newer.tags.title = "Retitled".to_owned();
        tracks.upsert(newer);

        assert_eq!(
            tracks.get(&hash(1)).map(|r| r.tags.title.as_str()),
            Some("Retitled")
        );
    }

    // Given hashes in every lifecycle state.
    // When asking whether a job is needed.
    // Then only unknown and queued hashes want one.
    #[test]
    fn needs_job_is_true_only_without_knowledge() {
        let mut tracks = Tracks::default();
        tracks.upsert(record(1, AnalysisState::Ready(analysis(138.0))));
        tracks.upsert(record(2, AnalysisState::Analyzing));
        tracks.upsert(record(3, AnalysisState::Failed("boom".to_owned())));
        tracks.upsert(record(4, AnalysisState::Queued));

        assert!(!tracks.needs_job(&hash(1)), "ready suppresses");
        assert!(!tracks.needs_job(&hash(2)), "analyzing suppresses");
        assert!(!tracks.needs_job(&hash(3)), "failed suppresses");
        assert!(tracks.needs_job(&hash(4)), "queued wants a job");
        assert!(tracks.needs_job(&hash(9)), "unknown wants a job");
    }

    // Given a ready record.
    // When the analysis is cleared.
    // Then the record survives, queued for analysis again.
    #[test]
    fn clear_analysis_leaves_record_queued() {
        let mut tracks = Tracks::default();
        tracks.upsert(record(1, AnalysisState::Ready(analysis(138.0))));

        tracks.clear_analysis(&hash(1));

        let got = tracks.get(&hash(1)).expect("record survives");
        assert!(matches!(got.analysis, AnalysisState::Queued));
        assert_eq!(got.tags.title, "T1", "tags survive");
    }

    // Given a failed record among hydrated-incomplete hashes.
    // When the retry pass runs.
    // Then only the failed state resets to queued.
    #[test]
    fn retry_failed_resets_only_failed_states() {
        let mut tracks = Tracks::default();
        tracks.upsert(record(1, AnalysisState::Failed("boom".to_owned())));
        tracks.upsert(record(2, AnalysisState::Ready(analysis(138.0))));

        tracks.retry_failed(&[hash(1), hash(2)]);

        assert!(matches!(
            tracks.get(&hash(1)).map(|r| &r.analysis),
            Some(AnalysisState::Queued)
        ));
        assert!(tracks.is_ready(&hash(2)), "ready untouched");
    }

    // Given a ready record.
    // When a manual grid save refreshes the grid.
    // Then the cached BPM follows the edit.
    #[test]
    fn refresh_grid_updates_bpm_of_ready_record() {
        let mut tracks = Tracks::default();
        tracks.upsert(record(1, AnalysisState::Ready(analysis(138.0))));

        let edited = crate::grid::EditableGrid {
            grid_bpm: 141.0,
            anchor_seconds: 0.25,
            downbeat_phase: 1,
        };
        tracks.refresh_grid(&hash(1), &edited);

        let AnalysisState::Ready(a) = &tracks.get(&hash(1)).expect("record").analysis else {
            panic!("ready");
        };
        assert!((a.bpm - 141.0).abs() < f32::EPSILON);
        assert!((a.grid.grid_bpm - 141.0).abs() < f32::EPSILON);
    }

    // Given a ready record.
    // When a cue save refreshes the cues.
    // Then the cached cue positions follow the flush.
    #[test]
    fn refresh_cues_updates_cue_points_of_ready_record() {
        let mut tracks = Tracks::default();
        tracks.upsert(record(1, AnalysisState::Ready(analysis(138.0))));

        tracks.refresh_cues(&hash(1), &CuePoints::with_in(3, 44_100));

        let AnalysisState::Ready(a) = &tracks.get(&hash(1)).expect("record").analysis else {
            panic!("ready");
        };
        assert_eq!(a.cues.get(CueKind::In, 3), Some(44_100));
    }
}
