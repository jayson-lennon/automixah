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
//! Versions today: v1 (`beat_grids`), v2 (musical-key columns on
//! `beat_grids`), v3 (playlist tables), v4 (`cue_points`), and v5 (library
//! index tables). The runner structure mirrors the schema-crate pattern used by jinn's session store so additional versions slot in mechanically.

use error_stack::{Report, ResultExt as _};
use wherror::Error;

/// The highest migration version this runner knows how to apply.
///
/// `run_pending` skips `BEGIN` when the DB is already at this version, so an
/// up-to-date database pays no transaction cost on startup.
const LATEST_VERSION: i32 = 5;

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
    if current < 2 {
        migrate_v2(conn)?;
        record_version(conn, 2, "add_beat_grids_key_columns")?;
    }
    if current < 3 {
        migrate_v3(conn)?;
        record_version(conn, 3, "create_playlist_tables")?;
    }
    if current < 4 {
        migrate_v4(conn)?;
        record_version(conn, 4, "create_cue_points")?;
    }
    if current < 5 {
        migrate_v5(conn)?;
        record_version(conn, 5, "create_library_index")?;
    }
    Ok(())
}

/// v1: The `beat_grids` table — manual grid overrides keyed by content hash.
///
/// `track_hash` is the SHA-256 hex digest of the audio file's bytes
/// (the content hash every subsystem addresses tracks by), so a grid
/// survives file renames and moves. `downbeat_phase` is the beat-in-bar
/// (0..=3) of the anchor.
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

/// v2: Musical-key columns on `beat_grids`.
///
/// `key_root` (0..=11, 0=C) and `key_mode` (0=major, 1=minor) are both
/// NULL on legacy rows — a grid stored before this version simply hasn't
/// been key-analyzed yet, and consumers treat NULL as "unknown". Ranges
/// are validated in Rust on read/write; SQLite `ALTER TABLE` cannot add
/// CHECK constraints to existing tables.
fn migrate_v2(conn: &mut rusqlite::Connection) -> Result<(), Report<SchemaMigrationError>> {
    conn.execute_batch(
        "ALTER TABLE beat_grids ADD COLUMN key_root INTEGER;\
         ALTER TABLE beat_grids ADD COLUMN key_mode INTEGER;",
    )
    .change_context(SchemaMigrationError)
    .attach("failed to add key columns to beat_grids")?;
    Ok(())
}

/// v3: Playlist tables — `tracks` (tags keyed by content hash),
/// `playlists`, and the ordered `playlist_tracks` join.
///
/// Referential ordering (tracks row before playlist_tracks row) is
/// enforced in store code, not by FK pragmas — the daow pool does not
/// promise `foreign_keys` is enabled.
fn migrate_v3(conn: &mut rusqlite::Connection) -> Result<(), Report<SchemaMigrationError>> {
    conn.execute_batch(
        "CREATE TABLE tracks (\
         track_hash TEXT PRIMARY KEY,\
         title TEXT NOT NULL,\
         artist TEXT NOT NULL,\
         duration_seconds REAL,\
         updated_at INTEGER NOT NULL);\
         CREATE TABLE playlists (\
         id INTEGER PRIMARY KEY AUTOINCREMENT,\
         name TEXT NOT NULL UNIQUE,\
         created_at INTEGER NOT NULL);\
         CREATE TABLE playlist_tracks (\
         playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,\
         position INTEGER NOT NULL,\
         track_hash TEXT NOT NULL REFERENCES tracks(track_hash),\
         added_path TEXT NOT NULL,\
         PRIMARY KEY (playlist_id, position),\
         UNIQUE (playlist_id, track_hash));",
    )
    .change_context(SchemaMigrationError)
    .attach("failed to create playlist tables")?;
    Ok(())
}

/// v4: Four numbered in-cue and four numbered out-cue slots per track.
///
/// Cue positions are source frames and are kept separate from beat grids so
/// forced grid re-analysis does not delete user-authored source positions.
fn migrate_v4(conn: &mut rusqlite::Connection) -> Result<(), Report<SchemaMigrationError>> {
    conn.execute_batch(
        "CREATE TABLE cue_points (\
         track_hash TEXT NOT NULL,\
         kind TEXT NOT NULL CHECK (kind IN ('in', 'out')),\
         slot INTEGER NOT NULL CHECK (slot BETWEEN 0 AND 3),\
         position_frames INTEGER NOT NULL CHECK (position_frames >= 0),\
         updated_at INTEGER NOT NULL,\
         PRIMARY KEY (track_hash, kind, slot));",
    )
    .change_context(SchemaMigrationError)
    .attach("failed to create cue_points table")?;
    Ok(())
}

/// v5: Library index — scanned root directories and their indexed audio
/// files.
///
/// `library_files` rows are keyed by `(root_id, rel_path)`; the same
/// content hash legitimately appears under multiple roots or paths, so
/// there is no UNIQUE on `track_hash`. Referential behavior (children die
/// with the root) is enforced in store code, not FK pragmas — the pool
/// does not promise `foreign_keys` is enabled.
fn migrate_v5(conn: &mut rusqlite::Connection) -> Result<(), Report<SchemaMigrationError>> {
    conn.execute_batch(
        "CREATE TABLE library_roots (\
         id INTEGER PRIMARY KEY AUTOINCREMENT,\
         path TEXT NOT NULL UNIQUE,\
         added_at INTEGER NOT NULL);\
         CREATE TABLE library_files (\
         root_id INTEGER NOT NULL,\
         rel_path TEXT NOT NULL,\
         track_hash TEXT NOT NULL,\
         title TEXT NOT NULL,\
         artist TEXT NOT NULL,\
         duration_seconds REAL,\
         mtime_secs INTEGER NOT NULL,\
         size_bytes INTEGER NOT NULL,\
         PRIMARY KEY (root_id, rel_path));",
    )
    .change_context(SchemaMigrationError)
    .attach("failed to create library index tables")?;
    Ok(())
}

/// Error type for schema migration failures.
///
/// Carries no variants — the failure detail lives in the `error_stack::Report`
/// context attachments (which migration failed, what SQL).
#[derive(Debug, Error)]
#[error(debug)]
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
        assert_eq!(count, 5, "one version row per applied migration");
    }

    // Given a fresh in-memory database.
    // When migrations run.
    // Then the library index tables exist and accept rows.
    #[test]
    fn v5_creates_library_index_tables() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        run_migrations(&mut conn).expect("migrations apply");

        conn.execute(
            "INSERT INTO library_roots (path, added_at) VALUES ('/music', 1)",
            [],
        )
        .expect("insert root");
        conn.execute(
            "INSERT INTO library_files (root_id, rel_path, track_hash, title, artist, \
             duration_seconds, mtime_secs, size_bytes) \
             VALUES (1, 'a/one.flac', 'h1', 'One', 'Artist', 61.0, 100, 2048)",
            [],
        )
        .expect("insert file");
        let title: String = conn
            .query_row(
                "SELECT title FROM library_files WHERE root_id = 1 AND rel_path = 'a/one.flac'",
                [],
                |row| row.get(0),
            )
            .expect("read back");
        assert_eq!(title, "One");
    }

    // Given a database migrated only to v4 with a stored grid row.
    // When pending migrations run.
    // Then the legacy row survives and the library tables exist.
    #[test]
    fn legacy_v4_row_survives_to_v5() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        run_migrations(&mut conn).expect("full migrate");
        // Rewind to v4 state: drop the v5 artifacts and its version row.
        conn.execute_batch(
            "DROP TABLE library_files;\
             DROP TABLE library_roots;\
             DELETE FROM _migrations WHERE version = 5;",
        )
        .expect("rewind to v4");
        conn.execute(
            "INSERT INTO beat_grids (track_hash, grid_bpm, anchor_seconds, downbeat_phase, \
             key_root, key_mode, updated_at) VALUES ('legacy', 128.0, 0.5, 2, 9, 1, 1)",
            [],
        )
        .expect("seed v4 row");
        assert_eq!(current_version(&mut conn).expect("version"), 4);

        // When the pending v5 migration runs.
        run_migrations(&mut conn).expect("migrate v4 to v5");

        let bpm: f64 = conn
            .query_row(
                "SELECT grid_bpm FROM beat_grids WHERE track_hash = 'legacy'",
                [],
                |row| row.get(0),
            )
            .expect("legacy row survives");
        assert!((bpm - 128.0).abs() < 1e-9);
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                 AND name IN ('library_roots', 'library_files')",
                [],
                |row| row.get(0),
            )
            .expect("count library tables");
        assert_eq!(tables, 2);
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

    // Given a database migrated only to v1 with a stored grid row.
    // When pending migrations run.
    // Then the legacy row survives with NULL key columns and the
    // playlist tables exist.
    #[test]
    fn legacy_v1_row_survives_to_v3_with_null_key() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        // Hand-build a v1 database (migrate v1 only, no version rows).
        bootstrap_tracking_table(&mut conn).expect("bootstrap");
        conn.execute_batch(
            "CREATE TABLE beat_grids (\
             track_hash TEXT PRIMARY KEY,\
             grid_bpm REAL NOT NULL,\
             anchor_seconds REAL NOT NULL,\
             downbeat_phase INTEGER NOT NULL CHECK (downbeat_phase BETWEEN 0 AND 3),\
             updated_at INTEGER NOT NULL);\
             INSERT INTO beat_grids VALUES ('legacy', 128.0, 0.5, 2, 1);\
             INSERT INTO _migrations (version, name) VALUES (1, 'create_beat_grids');",
        )
        .expect("seed v1");

        run_migrations(&mut conn).expect("migrate to v3");

        let (root, mode): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT key_root, key_mode FROM beat_grids WHERE track_hash = 'legacy'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read legacy row");
        assert_eq!((root, mode), (None, None), "legacy row keeps NULL key");

        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('tracks', 'playlists', 'playlist_tracks', 'cue_points')",
                [],
                |row| row.get(0),
            )
            .expect("count tables");
        assert_eq!(tables, 4, "v3 and v4 tables exist");
    }

    // Given a fresh database.
    // When migrations run and rows are inserted into the v3 tables.
    // Then the playlist schema contract holds (UNIQUE duplicate rejected).
    #[test]
    fn v3_playlist_tables_reject_duplicate_hash() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        run_migrations(&mut conn).expect("migrations apply");

        conn.execute_batch(
            "INSERT INTO tracks VALUES ('h', 'T', 'A', NULL, 0);\
             INSERT INTO playlists (name, created_at) VALUES ('p', 0);\
             INSERT INTO playlist_tracks VALUES (1, 0, 'h', '/x');",
        )
        .expect("seed playlist");

        let dup = conn.execute_batch("INSERT INTO playlist_tracks VALUES (1, 1, 'h', '/y');");
        assert!(dup.is_err(), "UNIQUE(playlist_id, track_hash) rejects");
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

    #[test]
    fn cue_points_schema_rejects_invalid_kind_and_slot() {
        // Given a fully migrated database.
        let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        run_migrations(&mut conn).expect("migrations apply");

        // When invalid cue rows are inserted.
        let bad_kind = conn.execute(
            "INSERT INTO cue_points (track_hash, kind, slot, position_frames, updated_at) VALUES ('h', 'middle', 0, 1, 0)",
            [],
        );
        let bad_slot = conn.execute(
            "INSERT INTO cue_points (track_hash, kind, slot, position_frames, updated_at) VALUES ('h', 'in', 4, 1, 0)",
            [],
        );

        // Then both rows are rejected by the schema checks.
        assert!(bad_kind.is_err(), "kind check rejects unknown kinds");
        assert!(bad_slot.is_err(), "slot check rejects slot 4");
    }

    #[test]
    fn cue_points_schema_preserves_all_slots_and_kinds() {
        // Given a fully migrated database.
        let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        run_migrations(&mut conn).expect("migrations apply");

        // When all eight numbered slots are inserted.
        for kind in ["in", "out"] {
            for slot in 0..4 {
                conn.execute(
                    "INSERT INTO cue_points (track_hash, kind, slot, position_frames, updated_at) VALUES (?, ?, ?, ?, 0)",
                    rusqlite::params!["h", kind, slot, slot + 1],
                )
                .expect("insert cue slot");
            }
        }

        // Then both cue kinds retain all four slots.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM cue_points WHERE track_hash = 'h'",
                [],
                |row| row.get(0),
            )
            .expect("count cue slots");
        assert_eq!(count, 8);
    }

    // Given a fresh database.
    // When migrations run.
    // Then the cue table exists alongside the earlier schema.
    #[test]
    fn v4_creates_cue_points_table() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        run_migrations(&mut conn).expect("migrations apply");

        let table: String = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'cue_points'",
                [],
                |row| row.get(0),
            )
            .expect("cue_points table");
        assert_eq!(table, "cue_points");
    }

    // Given the existing v3 migration test above.
    // When v4 completes.
    // Then the legacy playlist and grid data remains intact.
    #[test]
    fn v4_migration_from_legacy_v3_preserves_rows() {
        // Given a database that has completed v3 with existing grid and
        // playlist rows but does not yet have the cue-point table.
        let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        bootstrap_tracking_table(&mut conn).expect("bootstrap");
        conn.execute_batch(
            "CREATE TABLE beat_grids (\
             track_hash TEXT PRIMARY KEY,\
             grid_bpm REAL NOT NULL,\
             anchor_seconds REAL NOT NULL,\
             downbeat_phase INTEGER NOT NULL CHECK (downbeat_phase BETWEEN 0 AND 3),\
             updated_at INTEGER NOT NULL,\
             key_root INTEGER,\
             key_mode INTEGER);\
             CREATE TABLE tracks (\
             track_hash TEXT PRIMARY KEY,\
             title TEXT NOT NULL,\
             artist TEXT NOT NULL,\
             duration_seconds REAL,\
             updated_at INTEGER NOT NULL);\
             CREATE TABLE playlists (\
             id INTEGER PRIMARY KEY AUTOINCREMENT,\
             name TEXT NOT NULL UNIQUE,\
             created_at INTEGER NOT NULL);\
             CREATE TABLE playlist_tracks (\
             playlist_id INTEGER NOT NULL,\
             position INTEGER NOT NULL,\
             track_hash TEXT NOT NULL,\
             added_path TEXT NOT NULL,\
             PRIMARY KEY (playlist_id, position),\
             UNIQUE (playlist_id, track_hash));\
             INSERT INTO beat_grids VALUES ('legacy-grid', 128.0, 0.5, 2, 7, 9, 1);\
             INSERT INTO tracks VALUES ('legacy-track', 'Title', 'Artist', 12.0, 8);\
             INSERT INTO playlists VALUES (1, 'Legacy playlist', 9);\
             INSERT INTO playlist_tracks VALUES (1, 0, 'legacy-track', '/music/legacy.ogg');\
             INSERT INTO _migrations (version, name) VALUES (1, 'create_beat_grids');\
             INSERT INTO _migrations (version, name) VALUES (2, 'add_beat_grids_key_columns');\
             INSERT INTO _migrations (version, name) VALUES (3, 'create_playlist_tables');",
        )
        .expect("seed v3");

        // When the pending v4 migration runs.
        run_migrations(&mut conn).expect("migrate v3 to v4");

        // Then the existing v3 data remains intact and cue storage is added.
        let grid: (f64, i64, i64) = conn
            .query_row(
                "SELECT grid_bpm, key_root, key_mode FROM beat_grids WHERE track_hash = 'legacy-grid'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("legacy grid");
        assert_eq!(grid, (128.0, 9, 1));
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT title FROM tracks WHERE track_hash = 'legacy-track'",
                [],
                |row| row.get(0),
            )
            .expect("legacy track"),
            "Title"
        );
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT added_path FROM playlist_tracks WHERE playlist_id = 1 AND position = 0",
                [],
                |row| row.get(0),
            )
            .expect("legacy playlist row"),
            "/music/legacy.ogg"
        );
        assert!(
            conn.query_row::<String, _, _>(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'cue_points'",
                [],
                |row| row.get(0),
            )
            .is_ok()
        );
        assert_eq!(current_version(&mut conn).expect("version"), 5);
    }

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
