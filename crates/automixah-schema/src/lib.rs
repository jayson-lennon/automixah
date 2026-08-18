//! Standalone schema + migration crate for automixah's grid library.
//!
//! This is the **single source of truth** for the SQLite schema behind
//! `automixah-ui`'s manual beat-grid overrides. It is consumed at runtime
//! through a `daow::Pool::with_conn` closure on the user's
//! `library.sqlite` at startup.
//!
//! Living in its own leaf crate (no dependency on `automixah-ui`) keeps the
//! DDL addressable from any future consumer — the eventual CLI remake will
//! call the same [`run_migrations`] before reading grids.
//!
//! Only one migration exists today (v1: the `beat_grids` table). The runner
//! structure mirrors the schema-crate pattern used by jinn's session store
//! so additional versions slot in mechanically.

use error_stack::{Report, ResultExt as _};
use wherror::Error;

/// The highest migration version this runner knows how to apply.
///
/// `run_pending` skips `BEGIN` when the DB is already at this version, so an
/// up-to-date database pays no transaction cost on startup.
const LATEST_VERSION: i32 = 1;

/// Runs all pending schema migrations on the given connection.
///
/// Bootstraps the `_migrations` tracking table, reads the current version,
/// and applies every unapplied migration inside a single
/// `BEGIN IMMEDIATE … COMMIT` transaction. A failure before `COMMIT` rolls
/// the whole chain back to the last-applied version.
///
/// Safe to call on an empty database (bootstraps the tracking table) and
/// idempotent on a fully-migrated one (the version check short-circuits
/// before any `BEGIN`).
///
/// # Errors
///
/// Returns an error if the tracking table cannot be created, the version
/// cannot be read, or any migration fails (the transaction is rolled back
/// first).
pub fn run_migrations(conn: &mut rusqlite::Connection) -> Result<(), Report<SchemaMigrationError>> {
    bootstrap_tracking_table(conn)?;
    let current = current_version(conn)?;

    // No-op path: an up-to-date database pays no transaction cost.
    if current >= LATEST_VERSION {
        return Ok(());
    }

    // Raw BEGIN/COMMIT/ROLLBACK (not rusqlite's Transaction) because the
    // connection is a `&mut` borrowed under dao's `with_conn`. IMMEDIATE
    // acquires the write lock up front so the run can't fail mid-chain on
    // a lock upgrade.
    conn.execute_batch("BEGIN IMMEDIATE")
        .change_context(SchemaMigrationError)
        .attach("begin migration transaction")?;

    match apply_migration_chain(conn, current) {
        Ok(()) => {
            conn.execute_batch("COMMIT")
                .change_context(SchemaMigrationError)
                .attach("commit migration transaction")?;
            Ok(())
        }
        Err(report) => {
            // Best-effort rollback; the original migration error propagates.
            let _ = conn.execute_batch("ROLLBACK");
            Err(report)
        }
    }
}

/// Creates the `_migrations` tracking table if it does not exist.
///
/// Public so future consumers (e.g. a `build.rs` compile-time validation
/// DB) can bootstrap the same table the runtime runner reads.
///
/// # Errors
///
/// Returns an error if the DDL fails.
pub fn bootstrap_tracking_table(
    conn: &mut rusqlite::Connection,
) -> Result<(), Report<SchemaMigrationError>> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _migrations (\
         version INTEGER NOT NULL,\
         name TEXT NOT NULL,\
         applied_at TEXT NOT NULL DEFAULT (datetime('now')))",
    )
    .change_context(SchemaMigrationError)
    .attach("failed to create _migrations table")?;
    Ok(())
}

/// Reads the highest migration version from the tracking table.
///
/// Returns -1 if no migrations have been recorded (empty database).
fn current_version(conn: &mut rusqlite::Connection) -> Result<i32, Report<SchemaMigrationError>> {
    let version: Option<i32> = conn
        .query_row(
            "SELECT MAX(version) AS version FROM _migrations",
            [],
            |row| row.get(0),
        )
        .change_context(SchemaMigrationError)
        .attach("failed to query migration version")?;
    Ok(version.unwrap_or(-1))
}

/// Records a completed migration in the tracking table.
///
/// # Errors
///
/// Returns an error if the insert fails.
pub fn record_version(
    conn: &mut rusqlite::Connection,
    version: i32,
    name: &str,
) -> Result<(), Report<SchemaMigrationError>> {
    conn.execute(
        "INSERT INTO _migrations (version, name) VALUES (?, ?)",
        rusqlite::params![version, name],
    )
    .change_context(SchemaMigrationError)
    .attach(format!("failed to record migration v{version}"))?;
    Ok(())
}

/// Applies every migration above `current`, recording each version.
///
/// Runs inside the caller's open transaction; its `?` short-circuits on the
/// first failure, at which point `run_migrations` rolls back.
fn apply_migration_chain(
    conn: &mut rusqlite::Connection,
    current: i32,
) -> Result<(), Report<SchemaMigrationError>> {
    if current < 1 {
        migrate_v1(conn)?;
        record_version(conn, 1, "create_beat_grids")?;
    }
    Ok(())
}

/// v1: The `beat_grids` table — manual grid overrides keyed by content hash.
///
/// `track_hash` is the SHA-256 hex digest of the audio file's bytes (the
/// same key `automixah-cli` computes for `TrackHash`), so a grid survives
/// file renames and moves. `downbeat_phase` is the beat-in-bar (0..=3) of
/// the anchor.
fn migrate_v1(conn: &mut rusqlite::Connection) -> Result<(), Report<SchemaMigrationError>> {
    conn.execute_batch(
        "CREATE TABLE beat_grids (\
         track_hash TEXT PRIMARY KEY,\
         grid_bpm REAL NOT NULL,\
         anchor_seconds REAL NOT NULL,\
         downbeat_phase INTEGER NOT NULL CHECK (downbeat_phase BETWEEN 0 AND 3),\
         updated_at INTEGER NOT NULL)",
    )
    .change_context(SchemaMigrationError)
    .attach("failed to create beat_grids table")?;
    Ok(())
}

/// Error type for schema migration failures.
///
/// Carries no variants — the failure detail lives in the `error_stack::Report`
/// context attachments (which migration failed, what SQL).
#[derive(Debug, Error)]
#[error("schema migration error")]
pub struct SchemaMigrationError;

#[cfg(test)]
mod tests {
    use super::*;

    // Given a fresh in-memory database.
    // When migrations run.
    // Then they apply without error.
    #[test]
    fn fresh_database_migrates() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        run_migrations(&mut conn).expect("migrations apply");
    }

    // Given a fully-migrated database.
    // When migrations run a second time.
    // Then they no-op without duplicating version rows.
    #[test]
    fn migrations_are_idempotent() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        run_migrations(&mut conn).expect("first run");
        run_migrations(&mut conn).expect("second run");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _migrations", [], |row| row.get(0))
            .expect("count versions");
        assert_eq!(count, 1, "one version row per applied migration");
    }

    // Given a migrated database.
    // When the beat_grids schema is inspected.
    // Then it matches the v1 contract.
    #[test]
    fn v1_creates_beat_grids_table() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        run_migrations(&mut conn).expect("migrations apply");

        conn.execute(
            "INSERT INTO beat_grids (track_hash, grid_bpm, anchor_seconds, downbeat_phase, updated_at) \
             VALUES ('abc', 138.0, 0.42, 2, 0)",
            [],
        )
        .expect("insert row");
        let bpm: f64 = conn
            .query_row(
                "SELECT grid_bpm FROM beat_grids WHERE track_hash = 'abc'",
                [],
                |row| row.get(0),
            )
            .expect("read back");
        assert!((bpm - 138.0).abs() < 1e-9);
    }

    // Given a migrated database.
    // When a downbeat_phase outside 0..=3 is inserted.
    // Then the CHECK constraint rejects it.
    #[test]
    fn downbeat_phase_check_rejects_out_of_range() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        run_migrations(&mut conn).expect("migrations apply");

        let result = conn.execute(
            "INSERT INTO beat_grids (track_hash, grid_bpm, anchor_seconds, downbeat_phase, updated_at) \
             VALUES ('abc', 138.0, 0.42, 7, 0)",
            [],
        );
        assert!(result.is_err(), "CHECK constraint rejects phase 7");
    }

    // Given a migrated database.
    // When the same track_hash is inserted twice.
    // Then the PRIMARY KEY rejects the duplicate.
    #[test]
    fn track_hash_is_primary_key() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        run_migrations(&mut conn).expect("migrations apply");

        conn.execute(
            "INSERT INTO beat_grids (track_hash, grid_bpm, anchor_seconds, downbeat_phase, updated_at) \
             VALUES ('abc', 138.0, 0.42, 0, 0)",
            [],
        )
        .expect("first insert");
        let dup = conn.execute(
            "INSERT INTO beat_grids (track_hash, grid_bpm, anchor_seconds, downbeat_phase, updated_at) \
             VALUES ('abc', 140.0, 0.0, 1, 1)",
            [],
        );
        assert!(dup.is_err(), "duplicate hash rejected");
    }
}
