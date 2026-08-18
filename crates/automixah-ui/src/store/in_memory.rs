//! In-memory [`GridStore`] backend for tests and headless consumers.

use std::collections::HashMap;

use async_trait::async_trait;
use error_stack::Report;
use parking_lot::Mutex;

use automixah_engine::timeline::types::TrackHash;

use super::{GridOverride, GridStore, GridStoreError};

/// HashMap-backed store. Last write wins per hash.
#[derive(Debug, Default)]
pub struct InMemoryGridStore {
    grids: Mutex<HashMap<String, GridOverride>>,
}

impl InMemoryGridStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl GridStore for InMemoryGridStore {
    async fn get(&self, hash: &TrackHash) -> Result<Option<GridOverride>, Report<GridStoreError>> {
        let grids = self.grids.lock();
        Ok(grids.get(&hash.0).copied())
    }

    async fn put(&self, hash: &TrackHash, grid: &GridOverride) -> Result<(), Report<GridStoreError>> {
        let mut grids = self.grids.lock();
        grids.insert(hash.0.clone(), *grid);
        Ok(())
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
    };
    let second = GridOverride {
        grid_bpm: 140.0,
        anchor_seconds: 0.1,
        downbeat_phase: 3,
        updated_at: 2,
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
