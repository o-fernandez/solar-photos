//! The thumbnail pipeline: a priority work-queue plus a pool of worker threads
//! that decode originals and write small cached thumbnails to disk.
//!
//! How this honors the Responsiveness Principles:
//!
//! * **Off the UI thread (P1).** All decoding happens on these native worker
//!   threads. The webview's JavaScript thread — the one that scrolls the grid —
//!   is never asked to do image work, so it can't be blocked.
//! * **Foreground wins (P3).** The queue has two lanes: a `priority` lane for
//!   what the user is currently looking at, and a `normal` lane for everything
//!   else. Workers always drain `priority` first. As the user scrolls, the
//!   frontend keeps overwriting the priority lane with the now-visible photos,
//!   so on-screen thumbnails are always generated ahead of off-screen ones.
//! * **No reflow (P2).** A finished thumbnail is announced via an event that
//!   carries only the photo id. The frontend swaps that one cell's placeholder
//!   for the image, inside a box whose size never changes — so nothing on
//!   screen moves, and results for off-screen cells just sit in the cache.
//! * **Scale (P6).** Thumbnails are written as individual files bucketed into
//!   subfolders of ~1000 each, so no directory ever holds 100k entries.

use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use image::{DynamicImage, ImageReader};

use crate::db::{self, Job, STATUS_FAILED, STATUS_READY};

/// Longest edge of a generated thumbnail, in pixels. One size for v1.
const THUMB_EDGE: u32 = 256;
/// JPEG quality for cached thumbnails (1–100). 80 is a good size/quality knee.
const THUMB_QUALITY: u8 = 80;

/// Where a photo's cached thumbnail lives. Derived from the id alone so the
/// custom `thumb://` protocol can find it without touching the database.
/// Bucketed by id/1000 to keep each directory small (Principle 6).
pub fn thumb_path(cache_dir: &Path, id: i64) -> PathBuf {
    cache_dir.join((id / 1000).to_string()).join(format!("{id}.jpg"))
}

/// The shared work-queue. Two lanes (priority + normal) guarded by one lock,
/// with a condvar so idle workers sleep instead of spinning.
pub struct ThumbQueue {
    inner: Mutex<Inner>,
    available: Condvar,
}

struct Inner {
    /// Every still-pending job, by id. Removed when a worker takes it. Acts as
    /// the dedupe set: an id present here is "queued, not yet taken".
    jobs: HashMap<i64, Job>,
    /// Background order (FIFO) for everything not currently on screen.
    normal: VecDeque<i64>,
    /// What the user is looking at right now — drained before `normal`.
    priority: VecDeque<i64>,
    priority_set: HashSet<i64>,
    /// Set on shutdown so workers can exit their loop.
    shutdown: bool,
}

impl ThumbQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(ThumbQueue {
            inner: Mutex::new(Inner {
                jobs: HashMap::new(),
                normal: VecDeque::new(),
                priority: VecDeque::new(),
                priority_set: HashSet::new(),
                shutdown: false,
            }),
            available: Condvar::new(),
        })
    }

    /// Add jobs to the background lane. No-op for ids already queued.
    pub fn enqueue(&self, jobs: Vec<Job>) {
        let mut inner = self.inner.lock().unwrap();
        for job in jobs {
            if !inner.jobs.contains_key(&job.id) {
                inner.normal.push_back(job.id);
                inner.jobs.insert(job.id, job);
            }
        }
        drop(inner);
        self.available.notify_all();
    }

    /// Replace the priority lane with the photos currently visible. Ids that are
    /// no longer pending are ignored. Called (debounced) on every scroll, so the
    /// lane always reflects the live viewport.
    pub fn set_priority(&self, ids: Vec<i64>) {
        let mut inner = self.inner.lock().unwrap();
        inner.priority.clear();
        inner.priority_set.clear();
        for id in ids {
            if inner.jobs.contains_key(&id) && inner.priority_set.insert(id) {
                inner.priority.push_back(id);
            }
        }
        drop(inner);
        self.available.notify_all();
    }

    /// Block until a job is available (priority first), or return `None` on
    /// shutdown. Taking a job removes it from `jobs`, so any stale copy left in
    /// the other lane is skipped when popped.
    fn take(&self) -> Option<Job> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if inner.shutdown {
                return None;
            }
            // Priority lane first.
            while let Some(id) = inner.priority.pop_front() {
                inner.priority_set.remove(&id);
                if let Some(job) = inner.jobs.remove(&id) {
                    return Some(job);
                }
            }
            // Then the background lane, skipping any stale ids.
            while let Some(id) = inner.normal.pop_front() {
                if let Some(job) = inner.jobs.remove(&id) {
                    return Some(job);
                }
            }
            inner = self.available.wait(inner).unwrap();
        }
    }
}

/// Spawn `count` worker threads. Each owns its own DB connection (SQLite allows
/// many connections to one WAL database) and emits a `thumb-ready` event after
/// every photo so the frontend can update exactly one cell.
pub fn spawn_workers<F>(count: usize, queue: Arc<ThumbQueue>, db_path: PathBuf, cache_dir: PathBuf, notify: F)
where
    F: Fn(i64) + Send + Clone + 'static,
{
    for _ in 0..count {
        let queue = queue.clone();
        let db_path = db_path.clone();
        let cache_dir = cache_dir.clone();
        let notify = notify.clone();
        std::thread::spawn(move || {
            // A worker's DB connection is only used to record outcomes.
            let conn = match db::open(&db_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("worker: cannot open db: {e}");
                    return;
                }
            };
            while let Some(job) = queue.take() {
                let status = match generate(&cache_dir, &job) {
                    Ok(()) => STATUS_READY,
                    Err(e) => {
                        eprintln!("thumbnail failed for {}: {e}", job.path);
                        STATUS_FAILED
                    }
                };
                let _ = db::set_status(&conn, job.id, status);
                // Always notify, even on failure, so the cell can stop waiting.
                notify(job.id);
            }
        });
    }
}

/// Decode one original, downscale it, and write the cached JPEG thumbnail.
fn generate(cache_dir: &Path, job: &Job) -> Result<()> {
    let img = load_image(Path::new(&job.path))?;
    // `thumbnail` is an optimized downscaler that preserves aspect ratio and
    // fits the image within THUMB_EDGE × THUMB_EDGE.
    let thumb = img.thumbnail(THUMB_EDGE, THUMB_EDGE).to_rgb8();
    let (w, h) = thumb.dimensions();

    let mut buf: Vec<u8> = Vec::new();
    {
        let mut encoder =
            image::codecs::jpeg::JpegEncoder::new_with_quality(Cursor::new(&mut buf), THUMB_QUALITY);
        encoder.encode(thumb.as_raw(), w, h, image::ExtendedColorType::Rgb8)?;
    }

    let out = thumb_path(cache_dir, job.id);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&out, &buf)?;
    Ok(())
}

/// Decode any supported format into an in-memory image. HEIC/HEIF go through
/// libheif; everything else through the `image` crate.
fn load_image(path: &Path) -> Result<DynamicImage> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    if ext == "heic" || ext == "heif" {
        decode_heic(path)
    } else {
        Ok(ImageReader::open(path)?.with_guessed_format()?.decode()?)
    }
}

/// Decode a HEIC/HEIF file using libheif and copy its interleaved RGB plane into
/// an `image::RgbImage` (handling row stride, which is often wider than width).
fn decode_heic(path: &Path) -> Result<DynamicImage> {
    use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};

    let lib = LibHeif::new();
    let ctx = HeifContext::read_from_file(
        path.to_str().ok_or_else(|| anyhow!("non-utf8 path"))?,
    )?;
    let handle = ctx.primary_image_handle()?;
    let decoded = lib.decode(&handle, ColorSpace::Rgb(RgbChroma::Rgb), None)?;

    let planes = decoded.planes();
    let plane = planes
        .interleaved
        .ok_or_else(|| anyhow!("heic: no interleaved plane"))?;
    let width = plane.width as usize;
    let height = plane.height as usize;
    let stride = plane.stride;
    let data = plane.data;

    let mut rgb = vec![0u8; width * height * 3];
    for y in 0..height {
        let src = &data[y * stride..y * stride + width * 3];
        rgb[y * width * 3..(y + 1) * width * 3].copy_from_slice(src);
    }

    let buf = image::RgbImage::from_raw(width as u32, height as u32, rgb)
        .ok_or_else(|| anyhow!("heic: buffer size mismatch"))?;
    Ok(DynamicImage::ImageRgb8(buf))
}
