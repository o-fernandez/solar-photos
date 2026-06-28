//! Folder scanning: walk a directory tree, find the image files we support, and
//! record them in the database. This is deliberately *cheap* — we only read
//! file metadata (path, size, modified-time), never decode pixels. That keeps a
//! scan of 100k+ files fast (Principle 6), and lets the grid render its full
//! skeleton the instant the walk finishes, before any thumbnail exists.

use anyhow::Result;
use jwalk::WalkDir;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::db::{Job, STATUS_PENDING, STATUS_READY};

/// The only formats v1 supports. RAW and video are explicitly out of scope.
const SUPPORTED: &[&str] = &["jpg", "jpeg", "png", "webp", "heic", "heif"];

fn is_supported(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => SUPPORTED.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

/// A cache key that changes whenever the file's content plausibly changes.
/// Path + modified-time + size is cheap to compute and good enough to detect
/// edits/replacements without hashing the whole file.
fn cache_key(path: &str, mtime: i64, size: i64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.as_bytes());
    hasher.update(b"|");
    hasher.update(mtime.to_le_bytes());
    hasher.update(b"|");
    hasher.update(size.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

/// Walk `root`, upsert every supported image into the DB, and return the set of
/// photos that need a (re)generated thumbnail.
///
/// A photo needs work when it is new, or when its cache key changed (the file
/// was edited/replaced). Unchanged, already-thumbnailed photos are skipped —
/// this is what makes re-scanning an existing library nearly free.
pub fn scan(conn: &mut Connection, root: &str) -> Result<Vec<Job>> {
    let tx = conn.transaction()?;
    let mut jobs: Vec<Job> = Vec::new();

    {
        let mut select = tx.prepare("SELECT id, cache_key, thumb_status FROM photos WHERE path = ?1")?;
        let mut insert =
            tx.prepare("INSERT INTO photos (path, mtime, size, cache_key, thumb_status) VALUES (?1, ?2, ?3, ?4, ?5)")?;
        let mut update = tx.prepare(
            "UPDATE photos SET mtime = ?1, size = ?2, cache_key = ?3, thumb_status = ?4 WHERE id = ?5",
        )?;

        for entry in WalkDir::new(root).skip_hidden(false) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue, // unreadable dir/file — skip, never abort the scan
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
            let size = meta.len() as i64;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let path_str = path.to_string_lossy().to_string();
            let key = cache_key(&path_str, mtime, size);

            // Does this path already exist?
            let existing = select
                .query_row([&path_str], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
                })
                .ok();

            match existing {
                Some((_id, old_key, status)) if old_key == key && status == STATUS_READY => {
                    // Unchanged and already cached — nothing to do.
                }
                Some((id, _, _)) => {
                    // Known file but changed (or thumbnail missing) — regenerate.
                    update.execute(rusqlite::params![mtime, size, key, STATUS_PENDING, id])?;
                    jobs.push(Job { id, path: path_str });
                }
                None => {
                    // Brand new file.
                    insert.execute(rusqlite::params![path_str, mtime, size, key, STATUS_PENDING])?;
                    let id = tx.last_insert_rowid();
                    jobs.push(Job { id, path: path_str });
                }
            }
        }
    }

    tx.commit()?;
    Ok(jobs)
}
