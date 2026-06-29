//! Local face detection + embedding.
//!
//! YuNet finds faces and 5 landmarks; we align each face to the standard
//! ArcFace template (a similarity transform from the landmarks), then SFace turns
//! it into a 128-d embedding. All on-device, no network. The spike proved this
//! path; the improvements here over the spike are **letterbox** input (keeps
//! aspect ratio → better recall) and **5-point alignment** (much cleaner
//! embeddings than a raw crop).
//!
//! Models (bundled, permissively licensed): YuNet (MIT), SFace (Apache-2.0).

use anyhow::Result;
use image::{imageops::FilterType, Rgb, RgbImage};
use ndarray::Array4;
use ort::execution_providers::CoreMLExecutionProvider;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;
use std::path::Path;

use crate::db::DetectedFace;

const DET: u32 = 640; // YuNet square input (letterboxed)
const SCORE_THR: f32 = 0.6;
const NMS_IOU: f32 = 0.4;
const STRIDES: [u32; 3] = [8, 16, 32];

/// ArcFace 5-point template for a 112×112 crop, in order: left eye, right eye,
/// nose, left mouth corner, right mouth corner.
const TEMPLATE: [[f32; 2]; 5] = [
    [38.2946, 51.6963],
    [73.5318, 51.5014],
    [56.0252, 71.7366],
    [41.5493, 92.3655],
    [70.7299, 92.2041],
];

struct Candidate {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    score: f32,
    /// Landmarks in original-image pixels, reordered to match TEMPLATE.
    lm: [[f32; 2]; 5],
}

pub struct FaceModels {
    yunet: Session,
    sface: Session,
}

fn build(path: &Path) -> Result<Session> {
    Ok(Session::builder()?
        .with_execution_providers([CoreMLExecutionProvider::default().build()])?
        .with_optimization_level(GraphOptimizationLevel::Level3)?
        .commit_from_file(path)?)
}

/// RgbImage → NCHW f32 in BGR, 0-255 (OpenCV-style; both models were trained so).
fn to_nchw_bgr(img: &RgbImage) -> Array4<f32> {
    let (w, h) = img.dimensions();
    let mut a = Array4::<f32>::zeros((1, 3, h as usize, w as usize));
    for y in 0..h as usize {
        for x in 0..w as usize {
            let p = img.get_pixel(x as u32, y as u32).0;
            a[[0, 0, y, x]] = p[2] as f32;
            a[[0, 1, y, x]] = p[1] as f32;
            a[[0, 2, y, x]] = p[0] as f32;
        }
    }
    a
}

fn iou(a: &Candidate, b: &Candidate) -> f32 {
    let xx1 = a.x1.max(b.x1);
    let yy1 = a.y1.max(b.y1);
    let xx2 = a.x2.min(b.x2);
    let yy2 = a.y2.min(b.y2);
    let inter = (xx2 - xx1).max(0.0) * (yy2 - yy1).max(0.0);
    let ua = (a.x2 - a.x1) * (a.y2 - a.y1) + (b.x2 - b.x1) * (b.y2 - b.y1) - inter;
    if ua <= 0.0 { 0.0 } else { inter / ua }
}

fn nms(mut v: Vec<Candidate>) -> Vec<Candidate> {
    v.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    let mut keep: Vec<Candidate> = Vec::new();
    for c in v {
        if keep.iter().all(|k| iou(k, &c) < NMS_IOU) {
            keep.push(c);
        }
    }
    keep
}

/// Bilinear sample of an RgbImage at floating-point coordinates (clamped).
fn sample(img: &RgbImage, fx: f32, fy: f32) -> Rgb<u8> {
    let (w, h) = img.dimensions();
    let x = fx.clamp(0.0, (w - 1) as f32);
    let y = fy.clamp(0.0, (h - 1) as f32);
    let x0 = x.floor() as u32;
    let y0 = y.floor() as u32;
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);
    let dx = x - x0 as f32;
    let dy = y - y0 as f32;
    let mut out = [0u8; 3];
    for c in 0..3 {
        let p00 = img.get_pixel(x0, y0).0[c] as f32;
        let p10 = img.get_pixel(x1, y0).0[c] as f32;
        let p01 = img.get_pixel(x0, y1).0[c] as f32;
        let p11 = img.get_pixel(x1, y1).0[c] as f32;
        let top = p00 * (1.0 - dx) + p10 * dx;
        let bot = p01 * (1.0 - dx) + p11 * dx;
        out[c] = (top * (1.0 - dy) + bot * dy).round() as u8;
    }
    Rgb(out)
}

/// Warp the face to a 112×112 aligned crop using a similarity transform that
/// best maps its 5 landmarks onto the ArcFace template (closed-form 2D Umeyama).
fn align(img: &RgbImage, lm: &[[f32; 2]; 5]) -> RgbImage {
    // Centroids.
    let (mut msx, mut msy, mut mdx, mut mdy) = (0.0f32, 0.0, 0.0, 0.0);
    for i in 0..5 {
        msx += lm[i][0];
        msy += lm[i][1];
        mdx += TEMPLATE[i][0];
        mdy += TEMPLATE[i][1];
    }
    msx /= 5.0;
    msy /= 5.0;
    mdx /= 5.0;
    mdy /= 5.0;

    // Centered cross/dot sums and source variance.
    let (mut num, mut den, mut norm_s) = (0.0f32, 0.0, 0.0);
    for i in 0..5 {
        let sx = lm[i][0] - msx;
        let sy = lm[i][1] - msy;
        let dx = TEMPLATE[i][0] - mdx;
        let dy = TEMPLATE[i][1] - mdy;
        num += sx * dy - sy * dx;
        den += sx * dx + sy * dy;
        norm_s += sx * sx + sy * sy;
    }
    let scale = (num * num + den * den).sqrt() / norm_s.max(1e-9);
    let theta = num.atan2(den);
    // Forward transform M = scale*R, t (maps src landmark -> template).
    let a = scale * theta.cos();
    let b = scale * theta.sin();
    let tx = mdx - (a * msx - b * msy);
    let ty = mdy - (b * msx + a * msy);

    // Inverse (template pixel -> source pixel) for sampling.
    let det = a * a + b * b;
    let mut out = RgbImage::new(112, 112);
    for oy in 0..112u32 {
        for ox in 0..112u32 {
            let px = ox as f32 - tx;
            let py = oy as f32 - ty;
            let sx = (a * px + b * py) / det;
            let sy = (-b * px + a * py) / det;
            out.put_pixel(ox, oy, sample(img, sx, sy));
        }
    }
    out
}

/// Edge length of a cached face-crop, in pixels (crisp on retina tiles).
const FACE_CROP: u32 = 256;

/// Where a face's cached cover crop lives, bucketed by id like thumbnails.
pub fn face_crop_path(faces_dir: &Path, face_id: i64) -> std::path::PathBuf {
    faces_dir.join((face_id / 1000).to_string()).join(format!("{face_id}.jpg"))
}

/// Crop a square, margined face from a full-resolution image and return JPEG
/// bytes at FACE_CROP px. `bbox` is normalized (0-1). Cropping from the original
/// (not the 256px thumbnail) keeps cover faces sharp even when small in frame.
pub fn crop_face_jpeg(img: &RgbImage, bbox: (f32, f32, f32, f32)) -> Result<Vec<u8>> {
    let (w, h) = img.dimensions();
    let (nx1, ny1, nx2, ny2) = bbox;
    let cx = (nx1 + nx2) / 2.0 * w as f32;
    let cy = (ny1 + ny2) / 2.0 * h as f32;
    let side = ((nx2 - nx1) * w as f32).max((ny2 - ny1) * h as f32) * 1.4; // 40% margin
    let half = side / 2.0;
    let x0 = (cx - half).max(0.0) as u32;
    let y0 = (cy - half).max(0.0) as u32;
    let x1 = (cx + half).min(w as f32) as u32;
    let y1 = (cy + half).min(h as f32) as u32;
    let cw = x1.saturating_sub(x0).max(1);
    let ch = y1.saturating_sub(y0).max(1);
    let crop = image::imageops::crop_imm(img, x0, y0, cw, ch).to_image();
    let crop = image::imageops::resize(&crop, FACE_CROP, FACE_CROP, FilterType::Triangle);
    let mut buf = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::Cursor::new(&mut buf), 85);
    enc.encode(crop.as_raw(), crop.width(), crop.height(), image::ExtendedColorType::Rgb8)?;
    Ok(buf)
}

impl FaceModels {
    pub fn load(yunet_path: &Path, sface_path: &Path) -> Result<Self> {
        Ok(Self {
            yunet: build(yunet_path)?,
            sface: build(sface_path)?,
        })
    }

    /// Detect faces in an image and return each with its embedding. The stored
    /// bounding box is normalized to 0-1 (relative to the image) so a face crop
    /// can be taken from the cached thumbnail without knowing the original size.
    pub fn process(&mut self, img: &RgbImage) -> Result<Vec<DetectedFace>> {
        let (ow, oh) = img.dimensions();
        let (ow, oh) = (ow as f32, oh as f32);
        let cands = self.detect(img)?;
        let mut out = Vec::with_capacity(cands.len());
        for c in cands {
            let aligned = align(img, &c.lm); // alignment uses pixel landmarks
            let embedding = self.embed(&aligned)?;
            out.push(DetectedFace {
                x1: c.x1 / ow,
                y1: c.y1 / oh,
                x2: c.x2 / ow,
                y2: c.y2 / oh,
                score: c.score,
                embedding,
            });
        }
        Ok(out)
    }

    fn detect(&mut self, img: &RgbImage) -> Result<Vec<Candidate>> {
        let (ow, oh) = img.dimensions();
        // Letterbox into a DET×DET square (preserve aspect; pad with black).
        let scale = DET as f32 / ow.max(oh) as f32;
        let nw = ((ow as f32 * scale).round() as u32).max(1).min(DET);
        let nh = ((oh as f32 * scale).round() as u32).max(1).min(DET);
        let resized = image::imageops::resize(img, nw, nh, FilterType::Triangle);
        let mut canvas = RgbImage::from_pixel(DET, DET, Rgb([0, 0, 0]));
        let padx = (DET - nw) / 2;
        let pady = (DET - nh) / 2;
        image::imageops::overlay(&mut canvas, &resized, padx as i64, pady as i64);

        let input = to_nchw_bgr(&canvas);
        let outputs = self
            .yunet
            .run(ort::inputs!["input" => Tensor::from_array(input)?])?;

        // Map a coordinate in canvas space back to the original image.
        let back = |cx: f32, cy: f32| -> (f32, f32) {
            ((cx - padx as f32) / scale, (cy - pady as f32) / scale)
        };

        let mut cands = Vec::new();
        for s in STRIDES {
            let fw = (DET / s) as usize;
            let cls = outputs[format!("cls_{s}")].try_extract_tensor::<f32>()?.1;
            let obj = outputs[format!("obj_{s}")].try_extract_tensor::<f32>()?.1;
            let bbox = outputs[format!("bbox_{s}")].try_extract_tensor::<f32>()?.1;
            let kps = outputs[format!("kps_{s}")].try_extract_tensor::<f32>()?.1;
            let n = cls.len();
            for idx in 0..n {
                let score = cls[idx] * obj[idx];
                if score < SCORE_THR {
                    continue;
                }
                let r = (idx / fw) as f32;
                let c = (idx % fw) as f32;
                let sf = s as f32;
                let cx = (c + bbox[idx * 4]) * sf;
                let cy = (r + bbox[idx * 4 + 1]) * sf;
                let bw = bbox[idx * 4 + 2].exp() * sf;
                let bh = bbox[idx * 4 + 3].exp() * sf;
                let (x1, y1) = back(cx - bw / 2.0, cy - bh / 2.0);
                let (x2, y2) = back(cx + bw / 2.0, cy + bh / 2.0);

                // YuNet landmark order: right eye, left eye, nose, right mouth,
                // left mouth. Reorder to TEMPLATE order [LE, RE, N, LM, RM].
                let raw: [[f32; 2]; 5] = std::array::from_fn(|k| {
                    let (lx, ly) = back((c + kps[idx * 10 + k * 2]) * sf, (r + kps[idx * 10 + k * 2 + 1]) * sf);
                    [lx, ly]
                });
                let lm = [raw[1], raw[0], raw[2], raw[4], raw[3]];

                cands.push(Candidate { x1, y1, x2, y2, score, lm });
            }
        }
        Ok(nms(cands))
    }

    fn embed(&mut self, face112: &RgbImage) -> Result<Vec<f32>> {
        let input = to_nchw_bgr(face112);
        let outputs = self
            .sface
            .run(ort::inputs!["data" => Tensor::from_array(input)?])?;
        let mut v: Vec<f32> = outputs["fc1"].try_extract_tensor::<f32>()?.1.to_vec();
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        for x in &mut v {
            *x /= norm;
        }
        Ok(v)
    }
}
