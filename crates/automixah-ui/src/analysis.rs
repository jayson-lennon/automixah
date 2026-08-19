//! UI-owned analysis cache: content hash → detected beat grid.
//!
//! Keeps analysis a once-per-content operation within a session: repeated
//! opens of the same file skip the analyzing stage entirely. The cache is
//! process-local (never persisted); the SQLite library remains the only
//! persistent grid store.
//!
//! Single-threaded by design: owned by the UI state and mutated only when
//! applying events (the load task receives the cached grid as a message
//! input and reports detected grids back through the bus).

use std::collections::HashMap;

use automixah_engine::timeline::types::TrackHash;
use djcore::analyzer::BeatGrid;

/// Session-local analysis cache (data service: plain data, UI-owned).
#[derive(Debug, Default)]
pub struct AnalysisCache {
    grids: HashMap<TrackHash, BeatGrid>,
}

impl AnalysisCache {
    /// Returns the cached grid for `hash`, if analyzed this session.
    #[must_use]
    pub fn get(&self, hash: &TrackHash) -> Option<&BeatGrid> {
        self.grids.get(hash)
    }

    /// Caches the detected `grid` for `hash`.
    pub fn put(&mut self, hash: TrackHash, grid: BeatGrid) {
        self.grids.insert(hash, grid);
    }

    /// Drops the cached analysis for `hash` (re-analyze path).
    pub fn invalidate(&mut self, hash: &TrackHash) {
        self.grids.remove(hash);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid_at(bpm: f32) -> BeatGrid {
        BeatGrid {
            grid_bpm: bpm,
            ..BeatGrid::default()
        }
    }

    // Given an empty cache.
    // When looking up any hash.
    // Then nothing is found.
    #[test]
    fn missing_hash_is_none() {
        let cache = AnalysisCache::default();
        assert!(cache.get(&TrackHash("nope".to_owned())).is_none());
    }

    // Given a cached grid.
    // When looking it up.
    // Then the same grid comes back.
    #[test]
    fn put_then_get_round_trips() {
        let mut cache = AnalysisCache::default();
        let hash = TrackHash("deadbeef".to_owned());
        cache.put(hash.clone(), grid_at(138.0));
        assert_eq!(cache.get(&hash).map(|g| g.grid_bpm), Some(138.0));
    }

    // Given a cached grid.
    // When invalidated.
    // Then the lookup misses.
    #[test]
    fn invalidate_removes_entry() {
        let mut cache = AnalysisCache::default();
        let hash = TrackHash("deadbeef".to_owned());
        cache.put(hash.clone(), grid_at(140.0));
        cache.invalidate(&hash);
        assert!(cache.get(&hash).is_none());
    }
}
