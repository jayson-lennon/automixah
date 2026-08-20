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

use automixah_engine::timeline::types::{CuePoints, TrackHash};

pub mod in_memory;
pub mod sqlite;

/// Error type for grid-store failures.
///
/// Carries no variants — the failure detail lives in the
/// `error_stack::Report` context attachments.
#[derive(Debug, Error)]
#[error(debug)]
pub struct GridStoreError;

/// A stored manual grid override.
///
/// The canonical subset of a [`djcore::analyzer::BeatGrid`] that a human
/// edits; beats/downbeats/bars are re-derived by projection at load time.
#[derive(Debug, Clone, PartialEq)]
pub struct GridOverride {
    /// Constant-tempo BPM of the grid.
    pub grid_bpm: f32,
    /// Phase anchor: a beat time in `[0, bar)` seconds.
    pub anchor_seconds: f32,
    /// Beat-in-bar (0..=3) of the anchor.
    pub downbeat_phase: u8,
    /// Unix seconds of the last edit.
    pub updated_at: i64,
    /// Detected musical key. `None` means unknown — on write it leaves any
    /// stored key untouched (COALESCE upsert), on read it means the row
    /// predates key analysis.
    pub key: Option<djcore::key::Key>,
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
    async fn put(
        &self,
        hash: &TrackHash,
        grid: &GridOverride,
    ) -> Result<(), Report<GridStoreError>>;

    /// Deletes the stored override for `hash`, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails.
    async fn delete(&self, hash: &TrackHash) -> Result<(), Report<GridStoreError>>;

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

    /// Deletes the override for `hash`.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails.
    pub async fn delete(&self, hash: &TrackHash) -> Result<(), Report<GridStoreError>> {
        self.backend.delete(hash).await
    }

    /// Backend name for debugging.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.backend.name()
    }
}

/// Error type for cue-store failures.
#[derive(Debug, Error)]
#[error(debug)]
pub struct CueStoreError;

/// Persistence backend for source-frame cue points.
#[async_trait]
pub trait CueStore: Send + Sync {
    /// Loads cue slots for `hash`; an absent row is an empty cue set.
    ///
    /// # Errors
    ///
    /// Returns an error if the lookup fails.
    async fn get(&self, hash: &TrackHash) -> Result<CuePoints, Report<CueStoreError>>;

    /// Replaces all cue slots for `hash`.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    async fn put(&self, hash: &TrackHash, cues: &CuePoints) -> Result<(), Report<CueStoreError>>;

    /// Deletes all cue slots for `hash`.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails.
    async fn delete(&self, hash: &TrackHash) -> Result<(), Report<CueStoreError>>;

    /// Backend name for debugging.
    fn name(&self) -> &'static str;
}

/// Cheap-clone service wrapper around a [`CueStore`] backend.
#[derive(Clone)]
pub struct CueStoreService {
    backend: Arc<dyn CueStore>,
}

impl CueStoreService {
    /// Wraps a cue persistence backend.
    #[must_use]
    pub fn new(backend: Arc<dyn CueStore>) -> Self {
        Self { backend }
    }

    /// Loads all cue slots for `hash`, if any have been persisted.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend lookup fails.
    pub async fn get(&self, hash: &TrackHash) -> Result<CuePoints, Report<CueStoreError>> {
        self.backend.get(hash).await
    }

    /// Replaces all persisted cue slots for `hash`.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend write fails.
    pub async fn put(
        &self,
        hash: &TrackHash,
        cues: &CuePoints,
    ) -> Result<(), Report<CueStoreError>> {
        self.backend.put(hash, cues).await
    }

    /// Deletes all persisted cues for `hash`.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend delete fails.
    pub async fn delete(&self, hash: &TrackHash) -> Result<(), Report<CueStoreError>> {
        self.backend.delete(hash).await
    }

    /// Backend name for debugging.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.backend.name()
    }
}

impl std::fmt::Debug for CueStoreService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CueStoreService<{}>", self.backend.name())
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
    pool.with_conn(|conn| {
        automixah_schema::run_migrations(conn)
            .map(Ok::<_, Report<GridStoreError>>)
            .or_else(|report| Ok(Err(report.change_context(GridStoreError))))
    })
    .await
    .change_context(GridStoreError)
    .attach("failed to run grid library migrations")?
}

/// Test double wrapping a backend with put/get counters.
pub struct CountingStore {
    backend: Arc<dyn GridStore>,
    puts: std::sync::atomic::AtomicUsize,
    gets: std::sync::atomic::AtomicUsize,
}

impl CountingStore {
    pub fn new(backend: Arc<dyn GridStore>) -> Self {
        Self {
            backend,
            puts: std::sync::atomic::AtomicUsize::new(0),
            gets: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn puts(&self) -> usize {
        self.puts.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn gets(&self) -> usize {
        self.gets.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl GridStore for CountingStore {
    async fn get(&self, hash: &TrackHash) -> Result<Option<GridOverride>, Report<GridStoreError>> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.backend.get(hash).await
    }

    async fn put(
        &self,
        hash: &TrackHash,
        grid: &GridOverride,
    ) -> Result<(), Report<GridStoreError>> {
        self.puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.backend.put(hash, grid).await
    }

    async fn delete(&self, hash: &TrackHash) -> Result<(), Report<GridStoreError>> {
        self.backend.delete(hash).await
    }

    fn name(&self) -> &'static str {
        "counting"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn service_round_trips_cues() {
        // Given a cue store service wrapping an in-memory backend.
        let service = CueStoreService::new(Arc::new(in_memory::InMemoryCueStore::new()));
        let hash = TrackHash("cue-service".to_owned());
        let cues = CuePoints {
            ins: [Some(1), None, Some(3), None],
            outs: [None, Some(8), None, Some(13)],
        };

        // When the complete cue set is saved and loaded through the service.
        service.put(&hash, &cues).await.expect("save cues");
        let loaded = service.get(&hash).await.expect("load cues");

        // Then all slots are preserved at the trait-backed boundary.
        assert_eq!(loaded, cues);
        assert_eq!(service.name(), "in-memory-cues");
    }
    #[tokio::test]
    async fn service_round_trips_override() {
        // Given a grid store service wrapping an in-memory backend.
        let service = GridStoreService::new(Arc::new(in_memory::InMemoryGridStore::new()));
        let hash = TrackHash("deadbeef".to_owned());
        let grid = GridOverride {
            grid_bpm: 138.0,
            anchor_seconds: 0.42,
            downbeat_phase: 2,
            updated_at: 1_700_000_000,
            key: None,
        };

        // When an override is saved and loaded.
        service.put(&hash, &grid).await.expect("save");
        let loaded = service.get(&hash).await.expect("load");

        // Then the round-trip preserves every field.
        assert_eq!(loaded, Some(grid));
    }
}
