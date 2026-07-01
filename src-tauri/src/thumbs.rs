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

use anyhow::Result;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use image::{DynamicImage, ImageDecoder, ImageReader};

use crate::db::{self, Job, STATUS_READY};

/// Longest edge of a generated thumbnail, in pixels. One size for v1.
const THUMB_EDGE: u32 = 256;
/// JPEG quality for cached thumbnails (1–100). 80 is a good size/quality knee.
const THUMB_QUALITY: u8 = 80;
/// Longest edge of a viewer preview, in pixels. Big enough to look full-screen
/// crisp, small enough to open instantly and stay cheap to cache.
const PREVIEW_EDGE: u32 = 2560;
/// JPEG quality for previews — a touch higher than thumbnails.
const PREVIEW_QUALITY: u8 = 85;

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
    /// Jobs a worker has taken but not yet finished. Lets `enqueue` /
    /// `replace_pending` avoid re-queueing a download that's already in flight
    /// (the cloud lane re-reports the same visible ids on every scroll tick).
    in_flight: HashSet<i64>,
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
                in_flight: HashSet::new(),
                shutdown: false,
            }),
            available: Condvar::new(),
        })
    }

    /// Add jobs to the background lane. Skips ids already queued or in flight.
    pub fn enqueue(&self, jobs: Vec<Job>) {
        let mut inner = self.inner.lock().unwrap();
        for job in jobs {
            if !inner.jobs.contains_key(&job.id) && !inner.in_flight.contains(&job.id) {
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

    /// Replace the *entire* pending set with exactly these jobs. Used by the
    /// cloud lane: as the user scrolls, we want to download only what's currently
    /// visible and abandon cloud fetches they scrolled past before we started
    /// them. Jobs already taken by a worker (a download in flight) keep running.
    pub fn replace_pending(&self, jobs: Vec<Job>) {
        let mut inner = self.inner.lock().unwrap();
        inner.jobs.clear();
        inner.normal.clear();
        inner.priority.clear();
        inner.priority_set.clear();
        for job in jobs {
            if !inner.jobs.contains_key(&job.id) && !inner.in_flight.contains(&job.id) {
                inner.normal.push_back(job.id);
                inner.jobs.insert(job.id, job);
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
                    inner.in_flight.insert(id);
                    return Some(job);
                }
            }
            // Then the background lane, skipping any stale ids.
            while let Some(id) = inner.normal.pop_front() {
                if let Some(job) = inner.jobs.remove(&id) {
                    inner.in_flight.insert(id);
                    return Some(job);
                }
            }
            inner = self.available.wait(inner).unwrap();
        }
    }

    /// Mark a taken job finished, so its id can be queued again later if needed.
    fn complete(&self, id: i64) {
        let mut inner = self.inner.lock().unwrap();
        inner.in_flight.remove(&id);
    }
}

/// Spawn `count` worker threads draining `queue`. Each owns its own DB connection
/// (SQLite allows many connections to one WAL database) and emits a `thumb-ready`
/// event after every photo so the frontend can update exactly one cell.
///
/// `fail_status` is what a photo becomes if generation fails: local photos go to
/// FAILED (we couldn't decode them), but cloud photos fall back to CLOUD so a
/// later visit can retry the download.
pub fn spawn_workers<F>(
    count: usize,
    queue: Arc<ThumbQueue>,
    db_path: PathBuf,
    cache_dir: PathBuf,
    preview_dir: PathBuf,
    fail_status: i64,
    extract_date: bool,
    notify: F,
) where
    F: Fn(i64, bool) + Send + Clone + 'static,
{
    for _ in 0..count {
        let queue = queue.clone();
        let db_path = db_path.clone();
        let cache_dir = cache_dir.clone();
        let preview_dir = preview_dir.clone();
        let notify = notify.clone();
        std::thread::spawn(move || {
            // Yield to the UI: decoding full-res images must never out-prioritize
            // the foreground (PRINCIPLES #3).
            crate::background_qos();
            // A worker's DB connection is only used to record outcomes.
            let conn = match db::open(&db_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("worker: cannot open db: {e}");
                    return;
                }
            };
            while let Some(job) = queue.take() {
                let status = match generate(&cache_dir, &preview_dir, &job) {
                    Ok(()) => STATUS_READY,
                    Err(e) => {
                        eprintln!("thumbnail failed for {}: {e}", job.path);
                        fail_status
                    }
                };
                let _ = db::set_status(&conn, job.id, status);
                // For cloud files (now downloaded), read the capture date the scan
                // couldn't reach without forcing a download. Applied on next sort.
                if extract_date && status == STATUS_READY {
                    if let Some(ts) = crate::meta::read_taken_ts(Path::new(&job.path)) {
                        let _ = db::set_taken_ts_if_empty(&conn, job.id, ts);
                    }
                }
                queue.complete(job.id);
                // Notify either way so the cell stops waiting, but say whether a
                // thumbnail actually exists now: a failed cloud download reverts
                // to CLOUD and must NOT be shown as a (missing) image.
                notify(job.id, status == STATUS_READY);
            }
        });
    }
}

/// Encode an RGB image as a JPEG byte buffer at the given quality.
fn encode_jpeg(img: &image::RgbImage, quality: u8) -> Result<Vec<u8>> {
    let (w, h) = img.dimensions();
    let mut buf: Vec<u8> = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(Cursor::new(&mut buf), quality);
    encoder.encode(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)?;
    Ok(buf)
}

/// Downscale a decoded original to a viewer preview, never upscaling a small
/// original. Shared by the thumbnail pass, on-demand preview generation, and the
/// face sweep's fallback so "what a preview is" lives in one place.
fn to_preview(img: &DynamicImage) -> DynamicImage {
    if img.width() > PREVIEW_EDGE || img.height() > PREVIEW_EDGE {
        img.thumbnail(PREVIEW_EDGE, PREVIEW_EDGE)
    } else {
        img.clone()
    }
}

/// Decode one original **once** and derive both cached artifacts from it: the grid
/// thumbnail and the viewer preview. Deriving the preview here (rather than lazily)
/// means the original — often an expensive HEIC or a cold cloud fetch — is never
/// decoded a second time: the viewer and the background face sweep both read the
/// cached preview instead of re-materializing the original. The extra work added
/// to this pass is only a downscale + JPEG encode of pixels already in memory,
/// which is cheap next to the decode it saves downstream.
fn generate(cache_dir: &Path, preview_dir: &Path, job: &Job) -> Result<()> {
    let img = decode_oriented(Path::new(&job.path))?;
    // `thumbnail` is an optimized downscaler that preserves aspect ratio and
    // fits the image within THUMB_EDGE × THUMB_EDGE.
    let thumb = img.thumbnail(THUMB_EDGE, THUMB_EDGE).to_rgb8();
    let buf = encode_jpeg(&thumb, THUMB_QUALITY)?;

    let out = thumb_path(cache_dir, job.id);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&out, &buf)?;

    // Cache the preview from the same decoded image. Best-effort: a preview write
    // failure must never fail the thumbnail (the grid is what the user sees now).
    // Skip if one already exists so re-processing a photo doesn't re-encode it.
    let pv = preview_path(preview_dir, job.id);
    if !pv.exists() {
        let t = std::time::Instant::now();
        if let Ok(pbuf) = encode_jpeg(&to_preview(&img).to_rgb8(), PREVIEW_QUALITY) {
            if let Some(parent) = pv.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&pv, &pbuf);
        }
        crate::prof::record(crate::prof::Stage::Preview, t.elapsed());
    }
    Ok(())
}

/// Generate the large viewer preview for one photo: decode the original
/// (orientation already applied), downscale to fit PREVIEW_EDGE, encode JPEG,
/// cache to disk, and return the bytes. Called on demand when the viewer opens a
/// photo (and to prefetch its neighbors). Cached forever after first view.
pub fn generate_preview(out: &Path, original_path: &str) -> Result<Vec<u8>> {
    let img = decode_oriented(Path::new(original_path))?;
    let buf = encode_jpeg(&to_preview(&img).to_rgb8(), PREVIEW_QUALITY)?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, &buf)?;
    Ok(buf)
}

/// Where a photo's cached preview lives (sibling scheme to `thumb_path`).
pub fn preview_path(preview_dir: &Path, id: i64) -> std::path::PathBuf {
    preview_dir.join((id / 1000).to_string()).join(format!("{id}.jpg"))
}

/// Public wrapper: load an original as an upright (EXIF-oriented) image. Used by
/// the face pipeline, which needs the full-resolution oriented pixels.
pub fn load_oriented(path: &Path) -> Result<DynamicImage> {
    decode_oriented(path)
}

/// The image the face sweep detects + embeds from. Prefers the cached preview
/// (local, already downscaled) so we never re-read the original — the originals
/// are cloud-backed, and re-materializing each one is the whole cost of the
/// sweep. If no preview exists yet, decode the original once, cache a preview
/// (so this never repeats, and the viewer opens instantly later), and return it.
///
/// A PREVIEW_EDGE (2560px) source is plenty for detection (YuNet runs at 640) and
/// for SFace's 112px aligned crops — measured cosine vs the full-res embedding is
/// ~0.94, well inside same-person territory, so clustering quality is unchanged.
pub fn load_face_source(preview_dir: &Path, id: i64, original_path: &str) -> Result<image::RgbImage> {
    let pv = preview_path(preview_dir, id);
    if let Ok(bytes) = std::fs::read(&pv) {
        if let Ok(img) = image::load_from_memory(&bytes) {
            return Ok(img.to_rgb8());
        }
    }
    // One read of the original (cold cloud fetch); cache a preview so the next
    // pass — and the viewer — stay local.
    let img = decode_oriented(Path::new(original_path))?;
    let rgb = to_preview(&img).to_rgb8();
    // Best-effort cache; a write failure shouldn't sink the face work.
    if let Ok(buf) = encode_jpeg(&rgb, PREVIEW_QUALITY) {
        if let Some(parent) = pv.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&pv, &buf);
    }
    Ok(rgb)
}

/// Decode any supported format into an in-memory image, with EXIF orientation
/// applied so nothing is ever sideways. Genuine HEIC/HEIF goes through a
/// platform-specific path; everything else (including JPEG files that carry a
/// .heic extension, which photo-management tools sometimes produce) goes through
/// the `image` crate, which handles EXIF orientation correctly for each format.
fn decode_oriented(path: &Path) -> Result<DynamicImage> {
    let t = std::time::Instant::now();
    let img = decode_oriented_inner(path);
    if img.is_ok() {
        crate::prof::record(crate::prof::Stage::Decode, t.elapsed());
    }
    img
}

fn decode_oriented_inner(path: &Path) -> Result<DynamicImage> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    if (ext == "heic" || ext == "heif") && !has_jpeg_magic(path) {
        return decode_heic(path);
    }

    let mut decoder = ImageReader::open(path)?.with_guessed_format()?.into_decoder()?;
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    Ok(img)
}

/// Returns true when the file starts with the JPEG magic bytes (FF D8).
/// Used to detect JPEG files that carry a .heic extension.
fn has_jpeg_magic(path: &Path) -> bool {
    use std::io::Read;
    let mut buf = [0u8; 2];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut buf).map(|_| ()))
        .map(|_| buf == [0xFF, 0xD8])
        .unwrap_or(false)
}

// ── macOS: decode HEIC via the ImageIO system framework ──────────────────────
//
// Raw FFI against CoreFoundation, ImageIO, and CoreGraphics — no extra crates.
// The three frameworks are always available on macOS.
//
// Pipeline:
//   file path → CFURL → CGImageSource → CGImage (full-res, unrotated)
//   → draw into sRGB CGBitmapContext (P3→sRGB conversion is automatic)
//   → copy ARGB bytes → strip alpha → RgbImage → apply EXIF orientation

#[cfg(target_os = "macos")]
mod macos_heic {
    use anyhow::{anyhow, Result};
    use image::{DynamicImage, RgbImage};
    use std::ffi::{c_void, CString};

    // Opaque pointer types for CF/CG objects.
    type CFTypeRef       = *const c_void;
    type CFStringRef     = *const c_void;
    type CFURLRef        = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CGImageSourceRef= *const c_void;
    type CGImageRef      = *const c_void;
    type CGColorSpaceRef = *const c_void;
    type CGContextRef    = *const c_void;

    // CGRect as used by CGContextDrawImage (origin + size, all f64 on 64-bit macOS).
    #[repr(C)]
    struct CGRect { x: f64, y: f64, width: f64, height: f64 }

    // Bitmap format: kCGBitmapByteOrder32Big (4<<12) | kCGImageAlphaPremultipliedFirst (2)
    // → [A, R, G, B] in memory per pixel.  Photos are opaque so premul == plain RGB.
    const BITMAP_INFO: u32 = (4 << 12) | 2;
    const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const CF_URL_POSIX_PATH_STYLE: i32 = 0;

    #[allow(non_upper_case_globals)]
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFAllocatorDefault: CFTypeRef;
        static kCFBooleanTrue: CFTypeRef;
        // kCFTypeDictionaryKey/ValueCallBacks are C structs; we only ever pass
        // their address, so an opaque u8 placeholder is enough.
        static kCFTypeDictionaryKeyCallBacks: u8;
        static kCFTypeDictionaryValueCallBacks: u8;
        fn CFRelease(cf: CFTypeRef);
        fn CFStringCreateWithCString(alloc: CFTypeRef, c_str: *const std::os::raw::c_char, encoding: u32) -> CFStringRef;
        fn CFURLCreateWithFileSystemPath(alloc: CFTypeRef, path: CFStringRef, style: i32, is_dir: bool) -> CFURLRef;
        fn CFDictionaryCreate(
            alloc: CFTypeRef,
            keys: *const CFTypeRef,
            values: *const CFTypeRef,
            num_values: isize,
            key_callbacks: *const u8,
            value_callbacks: *const u8,
        ) -> CFDictionaryRef;
    }

    #[allow(non_upper_case_globals)]
    #[link(name = "ImageIO", kind = "framework")]
    extern "C" {
        fn CGImageSourceCreateWithURL(url: CFURLRef, options: CFDictionaryRef) -> CGImageSourceRef;
        // CreateThumbnail + WithTransform is the only API that consistently applies
        // ALL HEIC orientation mechanisms (EXIF tag AND the container irot box).
        // Without ThumbnailMaxPixelSize it decodes at the full image resolution.
        fn CGImageSourceCreateThumbnailAtIndex(src: CGImageSourceRef, index: usize, options: CFDictionaryRef) -> CGImageRef;
        static kCGImageSourceCreateThumbnailFromImageAlways: CFStringRef;
        static kCGImageSourceCreateThumbnailWithTransform: CFStringRef;
    }

    #[allow(non_upper_case_globals)]
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGImageGetWidth(image: CGImageRef) -> usize;
        fn CGImageGetHeight(image: CGImageRef) -> usize;
        fn CGImageRelease(image: CGImageRef);
        fn CGColorSpaceCreateWithName(name: CFStringRef) -> CGColorSpaceRef;
        fn CGColorSpaceRelease(space: CGColorSpaceRef);
        fn CGBitmapContextCreate(
            data: *mut c_void, width: usize, height: usize,
            bits_per_component: usize, bytes_per_row: usize,
            space: CGColorSpaceRef, bitmap_info: u32,
        ) -> CGContextRef;
        fn CGContextRelease(ctx: CGContextRef);
        fn CGContextTranslateCTM(ctx: CGContextRef, tx: f64, ty: f64);
        fn CGContextScaleCTM(ctx: CGContextRef, sx: f64, sy: f64);
        fn CGContextDrawImage(ctx: CGContextRef, rect: CGRect, image: CGImageRef);
        static kCGColorSpaceSRGB: CFStringRef;
    }

    pub fn decode(path: &std::path::Path) -> Result<DynamicImage> {
        let path_str = path.to_str().ok_or_else(|| anyhow!("non-utf8 path"))?;
        let c_path = CString::new(path_str).map_err(|_| anyhow!("null byte in path"))?;
        unsafe { decode_raw(&c_path, path_str) }
    }

    #[allow(non_upper_case_globals)]
    unsafe fn decode_raw(c_path: &CString, path_str: &str) -> Result<DynamicImage> {
        use std::ptr::null;

        // 1. Build a CFURL from the file-system path.
        let cf_str = CFStringCreateWithCString(kCFAllocatorDefault, c_path.as_ptr(), CF_STRING_ENCODING_UTF8);
        if cf_str.is_null() { return Err(anyhow!("CFStringCreateWithCString failed")); }
        let url = CFURLCreateWithFileSystemPath(kCFAllocatorDefault, cf_str, CF_URL_POSIX_PATH_STYLE, false);
        CFRelease(cf_str);
        if url.is_null() { return Err(anyhow!("CFURLCreateWithFileSystemPath failed")); }

        // 2. Open the HEIC through ImageIO.
        let src = CGImageSourceCreateWithURL(url, null());
        CFRelease(url);
        if src.is_null() { return Err(anyhow!("cannot open HEIC: {path_str}")); }

        // 3. Build decode options:
        //    - CreateThumbnailFromImageAlways: decode full image data, not an embedded thumb.
        //    - CreateThumbnailWithTransform: let ImageIO apply ALL orientation info (EXIF tag
        //      AND the HEIC container's irot box) exactly once.
        //    Without ThumbnailMaxPixelSize the output is full-resolution.
        let opts = {
            let keys: [CFTypeRef; 2] = [
                kCGImageSourceCreateThumbnailFromImageAlways as CFTypeRef,
                kCGImageSourceCreateThumbnailWithTransform as CFTypeRef,
            ];
            let vals: [CFTypeRef; 2] = [kCFBooleanTrue, kCFBooleanTrue];
            CFDictionaryCreate(
                kCFAllocatorDefault,
                keys.as_ptr(), vals.as_ptr(), 2,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        };
        if opts.is_null() {
            CFRelease(src);
            return Err(anyhow!("CFDictionaryCreate failed"));
        }

        // 4. Decode: full-resolution, orientation already applied by ImageIO.
        let cg_img = CGImageSourceCreateThumbnailAtIndex(src, 0, opts);
        CFRelease(src);
        CFRelease(opts);
        if cg_img.is_null() { return Err(anyhow!("HEIC decode failed: {path_str}")); }

        let width  = CGImageGetWidth(cg_img);
        let height = CGImageGetHeight(cg_img);
        if width == 0 || height == 0 {
            CGImageRelease(cg_img);
            return Err(anyhow!("zero-size HEIC: {path_str}"));
        }

        // 5. Create an sRGB CGBitmapContext backed by a Vec<u8> we own.
        //    Drawing a Display-P3 source into an sRGB context performs the
        //    gamut conversion automatically — prevents oversaturation.
        let srgb = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
        if srgb.is_null() {
            CGImageRelease(cg_img);
            return Err(anyhow!("failed to create sRGB color space"));
        }
        let bytes_per_row = width * 4;
        let mut buf = vec![0u8; bytes_per_row * height];
        let ctx = CGBitmapContextCreate(
            buf.as_mut_ptr() as *mut c_void,
            width, height, 8, bytes_per_row, srgb, BITMAP_INFO,
        );
        CGColorSpaceRelease(srgb);
        if ctx.is_null() {
            CGImageRelease(cg_img);
            return Err(anyhow!("CGBitmapContextCreate failed"));
        }

        // 6. CG bitmap contexts origin is bottom-left; flip the CTM so that
        //    row 0 of the drawn pixels maps to row 0 (top) of our buffer.
        CGContextTranslateCTM(ctx, 0.0, height as f64);
        CGContextScaleCTM(ctx, 1.0, -1.0);
        CGContextDrawImage(ctx, CGRect { x: 0.0, y: 0.0, width: width as f64, height: height as f64 }, cg_img);
        // Releasing the context does NOT free buf — we supplied the data pointer.
        CGContextRelease(ctx);
        CGImageRelease(cg_img);

        // 7. buf holds [A,R,G,B] per pixel. Extract RGB (alpha is always 255 for photos).
        let rgb: Vec<u8> = buf.chunks_exact(4).flat_map(|p| [p[1], p[2], p[3]]).collect();
        let rgb_img = RgbImage::from_raw(width as u32, height as u32, rgb)
            .ok_or_else(|| anyhow!("buffer size mismatch"))?;
        Ok(DynamicImage::ImageRgb8(rgb_img))
    }
}

#[cfg(target_os = "macos")]
fn decode_heic(path: &Path) -> Result<DynamicImage> {
    macos_heic::decode(path)
}

// ── non-macOS: fall back to bundled libheif ───────────────────────────────────

/// Decode a HEIC/HEIF file using libheif and copy its interleaved RGB plane into
/// an `image::RgbImage` (handling row stride, which is often wider than width).
#[cfg(not(target_os = "macos"))]
fn decode_heic(path: &Path) -> Result<DynamicImage> {
    use anyhow::anyhow;
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
