//! Folder scanning: walk a directory tree, find the image files we support, and
//! record them in the database.
//!
//! This is deliberately *streaming and cheap*. It only reads file metadata
//! (path, size, modified-time) — never pixels — so it stays fast even at 100k+
//! files (Principle 6). Crucially it runs on a background thread, commits in
//! small batches, and emits progress as it goes, so the UI never freezes waiting
//! for a giant folder to finish (Principle 1). Each batch grows the grid; new
//! photos only ever append (discovery order), so the user's view never reflows
//! (Principle 2).
//!
//! Cloud awareness: reading a file's metadata does NOT download it. We detect
//! cloud-only originals (macOS marks un-downloaded File Provider files as
//! "dataless") and register them as placeholders without thumbnailing — we do
//! not bulk-download a cloud library. Those wait until the user scrolls to them.

use anyhow::Result;
use jwalk::WalkDir;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use crate::db::{self, Job, STATUS_CLOUD, STATUS_PENDING, STATUS_READY};
use crate::meta;
use crate::thumbs::ThumbQueue;

/// The only formats v1 supports. RAW and video are explicitly out of scope.
const SUPPORTED: &[&str] = &["jpg", "jpeg", "png", "webp", "heic", "heif"];

/// How many files we register per transaction / progress tick.
const BATCH: usize = 256;

fn is_supported(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => SUPPORTED.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

/// macOS marks the content of an un-downloaded File Provider file (iCloud,
/// Proton Drive, Dropbox, etc.) as "dataless". `SF_DATALESS` in `st_flags` lets
/// us recognize a cloud-only original without triggering a download.
#[cfg(target_os = "macos")]
fn is_cloud(meta: &std::fs::Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;
    const SF_DATALESS: u32 = 0x4000_0000;
    (meta.st_flags() & SF_DATALESS) != 0
}

#[cfg(not(target_os = "macos"))]
fn is_cloud(_meta: &std::fs::Metadata) -> bool {
    false
}

/// A cache key that changes whenever the file's content plausibly changes.
/// Path + modified-time + size is cheap and good enough to detect edits without
/// hashing the whole file (and without downloading a cloud original).
fn cache_key(path: &str, mtime: i64, size: i64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.as_bytes());
    hasher.update(b"|");
    hasher.update(mtime.to_le_bytes());
    hasher.update(b"|");
    hasher.update(size.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

/// One supported file's metadata, gathered before we touch the DB.
struct Item {
    path: String,
    mtime: i64,
    size: i64,
    cloud: bool,
}

/// Walk `root` and register every supported image, streaming results in batches.
///
/// Runs on its own DB connection and commits per batch, so it never holds a lock
/// that would stall the grid's reads. Local photos that need a thumbnail are
/// enqueued onto `queue` as they're discovered; cloud-only photos are recorded as
/// placeholders and left for on-demand handling. `progress(total, done)` is
/// invoked after each batch so the frontend can grow the grid live.
pub fn run_scan<F>(db_path: &Path, root: &str, gen: i64, queue: Arc<ThumbQueue>, progress: F) -> Result<()>
where
    F: Fn(i64, bool),
{
    let mut conn = db::open(db_path)?;
    let mut total: i64 = conn.query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))?;

    let mut walk = WalkDir::new(root).skip_hidden(false).into_iter();

    loop {
        // Gather up to BATCH supported files (metadata only — no downloads).
        let mut items: Vec<Item> = Vec::with_capacity(BATCH);
        while items.len() < BATCH {
            let entry = match walk.next() {
                None => break,
                Some(Ok(e)) => e,
                Some(Err(_)) => continue, // unreadable dir/file — skip, never abort
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if !is_supported(&path) {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let cloud = is_cloud(&meta);
            let size = meta.len() as i64;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            items.push(Item {
                path: path.to_string_lossy().to_string(),
                mtime,
                size,
                cloud,
            });
        }

        if items.is_empty() {
            break;
        }

        // Register this batch in one transaction.
        let mut new_jobs: Vec<Job> = Vec::new();
        let tx = conn.transaction()?;
        {
            let mut select =
                tx.prepare("SELECT id, cache_key, thumb_status FROM photos WHERE path = ?1")?;
            let mut insert = tx.prepare(
                "INSERT INTO photos (path, mtime, size, cache_key, thumb_status, taken_ts, seen) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            let mut update = tx.prepare(
                "UPDATE photos SET mtime = ?1, size = ?2, cache_key = ?3, thumb_status = ?4, taken_ts = ?5, seen = ?6 WHERE id = ?7",
            )?;
            // Bump only the seen-generation for files that are unchanged, so the
            // post-rescan prune knows they still exist.
            let mut touch = tx.prepare("UPDATE photos SET seen = ?1 WHERE id = ?2")?;

            for item in &items {
                let key = cache_key(&item.path, item.mtime, item.size);
                // Cloud-only files become placeholders; local files are queued.
                let fresh_status = if item.cloud { STATUS_CLOUD } else { STATUS_PENDING };
                // Capture date: EXIF for local files (reading a cloud original's
                // EXIF would force a download), else the date parsed from the
                // filename — free, and the main signal for cloud photos.
                let taken: Option<i64> = if item.cloud {
                    None
                } else {
                    meta::read_taken_ts(Path::new(&item.path))
                }
                .or_else(|| {
                    Path::new(&item.path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(meta::parse_filename_date)
                });

                let existing = select
                    .query_row([&item.path], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
                    })
                    .ok();

                match existing {
                    Some((id, old_key, status)) if old_key == key && status == STATUS_READY => {
                        // Unchanged and already cached — just mark it still here.
                        touch.execute(rusqlite::params![gen, id])?;
                    }
                    Some((id, _, _)) => {
                        update.execute(rusqlite::params![
                            item.mtime,
                            item.size,
                            key,
                            fresh_status,
                            taken,
                            gen,
                            id
                        ])?;
                        if !item.cloud {
                            new_jobs.push(Job { id, path: item.path.clone() });
                        }
                    }
                    None => {
                        insert.execute(rusqlite::params![
                            item.path,
                            item.mtime,
                            item.size,
                            key,
                            fresh_status,
                            taken,
                            gen
                        ])?;
                        let id = tx.last_insert_rowid();
                        total += 1;
                        if !item.cloud {
                            new_jobs.push(Job { id, path: item.path.clone() });
                        }
                    }
                }
            }
        }
        tx.commit()?;

        // Enqueue local thumbnails and grow the grid.
        queue.enqueue(new_jobs);
        progress(total, false);
    }

    progress(total, true);
    Ok(())
}
