//! Grid persistence: the [`GridStore`] trait plus its service wrapper.
//!
//! Manual grid alignments persist to the SQLite library (`library.sqlite`)
//! keyed by the track's content hash, so a grid survives renames and moves.
//! The trait exists so tests (and any future consumer) can run against an
//! in-memory backend instead of a file.

use std::sync::Arc;

use async_trait::async_trait;
use error_stack::{Report, ResultExt as _};
use wherror::Error;

use automixah_engine::timeline::types::TrackHash;

pub mod in_memory;
pub mod sqlite;

/// Error type for grid-store failures.
///
/// Carries no variants — the failure detail lives in the
/// `error_stack::Report` context attachments.
#[derive(Debug, Error)]
#[error("grid store error")]
pub struct GridStoreError;

/// A stored manual grid override.
///
/// The canonical subset of a [`djcore::analyzer::BeatGrid`] that a human
/// edits; beats/downbeats/bars are re-derived by projection at load time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridOverride {
    /// Constant-tempo BPM of the grid.
    pub grid_bpm: f32,
    /// Phase anchor: a beat time in `[0, bar)` seconds.
    pub anchor_seconds: f32,
    /// Beat-in-bar (0..=3) of the anchor.
    pub downbeat_phase: u8,
    /// Unix seconds of the last edit.
    pub updated_at: i64,
}

/// Persistence backend for manual grid overrides.
///
/// Implementations: [`sqlite::SqliteGridStore`] (production, daow pool over
/// `library.sqlite`) and [`in_memory::InMemoryGridStore`] (tests).
#[async_trait]
pub trait GridStore: Send + Sync {
    /// Returns the stored override for `hash`, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if the lookup fails.
    async fn get(&self, hash: &TrackHash) -> Result<Option<GridOverride>, Report<GridStoreError>>;

    /// Upserts the override for `hash`.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    async fn put(&self, hash: &TrackHash, grid: &GridOverride) -> Result<(), Report<GridStoreError>>;

    /// Backend name for debugging.
    fn name(&self) -> &'static str;
}

/// Cheap-clone service wrapper around a [`GridStore`] backend.
///
/// The `Services` container and the eframe app hold this, never the raw
/// trait object — swapping the backend (SQLite ↔ in-memory) happens at
/// assembly time only.
#[derive(Clone)]
pub struct GridStoreService {
    backend: Arc<dyn GridStore>,
}

impl std::fmt::Debug for GridStoreService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GridStoreService<{}>", self.backend.name())
    }
}

impl GridStoreService {
    /// Wraps a backend.
    #[must_use]
    pub fn new(backend: Arc<dyn GridStore>) -> Self {
        Self { backend }
    }

    /// Returns the stored override for `hash`, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend lookup fails.
    pub async fn get(
        &self,
        hash: &TrackHash,
    ) -> Result<Option<GridOverride>, Report<GridStoreError>> {
        self.backend.get(hash).await
    }

    /// Upserts the override for `hash`.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend write fails.
    pub async fn put(
        &self,
        hash: &TrackHash,
        grid: &GridOverride,
    ) -> Result<(), Report<GridStoreError>> {
        self.backend.put(hash, grid).await
    }

    /// Backend name for debugging.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.backend.name()
    }
}

/// Runs schema migrations on a daow pool at startup.
///
/// Adapts `automixah_schema::run_migrations` (synchronous, raw connection)
/// to the pool's async `with_conn`. Migrations run before any store call.
///
/// # Errors
///
/// Returns an error if the connection cannot be acquired or a migration
/// fails.
pub async fn run_migrations(pool: &daow::Pool) -> Result<(), Report<GridStoreError>> {
    let outcome = pool
        .with_conn(|conn| {
            automixah_schema::run_migrations(conn)
                .map(Ok::<_, Report<GridStoreError>>)
                .or_else(|report| Ok(Err(report.change_context(GridStoreError))))
        })
        .await
        .change_context(GridStoreError)
        .attach("failed to run grid library migrations")?;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given a grid store service wrapping an in-memory backend.
    // When an override is saved and loaded.
    // Then the round-trip preserves every field.
    #[tokio::test]
    async fn service_round_trips_override() {
        let service = GridStoreService::new(Arc::new(in_memory::InMemoryGridStore::new()));
        let hash = TrackHash("deadbeef".to_owned());
        let grid = GridOverride {
            grid_bpm: 138.0,
            anchor_seconds: 0.42,
            downbeat_phase: 2,
            updated_at: 1_700_000_000,
        };

        service.put(&hash, &grid).await.expect("save");
        let loaded = service.get(&hash).await.expect("load");

        assert_eq!(loaded, Some(grid));
    }
}
