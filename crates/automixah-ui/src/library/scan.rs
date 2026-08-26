//! Library scanning: refresh the persisted index from disk.
//!
//! One scan at a time, spawned as a plain async task on the services
//! runtime (file reads and SHA-256 run inline — nothing here outgrows
//! the load pipeline's style). The walk stats every supported-audio
//! file; unchanged files (same mtime + size) are skipped without a
//! read, changed or new files are read once for hash, tags, and
//! duration. Indexed-but-unseen files are pruned (vanished or moved),
//! and a known hash found at a new location refreshes the add-time
//! paths of every playlist row referencing it. Everything reports
//! through the bus like every other background task.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;

use crate::bus::Event;
use crate::library::store::{IndexedFile, LibraryEntry, LibraryStoreService};
use crate::services::Services;
use crate::track::identity;
use automixah_engine::timeline::types::TrackHash;

/// Terminal outcome of one scan, for tests and the done event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanOutcome {
    /// Files newly indexed.
    pub added: usize,
    /// Files re-read (changed on disk).
    pub updated: usize,
    /// Files removed from the index (vanished or moved).
    pub pruned: usize,
}

/// Spawns a full scan of every library root; the task reports progress
/// and the terminal outcome through the bus. Single-flight: while a
/// scan runs, the request is recorded and the finishing scan starts a
/// follow-up — roots are snapshotted at scan start, so a root added
/// mid-scan would otherwise stay unindexed (the record's "one scan at
/// a time" rule, enforced at the source instead of at guarded call
/// sites).
pub fn spawn_scan(services: &Services, tx: Sender<Event>) {
    let Some(guard) = services.scan_latch.try_acquire() else {
        services.scan_latch.request_rerun();
        return;
    };
    let services = services.clone();
    let handle = services.runtime.handle().clone();
    let progress = ProgressMonitor::new(tx.clone());
    handle.spawn({
        async move {
            let _guard = guard; // holds the latch until the task ends
            let _ = tx.send(Event::LibraryScanStarted);
            match scan(&services, progress).await {
                Ok(outcome) => {
                    let _ = tx.send(Event::LibraryScanDone {
                        added: outcome.added,
                        updated: outcome.updated,
                        pruned: outcome.pruned,
                    });
                    // The committed index replaces the frontend state in one
                    // event; the applier stays a dumb state-replacer.
                    send_loaded(&services, &tx).await;
                }
                Err(message) => {
                    let _ = tx.send(Event::LibraryScanFailed { message });
                }
            }
            // A scan requested while this one ran: run it now that the
            // latch is ours again (guard drops first inside the task).
            if services.scan_latch.take_rerun() {
                drop(_guard);
                spawn_scan(&services, tx);
            }
        }
    });
}

/// Spawns the startup hydration: read the persisted index, no scan.
pub fn spawn_load(services: &Services, tx: std::sync::mpsc::Sender<Event>) {
    let services = services.clone();
    let handle = services.runtime.handle().clone();
    handle.spawn(async move {
        send_loaded(&services, &tx).await;
    });
}

/// Reads roots + entries and sends one `LibraryLoaded`; failures land
/// as `LibraryScanFailed` (there is no separate hydration error dialect
/// worth its own event — the status line shows the message either way).
async fn send_loaded(services: &Services, tx: &std::sync::mpsc::Sender<Event>) {
    match load_library(&services.library_store).await {
        Ok((roots, entries)) => {
            // The analyzed set rides along for backfill enrollment; a
            // failed listing degrades to an empty set (over-enrollment
            // self-corrects via re-analysis, a stalled hydration does
            // not).
            let analyzed = services
                .grid_store
                .analyzed_hashes()
                .await
                .map(|hexes| hexes.into_iter().map(TrackHash).collect())
                .unwrap_or_default();
            let _ = tx.send(Event::LibraryLoaded {
                roots,
                entries,
                analyzed,
            });
        }
        Err(message) => {
            let _ = tx.send(Event::LibraryScanFailed { message });
        }
    }
}

/// Reads the full persisted library.
///
/// # Errors
///
/// Returns a rendered message when either store query fails.
pub async fn load_library(
    store: &LibraryStoreService,
) -> Result<
    (
        Vec<crate::library::store::LibraryRoot>,
        Vec<crate::library::store::LibraryEntry>,
    ),
    String,
> {
    let roots = store.list_roots().await.map_err(report_message)?;
    let entries = store.list_entries().await.map_err(report_message)?;
    Ok((roots, entries))
}

/// How many walked files share one commit boundary: read files accumulate
/// into a batch of at most this size, so thousands of files never become
/// thousands of transactions.
const BATCH_STEP: usize = 100;

/// One discovered file, handed from the walker to the classifier.
struct Found {
    root_id: i64,
    root_path: PathBuf,
    rel: PathBuf,
    file: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ProgressMonitor {
    processed: Arc<AtomicUsize>,
    seen: Arc<AtomicUsize>,
    tx: Sender<Event>,
}

impl ProgressMonitor {
    pub fn new(tx: Sender<Event>) -> Self {
        Self {
            processed: Arc::new(AtomicUsize::new(0)),
            seen: Arc::new(AtomicUsize::new(0)),
            tx,
        }
    }

    pub fn add_processed(&self, amount: usize) {
        self.processed.fetch_add(amount, Ordering::Relaxed);
        let (processed, seen) = self.get();
        let _ = self.tx.send(Event::LibraryScanProgress {
            files_done: processed,
            files_seen: seen,
        });
    }

    pub fn add_seen(&self, amount: usize) {
        self.seen.fetch_add(amount, Ordering::Relaxed);
    }

    pub fn reset(&self) {
        self.processed.store(0, Ordering::Relaxed);
        self.seen.store(0, Ordering::Relaxed);
    }

    pub fn get(&self) -> (usize, usize) {
        (
            self.processed.load(Ordering::Relaxed),
            self.seen.load(Ordering::Relaxed),
        )
    }
}

/// Runs one full scan: a blocking walker task streams discovered files
/// over a channel to the classifier loop, which stats/hashes each one on
/// the blocking pool, commits in batches, and reports per-file progress.
///
/// # Errors
///
/// Returns a rendered message on the first store failure. Per-file
/// read/stat failures skip the file (a file vanishing mid-scan is
/// ordinary), they never abort the scan.
pub async fn scan(services: &Services, progress: ProgressMonitor) -> Result<ScanOutcome, String> {
    let store = services.library_store.clone();
    let roots = store.list_roots().await.map_err(report_message)?;
    let indexed = store.indexed_files().await.map_err(report_message)?;
    let mut indexed_map = indexed_map(&indexed);
    let supported = djcore::decoder::DecoderRegistry::with_symphonia().supported_extensions();

    let mut outcome = ScanOutcome::default();
    let mut seen: HashSet<(i64, String)> = HashSet::new();
    let mut batch: Vec<(LibraryEntry, String)> = Vec::new();
    let mut files_done = 0usize;

    // Walker on the blocking pool: walkdir iterates synchronously, so
    // keeping it on the executor would stall every other task for the
    // whole traversal. The channel is the streaming seam — discovery
    // and classification interleave, counts climb from the first files.
    let (found_tx, mut found_rx) = tokio::sync::mpsc::channel::<Found>(BATCH_STEP);
    let walk_handle = {
        let roots = roots.clone();
        let supported = supported.clone();
        services.runtime.spawn_blocking({
            let progress = progress.clone();
            move || {
                for root in &roots {
                    for entry in walkdir::WalkDir::new(&root.path).follow_links(false) {
                        let Ok(entry) = entry else { continue };
                        if !entry.file_type().is_file() {
                            continue;
                        }
                        let file = entry.into_path();
                        if !supported
                            .iter()
                            .any(|candidate| *candidate == identity::extension_of(&file))
                        {
                            continue;
                        }
                        let Some(rel) = rel_path(&root.path, &file) else {
                            continue;
                        };
                        if found_tx
                            .blocking_send(Found {
                                root_id: root.id,
                                root_path: root.path.clone(),
                                rel,
                                file,
                            })
                            .is_err()
                        {
                            return; // classifier gone: scan failed
                        }
                        progress.add_seen(1);
                    }
                }
            }
        })
    };

    while let Some(found) = found_rx.recv().await {
        let Found {
            root_id,
            root_path,
            rel,
            file,
        } = found;
        let key = (root_id, rel.to_string_lossy().into_owned());
        seen.insert(key.clone());
        // Stat/hash/tags on the blocking pool: each classify is a disk
        // read of a whole audio file, never executor work.
        let classify_map = indexed_map.clone();
        let entry = {
            let root = crate::library::store::LibraryRoot {
                id: root_id,
                path: root_path,
            };
            let rel2 = rel.clone();
            let file2 = file.clone();
            services
                .runtime
                .spawn_blocking(move || classify_file(&root, &rel2, &file2, &classify_map))
                .await
                .map_err(|e| e.to_string())?
        };
        if let Some(entry) = entry {
            if indexed_map.contains_key(&key) {
                outcome.updated += 1;
            } else {
                outcome.added += 1;
            }
            indexed_map.insert(key, (&entry).into());
            batch.push((entry, file.display().to_string()));
        }
        files_done += 1;
        progress.add_processed(1);
        if files_done.is_multiple_of(BATCH_STEP) {
            flush_batch(&store, &mut batch).await?;
        }
    }
    walk_handle
        .await
        .map_err(|e| format!("walk task failed: {e}"))?;
    flush_batch(&store, &mut batch).await?;
    progress.reset();
    outcome.pruned = prune_unseen(&store, &indexed, &seen).await?;
    Ok(outcome)
}

/// Commits the pending batch in one store transaction.
async fn flush_batch(
    store: &LibraryStoreService,
    batch: &mut Vec<(LibraryEntry, String)>,
) -> Result<(), String> {
    if batch.is_empty() {
        return Ok(());
    }
    store.upsert_files(batch).await.map_err(report_message)?;
    batch.clear();
    Ok(())
}

/// `(root_id, rel_path)` keyed change-detection snapshot.
fn indexed_map(indexed: &[IndexedFile]) -> HashMap<(i64, String), IndexedFile> {
    indexed
        .iter()
        .cloned()
        .map(|row| {
            (
                (row.root_id, row.rel_path.to_string_lossy().into_owned()),
                row,
            )
        })
        .collect()
}

/// Classifies one walked file against the index: `None` skips (vanished
/// mid-walk or unchanged — no read), `Some(entry)` needs a store write
/// and has been read exactly once (hash, tags, duration).
fn classify_file(
    root: &crate::library::store::LibraryRoot,
    rel: &Path,
    file: &Path,
    indexed_map: &HashMap<(i64, String), IndexedFile>,
) -> Option<LibraryEntry> {
    let stat = std::fs::metadata(file).ok()?;
    let key = (root.id, rel.to_string_lossy().into_owned());
    if indexed_map
        .get(&key)
        .is_some_and(|row| stat_matches(row, &stat))
    {
        return None; // unchanged: skip without a read
    }
    read_file(root.id, rel, file, &stat)
}

/// Deletes every indexed row that the walk did not see, in batches.
async fn prune_unseen(
    store: &LibraryStoreService,
    indexed: &[IndexedFile],
    seen: &HashSet<(i64, String)>,
) -> Result<usize, String> {
    let vanished: Vec<IndexedFile> = indexed
        .iter()
        .filter(|row| !seen.contains(&(row.root_id, row.rel_path.to_string_lossy().into_owned())))
        .cloned()
        .collect();
    for chunk in vanished.chunks(BATCH_STEP) {
        store.delete_files(chunk).await.map_err(report_message)?;
    }
    Ok(vanished.len())
}

/// Path of `file` relative to `root` (`None` when not under it).
fn rel_path(root: &Path, file: &Path) -> Option<PathBuf> {
    file.strip_prefix(root).ok().map(Path::to_path_buf)
}

/// Whether an indexed row's change-detection facts match the stat
/// (mtime truncated to seconds — the SQLite column is seconds).
fn stat_matches(row: &IndexedFile, stat: &std::fs::Metadata) -> bool {
    #[expect(
        clippy::cast_possible_wrap,
        reason = "system times map to unix seconds"
    )]
    let mtime_secs = stat
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(-1, |d| d.as_secs() as i64);
    let size_bytes = i64::try_from(stat.len()).unwrap_or(-1);
    row.mtime_secs == mtime_secs && row.size_bytes == size_bytes
}

/// Reads one file and produces its index entry (hash, tags, duration,
/// change facts). `None` skips the file (read failure).
fn read_file(
    root_id: i64,
    rel: &Path,
    file: &Path,
    stat: &std::fs::Metadata,
) -> Option<LibraryEntry> {
    let bytes = std::fs::read(file).ok()?;
    let hash = TrackHash(identity::hex_sha256(&bytes));
    let tags = identity::resolve_tags(&bytes, file);
    let duration = identity::probe_duration(&bytes, file);
    #[expect(
        clippy::cast_possible_wrap,
        reason = "system times map to unix seconds"
    )]
    let mtime_secs = stat
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(LibraryEntry {
        root_id,
        rel_path: rel.to_owned(),
        hash,
        title: tags.title,
        artist: tags.artist,
        duration,
        mtime_secs,
        size_bytes: i64::try_from(stat.len()).unwrap_or(0),
    })
}

/// Renders a report into a display string.
fn report_message<E>(report: error_stack::Report<E>) -> String {
    format!("{report:#}")
}
