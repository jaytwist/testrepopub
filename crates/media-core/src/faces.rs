//! Face detection (OpenCV Zoo YuNet, MIT) and face embeddings (OpenCV Zoo SFace, Apache-2.0),
//! run through ONNX Runtime loaded at run time from the library we ship (`ort` load-dynamic).
//!
//! Pipeline, matching OpenCV's FaceDetectorYN / FaceRecognizerSF:
//!   image -> letterbox to 640x640 BGR -> YuNet -> decode (strides 8/16/32) -> NMS
//!   face  -> 5-point similarity alignment to the ArcFace 112x112 template -> RGB -> SFace -> L2-normalized 128-d

use std::path::Path;
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use image::RgbImage;
use ort::session::Session;
use ort::value::Tensor;

pub const DET_MODEL: &str = "face_detection_yunet_2023mar.onnx";
pub const REC_MODEL: &str = "face_recognition_sface_2021dec.onnx";
/// Identifies the embedding space; stored per face so a future model swap can coexist.
pub const REC_MODEL_ID: &str = "sface-2021dec";
pub const EMBED_DIM: usize = 128;

const DET_SIZE: u32 = 640;
const NMS_IOU: f32 = 0.3;

/// ArcFace reference landmarks for a 112x112 crop (same order as YuNet: image-left eye,
/// image-right eye, nose tip, image-left mouth corner, image-right mouth corner).
const ARCFACE_REF: [[f32; 2]; 5] = [[38.2946, 51.6963], [73.5318, 51.5014], [56.0252, 71.7366], [41.5493, 92.3655], [70.7299, 92.2041]];

#[derive(Clone, Debug)]
pub struct Face {
    /// Box in input-image pixels.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub score: f32,
    pub kps: [[f32; 2]; 5],
}

/// The ONNX Runtime environment, created once it loads successfully (a failure is retried on the
/// next attempt instead of being remembered until the app restarts).
static ENV: Mutex<Option<&'static ort::environment::Environment>> = Mutex::new(None);

/// Make Windows find the DLLs that onnxruntime.dll depends on (DirectML.dll) in the same `ai`
/// folder: by default it only looks next to the .exe and in the system folders, where an older
/// DirectML.dll may be found instead — or none at all.
pub fn prepare_dll_dir(dir: &Path) {
    #[cfg(windows)]
    {
        use windows::core::HSTRING;
        use windows::Win32::System::LibraryLoader::SetDllDirectoryW;
        // SAFETY: plain Win32 call with a valid, NUL-terminated path.
        let _ = unsafe { SetDllDirectoryW(&HSTRING::from(dir.as_os_str())) };
        if let Some(cur) = std::env::var_os("PATH") {
            let mut paths = vec![dir.to_path_buf()];
            paths.extend(std::env::split_paths(&cur).filter(|p| p != dir));
            if let Ok(joined) = std::env::join_paths(paths) {
                // SAFETY: only called right before the face engine loads, from the analysis thread.
                unsafe { std::env::set_var("PATH", joined) };
            }
        }
    }
    #[cfg(not(windows))]
    let _ = dir;
}

fn environment(lib: &Path) -> Result<&'static ort::environment::Environment> {
    let mut slot = ENV.lock().map_err(|_| anyhow!("ONNX Runtime lock poisoned"))?;
    if let Some(env) = *slot {
        return Ok(env);
    }
    if let Some(dir) = lib.parent() {
        prepare_dll_dir(dir);
    }
    let env = ort::init_from(lib)
        .map_err(|e| anyhow!("ONNX Runtime을 불러오지 못했습니다 ({}): {e}", lib.display()))?
        .with_name("media-organizer")
        .build()
        .map_err(|e| anyhow!("ONNX Runtime 초기화 실패: {e}"))?;
    let env: &'static ort::environment::Environment = Box::leak(Box::new(env));
    *slot = Some(env);
    Ok(env)
}

pub struct FaceEngine {
    det: Session,
    rec: Session,
    /// "DirectML" or "CPU" — for the settings/diagnostics screen.
    pub provider: String,
}

impl FaceEngine {
    /// `lib`: path to onnxruntime.dll / libonnxruntime.so. `models`: folder with the two .onnx files.
    pub fn load(lib: &Path, models: &Path, use_gpu: bool, threads: usize) -> Result<Self> {
        let env = environment(lib)?;
        let build = |file: &str, gpu: bool| -> Result<Session> {
            let e = |e: ort::Error<ort::session::builder::SessionBuilder>| anyhow!("{e}");
            let mut b = Session::builder(env)?.with_intra_threads(threads.max(1)).map_err(e)?;
            #[cfg(windows)]
            if gpu {
                b = b.with_execution_providers([ort::ep::DirectML::default().build().error_on_failure()]).map_err(e)?;
            }
            #[cfg(not(windows))]
            let _ = gpu;
            Ok(b.commit_from_file(models.join(file)).with_context(|| format!("모델을 불러오지 못했습니다: {file}"))?)
        };
        // GPU first (Windows), CPU if the GPU path fails for any reason.
        if use_gpu && cfg!(windows) {
            if let (Ok(det), Ok(rec)) = (build(DET_MODEL, true), build(REC_MODEL, true)) {
                return Ok(Self { det, rec, provider: "DirectML".into() });
            }
        }
        Ok(Self { det: build(DET_MODEL, false)?, rec: build(REC_MODEL, false)?, provider: "CPU".into() })
    }

    /// Detect faces with score >= `min_score`.
    pub fn detect(&mut self, img: &RgbImage, min_score: f32) -> Result<Vec<Face>> {
        let (w, h) = img.dimensions();
        let scale = DET_SIZE as f32 / w.max(h) as f32;
        let (sw, sh) = (((w as f32 * scale).round() as u32).max(1), ((h as f32 * scale).round() as u32).max(1));
        let resized = image::imageops::resize(img, sw, sh, image::imageops::FilterType::Triangle);
        // Letterbox into 640x640 (top-left), BGR, 0..255, NCHW.
        let n = (DET_SIZE * DET_SIZE) as usize;
        let mut data = vec![0f32; 3 * n];
        for (x, y, p) in resized.enumerate_pixels() {
            let i = (y * DET_SIZE + x) as usize;
            data[i] = p[2] as f32;
            data[n + i] = p[1] as f32;
            data[2 * n + i] = p[0] as f32;
        }
        let input = Tensor::from_array(([1usize, 3, DET_SIZE as usize, DET_SIZE as usize], data))?;
        let out = self.det.run(ort::inputs![input])?;
        let mut faces = Vec::new();
        for stride in [8u32, 16, 32] {
            let get = |name: &str| -> Result<Vec<f32>> {
                let (_, v) = out[format!("{name}_{stride}").as_str()].try_extract_tensor::<f32>()?;
                Ok(v.to_vec())
            };
            let (cls, obj, bbox, kps) = (get("cls")?, get("obj")?, get("bbox")?, get("kps")?);
            let cols = DET_SIZE / stride;
            let s = stride as f32;
            for (idx, (c, o)) in cls.iter().zip(obj.iter()).enumerate() {
                let score = (c.clamp(0.0, 1.0) * o.clamp(0.0, 1.0)).sqrt();
                if score < min_score {
                    continue;
                }
                let (col, row) = ((idx as u32 % cols) as f32, (idx as u32 / cols) as f32);
                let b = &bbox[idx * 4..idx * 4 + 4];
                let (cx, cy) = ((col + b[0]) * s, (row + b[1]) * s);
                let (bw, bh) = (b[2].exp() * s, b[3].exp() * s);
                let k = &kps[idx * 10..idx * 10 + 10];
                let mut pts = [[0f32; 2]; 5];
                for (j, p) in pts.iter_mut().enumerate() {
                    *p = [(k[2 * j] + col) * s / scale, (k[2 * j + 1] + row) * s / scale];
                }
                faces.push(Face { x: (cx - bw / 2.0) / scale, y: (cy - bh / 2.0) / scale, w: bw / scale, h: bh / scale, score, kps: pts });
            }
        }
        Ok(nms(faces, NMS_IOU))
    }

    /// 128-d L2-normalized identity embedding for one detected face.
    pub fn embed(&mut self, img: &RgbImage, face: &Face) -> Result<Vec<f32>> {
        let crop = align_crop(img, &face.kps);
        let n = 112 * 112;
        let mut data = vec![0f32; 3 * n];
        for (x, y, p) in crop.enumerate_pixels() {
            let i = (y * 112 + x) as usize;
            data[i] = p[0] as f32;
            data[n + i] = p[1] as f32;
            data[2 * n + i] = p[2] as f32;
        }
        let input = Tensor::from_array(([1usize, 3, 112, 112], data))?;
        let out = self.rec.run(ort::inputs![input])?;
        let (_, v) = out[0].try_extract_tensor::<f32>()?;
        Ok(l2_normalize(v))
    }
}

fn iou(a: &Face, b: &Face) -> f32 {
    let (x1, y1) = (a.x.max(b.x), a.y.max(b.y));
    let (x2, y2) = ((a.x + a.w).min(b.x + b.w), (a.y + a.h).min(b.y + b.h));
    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    inter / (a.w * a.h + b.w * b.h - inter).max(1e-6)
}

fn nms(mut faces: Vec<Face>, thr: f32) -> Vec<Face> {
    faces.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut keep: Vec<Face> = Vec::new();
    for f in faces {
        if keep.iter().all(|k| iou(k, &f) <= thr) {
            keep.push(f);
        }
    }
    keep
}

pub fn l2_normalize(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    v.iter().map(|x| x / n).collect()
}

/// Cosine similarity of two L2-normalized embeddings.
pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Least-squares similarity transform (rotation + uniform scale + translation) mapping `src` to
/// `dst`. Returns (a, b, tx, ty) for x' = a*x - b*y + tx, y' = b*x + a*y + ty.
pub fn similarity_transform(src: &[[f32; 2]; 5], dst: &[[f32; 2]; 5]) -> (f32, f32, f32, f32) {
    let mean = |p: &[[f32; 2]; 5]| {
        let (sx, sy) = p.iter().fold((0.0, 0.0), |acc, q| (acc.0 + q[0], acc.1 + q[1]));
        (sx / 5.0, sy / 5.0)
    };
    let (msx, msy) = mean(src);
    let (mdx, mdy) = mean(dst);
    let (mut num_a, mut num_b, mut den) = (0f32, 0f32, 0f32);
    for (s, d) in src.iter().zip(dst) {
        let (sx, sy, dx, dy) = (s[0] - msx, s[1] - msy, d[0] - mdx, d[1] - mdy);
        num_a += sx * dx + sy * dy;
        num_b += sx * dy - sy * dx;
        den += sx * sx + sy * sy;
    }
    let (a, b) = (num_a / den.max(1e-6), num_b / den.max(1e-6));
    (a, b, mdx - (a * msx - b * msy), mdy - (b * msx + a * msy))
}

/// Warp the face to the 112x112 ArcFace template (bilinear sampling).
pub fn align_crop(img: &RgbImage, kps: &[[f32; 2]; 5]) -> RgbImage {
    let (a, b, tx, ty) = similarity_transform(kps, &ARCFACE_REF);
    // Inverse of [a -b; b a] is [a b; -b a] / (a^2 + b^2).
    let det = (a * a + b * b).max(1e-12);
    let (w, h) = img.dimensions();
    let sample = |x: f32, y: f32| -> [f32; 3] {
        if x < 0.0 || y < 0.0 || x > (w - 1) as f32 || y > (h - 1) as f32 {
            return [0.0; 3];
        }
        let (x0, y0) = (x.floor() as u32, y.floor() as u32);
        let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let mut out = [0f32; 3];
        for (c, o) in out.iter_mut().enumerate() {
            let p = |xx: u32, yy: u32| img.get_pixel(xx, yy)[c] as f32;
            *o = p(x0, y0) * (1.0 - fx) * (1.0 - fy) + p(x1, y0) * fx * (1.0 - fy) + p(x0, y1) * (1.0 - fx) * fy + p(x1, y1) * fx * fy;
        }
        out
    };
    RgbImage::from_fn(112, 112, |u, v| {
        let (dx, dy) = (u as f32 - tx, v as f32 - ty);
        let (sx, sy) = ((a * dx + b * dy) / det, (-b * dx + a * dy) / det);
        let p = sample(sx, sy);
        image::Rgb([p[0].round() as u8, p[1].round() as u8, p[2].round() as u8])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_recovers_known_similarity() {
        // dst = 2*R(30deg)*src + (5, -3)
        let (c, s) = (2.0 * 30f32.to_radians().cos(), 2.0 * 30f32.to_radians().sin());
        let src = [[1.0, 2.0], [4.0, 2.5], [2.5, 4.0], [1.5, 6.0], [3.5, 6.0]];
        let mut dst = [[0f32; 2]; 5];
        for (d, p) in dst.iter_mut().zip(src.iter()) {
            *d = [c * p[0] - s * p[1] + 5.0, s * p[0] + c * p[1] - 3.0];
        }
        let (a, b, tx, ty) = similarity_transform(&src, &dst);
        assert!((a - c).abs() < 1e-4 && (b - s).abs() < 1e-4 && (tx - 5.0).abs() < 1e-3 && (ty + 3.0).abs() < 1e-3);
    }

    #[test]
    fn nms_keeps_best_of_overlapping() {
        let f = |x: f32, s: f32| Face { x, y: 0.0, w: 10.0, h: 10.0, score: s, kps: [[0.0; 2]; 5] };
        let kept = nms(vec![f(0.0, 0.8), f(1.0, 0.9), f(50.0, 0.7)], 0.3);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].score, 0.9);
    }
}
