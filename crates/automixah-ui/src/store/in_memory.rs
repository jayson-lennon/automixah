//! In-memory [`GridStore`] backend for tests and headless consumers.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use error_stack::Report;
use parking_lot::Mutex;

use automixah_engine::timeline::types::{CuePoints, TrackHash};

use super::{CueStore, CueStoreError, GridOverride, GridStore, GridStoreError};

/// HashMap-backed store. Last write wins per hash.
#[derive(Debug, Default)]
pub struct InMemoryGridStore {
    grids: Mutex<HashMap<String, GridOverride>>,
}

/// HashMap-backed cue store. Replaces the full cue set per hash.
#[derive(Debug, Default)]
pub struct InMemoryCueStore {
    cues: Mutex<HashMap<String, CuePoints>>,
}

impl InMemoryGridStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// HashMap-backed cue store.
impl InMemoryCueStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl CueStore for InMemoryCueStore {
    async fn get(&self, hash: &TrackHash) -> Result<CuePoints, Report<CueStoreError>> {
        let cues = self.cues.lock();
        Ok(cues.get(&hash.0).copied().unwrap_or_default())
    }

    async fn put(&self, hash: &TrackHash, cues: &CuePoints) -> Result<(), Report<CueStoreError>> {
        self.cues.lock().insert(hash.0.clone(), *cues);
        Ok(())
    }

    async fn delete(&self, hash: &TrackHash) -> Result<(), Report<CueStoreError>> {
        self.cues.lock().remove(&hash.0);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "in-memory-cues"
    }
}

#[async_trait]
impl GridStore for InMemoryGridStore {
    async fn get(&self, hash: &TrackHash) -> Result<Option<GridOverride>, Report<GridStoreError>> {
        let grids = self.grids.lock();
        Ok(grids.get(&hash.0).cloned())
    }

    async fn put(
        &self,
        hash: &TrackHash,
        grid: &GridOverride,
    ) -> Result<(), Report<GridStoreError>> {
        let mut grids = self.grids.lock();
        grids.insert(hash.0.clone(), grid.clone());
        Ok(())
    }

    async fn delete(&self, hash: &TrackHash) -> Result<(), Report<GridStoreError>> {
        self.grids.lock().remove(&hash.0);
        Ok(())
    }

    async fn analyzed_hashes(&self) -> Result<HashSet<String>, Report<GridStoreError>> {
        let grids = self.grids.lock();
        // "Analyzed" mirrors the fast-path completeness contract:
        // a stored grid WITH a musical key.
        Ok(grids
            .iter()
            .filter(|(_, grid)| grid.key.is_some())
            .map(|(hash, _)| hash.clone())
            .collect())
    }

    fn name(&self) -> &'static str {
        "in-memory"
    }
}

/// Given an in-memory store.
/// When the same hash is written twice.
/// Then the last write wins.
#[tokio::test]
async fn last_write_wins() {
    use super::GridStore as _;

    let store = InMemoryGridStore::new();
    let hash = TrackHash("aa".to_owned());
    let first = GridOverride {
        grid_bpm: 120.0,
        anchor_seconds: 0.0,
        downbeat_phase: 0,
        updated_at: 1,
        key: None,
    };
    let second = GridOverride {
        grid_bpm: 140.0,
        anchor_seconds: 0.1,
        downbeat_phase: 3,
        updated_at: 2,
        key: None,
    };

    store.put(&hash, &first).await.expect("first write");
    store.put(&hash, &second).await.expect("second write");

    assert_eq!(store.get(&hash).await.expect("load"), Some(second));
}

/// Given two different hashes in an in-memory store.
/// When each is looked up.
/// Then each returns its own override (no cross-talk).
#[tokio::test]
async fn distinct_hashes_are_independent() {
    use super::GridStore as _;

    let store = InMemoryGridStore::new();
    let a = TrackHash("a".to_owned());
    let b = TrackHash("b".to_owned());
    let grid = GridOverride {
        grid_bpm: 130.0,
        anchor_seconds: 0.5,
        downbeat_phase: 1,
        updated_at: 3,
        key: None,
    };

    store.put(&a, &grid).await.expect("write");

    assert_eq!(store.get(&a).await.expect("load a"), Some(grid));
    assert_eq!(store.get(&b).await.expect("load b"), None);
}

/// Given a missing hash.
/// When looked up.
/// Then None comes back with no error (silent miss path).
#[tokio::test]
async fn missing_hash_is_none_not_error() {
    use super::GridStore as _;

    let store = InMemoryGridStore::new();
    let hash = TrackHash("ghost".to_owned());

    let result = store.get(&hash).await;
    let value = result.expect("lookup succeeds");
    assert!(value.is_none());
}

/// Given a store holding a keyed grid and a keyless (legacy) grid.
/// When the analyzed-hash set is listed.
/// Then only the keyed hash appears.
#[tokio::test]
async fn analyzed_hashes_list_only_keyed_grids() {
    use super::GridStore as _;

    let store = InMemoryGridStore::new();
    let analyzed = TrackHash("analyzed".to_owned());
    let legacy = TrackHash("legacy".to_owned());
    let keyed = GridOverride {
        grid_bpm: 138.0,
        anchor_seconds: 0.0,
        downbeat_phase: 0,
        updated_at: 1,
        key: Some(djcore::key::Key {
            root: 9,
            mode: djcore::key::KeyMode::Minor,
        }),
    };
    let keyless = GridOverride {
        key: None,
        ..keyed.clone()
    };
    store.put(&analyzed, &keyed).await.expect("save keyed");
    store.put(&legacy, &keyless).await.expect("save keyless");

    let listed = store.analyzed_hashes().await.expect("list");

    assert!(
        listed.contains("analyzed"),
        "a grid with a key counts as analyzed"
    );
    assert!(
        !listed.contains("legacy"),
        "a keyless grid predates key analysis and must not count"
    );
}

/// Given an in-memory cue store.
/// When a full cue set is saved and loaded.
/// Then every slot round-trips.
#[tokio::test]
async fn in_memory_cues_round_trip() {
    use super::CueStore as _;

    let store = InMemoryCueStore::new();
    let hash = TrackHash("cues".to_owned());
    let cues = CuePoints {
        ins: [Some(100), Some(200), None, None],
        outs: [None, None, Some(900), Some(950)],
    };

    store.put(&hash, &cues).await.expect("save");
    assert_eq!(store.get(&hash).await.expect("load"), cues);
}

/// Given a saved cue set.
/// When it is replaced with an empty set and then deleted.
/// Then loading returns an empty set each time.
#[tokio::test]
async fn in_memory_cues_clear_and_delete() {
    use super::CueStore as _;

    let store = InMemoryCueStore::new();
    let hash = TrackHash("cues".to_owned());
    let cues = CuePoints {
        outs: [Some(5), None, None, None],
        ..CuePoints::default()
    };
    store.put(&hash, &cues).await.expect("save");

    store
        .put(&hash, &CuePoints::default())
        .await
        .expect("clear");
    assert_eq!(store.get(&hash).await.expect("load"), CuePoints::default());

    store.delete(&hash).await.expect("delete");
    assert_eq!(store.get(&hash).await.expect("load"), CuePoints::default());
}

/// Given two hashes.
/// When one has cues and the other does not.
/// Then each returns only its own set.
#[tokio::test]
async fn in_memory_cues_are_per_hash() {
    use super::CueStore as _;

    let store = InMemoryCueStore::new();
    let a = TrackHash("a".to_owned());
    let b = TrackHash("b".to_owned());
    let cues = CuePoints {
        ins: [Some(42), None, None, None],
        ..CuePoints::default()
    };
    store.put(&a, &cues).await.expect("save");

    assert_eq!(store.get(&a).await.expect("load a"), cues);
    assert_eq!(store.get(&b).await.expect("load b"), CuePoints::default());
}
