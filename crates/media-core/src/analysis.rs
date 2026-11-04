//! Background photo analysis: each file is read and decoded **once**, and from that one image we
//! write the thumbnail (if missing), compute a sharpness score and a perceptual hash, and — when the
//! people feature is on — detect faces and their embeddings.
//!
//! Resumable: a row in `analysis` (with `version` / `face_version`) marks a file as done, so an
//! interrupted run continues where it stopped; files on unplugged drives are simply picked up later.
//! Pausable: the caller flips a pause flag (user job running, laptop on battery) and the loop waits
//! between chunks.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use image::{DynamicImage, GrayImage};
use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db::{FileRow, FILE_COLS};
use crate::faces::{FaceEngine, REC_MODEL_ID};
use crate::thumbs::ThumbCache;
use crate::{Db, Progress, ProgressFn, StopFlag};

/// Bump when the per-file analysis changes so files are re-analysed.
pub const ANALYSIS_VERSION: i64 = 1;
/// Bump when face detection/embedding changes.
pub const FACE_VERSION: i64 = 1;
/// Longest side of the image used for analysis.
const ANALYSIS_SIZE: u32 = 1280;
const CHUNK: usize = 48;
const FACE_MIN_SCORE: f32 = 0.8;
const FACE_MIN_PX: f32 = 32.0;

// ---------------------------------------------------------------------------------------------
// Options (persisted in `settings`)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AnalysisOptions {
    /// Start analysing automatically after scans / at app start.
    pub auto: bool,
    /// People feature (face analysis). Off until the user turns it on.
    pub faces: bool,
    /// Use the GPU (DirectML) when available.
    pub gpu: bool,
    pub pause_on_battery: bool,
}

impl Default for AnalysisOptions {
    fn default() -> Self {
        Self { auto: true, faces: false, gpu: true, pause_on_battery: true }
    }
}

pub fn load_options(conn: &Connection) -> Result<AnalysisOptions> {
    let v: Option<String> = conn.query_row("SELECT value FROM settings WHERE key = 'analysis_options'", [], |r| r.get(0)).optional()?;
    Ok(v.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
}

pub fn save_options(conn: &Connection, o: &AnalysisOptions) -> Result<()> {
    conn.execute(
        "INSERT INTO settings(key, value) VALUES ('analysis_options', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![serde_json::to_string(o)?],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Image measures
// ---------------------------------------------------------------------------------------------

/// Focus measure robust to bokeh and plain skies: the image is split into an 8x8 grid, the variance
/// of the Laplacian is computed per tile, and the 95th percentile tile (about the 3rd sharpest of 64)
/// is returned — i.e. "how sharp is the sharpest part", which is what a human checks. A subject that
/// covers only a few tiles (portrait with bokeh) still counts as sharp.
pub fn sharpness(gray: &GrayImage) -> f32 {
    let (w, h) = gray.dimensions();
    if w < 16 || h < 16 {
        return 0.0;
    }
    const G: u32 = 8;
    let mut sum = [[0f64; G as usize]; G as usize];
    let mut sum2 = [[0f64; G as usize]; G as usize];
    let mut n = [[0u32; G as usize]; G as usize];
    let px = |x: u32, y: u32| gray.get_pixel(x, y)[0] as f64;
    for y in 1..h - 1 {
        let ty = ((y * G) / h) as usize;
        for x in 1..w - 1 {
            let tx = ((x * G) / w) as usize;
            let lap = px(x - 1, y) + px(x + 1, y) + px(x, y - 1) + px(x, y + 1) - 4.0 * px(x, y);
            sum[ty][tx] += lap;
            sum2[ty][tx] += lap * lap;
            n[ty][tx] += 1;
        }
    }
    let mut vars: Vec<f64> = Vec::with_capacity((G * G) as usize);
    for ty in 0..G as usize {
        for tx in 0..G as usize {
            let c = n[ty][tx].max(1) as f64;
            let mean = sum[ty][tx] / c;
            vars.push(sum2[ty][tx] / c - mean * mean);
        }
    }
    vars.sort_by(|a, b| a.total_cmp(b));
    vars[((vars.len() as f64) * 0.95) as usize - 1] as f32
}

/// 64-bit difference hash (gradient of a 9x8 grayscale thumbnail).
pub fn dhash(img: &DynamicImage) -> u64 {
    let g = img.resize_exact(9, 8, image::imageops::FilterType::Triangle).to_luma8();
    let mut h = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            h = (h << 1) | (g.get_pixel(x, y)[0] > g.get_pixel(x + 1, y)[0]) as u64;
        }
    }
    h
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Grayscale normalised to a fixed size so scores are comparable across sources.
fn analysis_gray(img: &DynamicImage) -> GrayImage {
    let g = if img.width().max(img.height()) > 1024 { img.resize(1024, 1024, image::imageops::FilterType::Triangle) } else { img.clone() };
    g.to_luma8()
}

// ---------------------------------------------------------------------------------------------
// Decoding (one read per file)
// ---------------------------------------------------------------------------------------------

/// JPEG via DCT-scaled decoding (~1.6x faster than a full decode at 12 MP); EXIF orientation applied.
fn decode_jpeg_scaled(path: &Path, orientation: Option<i64>) -> Result<DynamicImage> {
    let file = std::fs::File::open(path)?;
    let mut d = jpeg_decoder::Decoder::new(std::io::BufReader::new(file));
    let (w, h) = d.scale(960, 960)?;
    let px = d.decode()?;
    let info = d.info().ok_or_else(|| anyhow!("no jpeg info"))?;
    let img = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => DynamicImage::ImageRgb8(image::RgbImage::from_raw(w as u32, h as u32, px).ok_or_else(|| anyhow!("size"))?),
        jpeg_decoder::PixelFormat::L8 => DynamicImage::ImageLuma8(image::GrayImage::from_raw(w as u32, h as u32, px).ok_or_else(|| anyhow!("size"))?),
        other => return Err(anyhow!("unsupported jpeg pixel format {other:?}")),
    };
    let mut img = img;
    if let Some(o) = orientation.and_then(|o| image::metadata::Orientation::from_exif(o as u8)) {
        img.apply_orientation(o);
    }
    Ok(img)
}

/// The analysis image (≤ 1280 px), decoded once. HEIC/RAW/video go through the thumbnail chain
/// (Rust decoders → Windows Shell → ffmpeg).
pub fn decode(f: &FileRow) -> Result<DynamicImage> {
    let path = f.abs_path();
    if matches!(f.ext.as_str(), "jpg" | "jpeg" | "jpe" | "jfif") {
        if let Ok(img) = decode_jpeg_scaled(&path, f.orientation) {
            return Ok(img);
        }
    }
    let img = crate::thumbs::render(&path, &f.kind, &f.ext, ANALYSIS_SIZE)?;
    Ok(if img.width().max(img.height()) > ANALYSIS_SIZE { img.resize(ANALYSIS_SIZE, ANALYSIS_SIZE, image::imageops::FilterType::Triangle) } else { img })
}

// ---------------------------------------------------------------------------------------------
// Faces runtime
// ---------------------------------------------------------------------------------------------

/// Where the ONNX Runtime library and the face models live (shipped with the app).
#[derive(Clone, Debug)]
pub struct FaceRuntime {
    pub lib: PathBuf,
    pub models: PathBuf,
}

impl FaceRuntime {
    pub fn available(&self) -> bool {
        self.lib.is_file() && self.models.join(crate::faces::DET_MODEL).is_file() && self.models.join(crate::faces::REC_MODEL).is_file()
    }
}

pub fn embedding_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub fn blob_to_embedding(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

// ---------------------------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize)]
pub struct AnalysisStats {
    pub processed: u64,
    pub failed: u64,
    pub faces: u64,
    pub provider: String,
    pub cancelled: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Pending {
    /// Waiting and on a connected drive.
    pub online: i64,
    /// Waiting on drives that are not connected.
    pub offline: i64,
    pub analyzed: i64,
}

fn pending_where(faces: bool) -> String {
    format!(
        "f.missing = 0 AND f.quarantined = 0 AND (a.file_id IS NULL OR a.version < {ANALYSIS_VERSION}{})",
        if faces { format!(" OR (a.face_version < {FACE_VERSION} AND f.kind IN ('photo','raw'))") } else { String::new() }
    )
}

pub fn pending(conn: &Connection, faces: bool) -> Result<Pending> {
    let w = pending_where(faces);
    Ok(conn.query_row(
        &format!(
            "SELECT COALESCE(SUM(v.online = 1 AND ({w})), 0), COALESCE(SUM(v.online = 0 AND ({w})), 0),
                    COALESCE(SUM(a.file_id IS NOT NULL), 0)
             FROM files f JOIN volumes v ON v.id = f.volume_id LEFT JOIN analysis a ON a.file_id = f.id
             WHERE f.missing = 0 AND f.quarantined = 0"
        ),
        [],
        |r| Ok(Pending { online: r.get(0)?, offline: r.get(1)?, analyzed: r.get(2)? }),
    )?)
}

struct FaceOut {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    score: f32,
    sharpness: f32,
    embedding: Vec<f32>,
    crop: DynamicImage,
}

struct FileOut {
    id: i64,
    sharpness: Option<f32>,
    dhash: Option<i64>,
    faces: Option<Vec<FaceOut>>,
}

fn analyze_one(f: &FileRow, cache: &ThumbCache, engine: Option<&Mutex<FaceEngine>>) -> Result<FileOut> {
    // Videos: only make sure a thumbnail exists; nothing else to measure.
    if f.kind == "video" {
        cache.ensure_thumb(f)?;
        return Ok(FileOut { id: f.id, sharpness: None, dhash: None, faces: None });
    }
    let img = decode(f)?;
    if !cache.has_thumb(f) {
        cache.store_thumb(f, &img)?;
    }
    let sharp = sharpness(&analysis_gray(&img));
    let hash = dhash(&img) as i64;
    let faces = match engine {
        Some(eng) => {
            let rgb = img.to_rgb8();
            let (iw, ih) = (rgb.width() as f32, rgb.height() as f32);
            let mut eng = eng.lock().map_err(|_| anyhow!("face engine poisoned"))?;
            let found = eng.detect(&rgb, FACE_MIN_SCORE)?;
            let mut out = Vec::new();
            for face in found.iter().filter(|d| d.w.min(d.h) >= FACE_MIN_PX) {
                let embedding = eng.embed(&rgb, face)?;
                // Square crop around the face with a margin, for the People screen.
                let side = (face.w.max(face.h) * 1.4).min(iw.min(ih));
                let cx = (face.x + face.w / 2.0 - side / 2.0).clamp(0.0, iw - side);
                let cy = (face.y + face.h / 2.0 - side / 2.0).clamp(0.0, ih - side);
                let crop = img.crop_imm(cx as u32, cy as u32, side as u32, side as u32);
                let fs = sharpness(&crop.resize(128, 128, image::imageops::FilterType::Triangle).to_luma8());
                out.push(FaceOut {
                    x: (face.x / iw).clamp(0.0, 1.0),
                    y: (face.y / ih).clamp(0.0, 1.0),
                    w: (face.w / iw).clamp(0.0, 1.0),
                    h: (face.h / ih).clamp(0.0, 1.0),
                    score: face.score,
                    sharpness: fs,
                    embedding,
                    crop: crop.thumbnail(128, 128),
                });
            }
            Some(out)
        }
        None => None,
    };
    Ok(FileOut { id: f.id, sharpness: Some(sharp), dhash: Some(hash), faces })
}

fn save(conn: &mut Connection, cache: &ThumbCache, outs: Vec<Result<FileOut, (i64, String, bool)>>, stats: &mut AnalysisStats) -> Result<()> {
    let now = crate::now_ts();
    let tx = conn.transaction()?;
    let mut crops: Vec<(i64, DynamicImage)> = Vec::new();
    for o in outs {
        match o {
            Ok(o) => {
                let face_version = if o.faces.is_some() { FACE_VERSION } else { 0 };
                let prev_face_version: i64 = tx
                    .query_row("SELECT face_version FROM analysis WHERE file_id = ?", [o.id], |r| r.get(0))
                    .optional()?
                    .unwrap_or(0);
                tx.execute(
                    "INSERT INTO analysis(file_id, version, sharpness, dhash, face_count, face_version, analyzed_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(file_id) DO UPDATE SET version = excluded.version, sharpness = excluded.sharpness,
                        dhash = excluded.dhash,
                        face_count = COALESCE(excluded.face_count, analysis.face_count),
                        face_version = MAX(excluded.face_version, analysis.face_version),
                        analyzed_at = excluded.analyzed_at",
                    params![o.id, ANALYSIS_VERSION, o.sharpness, o.dhash, o.faces.as_ref().map(|v| v.len() as i64), face_version.max(prev_face_version), now],
                )?;
                if let Some(faces) = o.faces {
                    // Keep user decisions: only replace faces that were never confirmed by the user.
                    let locked: i64 = tx.query_row("SELECT COUNT(*) FROM faces WHERE file_id = ? AND locked = 1", [o.id], |r| r.get(0))?;
                    if locked == 0 {
                        tx.execute("DELETE FROM faces WHERE file_id = ?", [o.id])?;
                        for fc in faces {
                            tx.execute(
                                "INSERT INTO faces(file_id, x, y, w, h, score, sharpness, model, embedding) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                                params![o.id, fc.x, fc.y, fc.w, fc.h, fc.score, fc.sharpness, REC_MODEL_ID, embedding_to_blob(&fc.embedding)],
                            )?;
                            crops.push((tx.last_insert_rowid(), fc.crop));
                            stats.faces += 1;
                        }
                    }
                }
                stats.processed += 1;
            }
            Err((_, _, false)) => {
                // The file moved or vanished mid-run (organize/delete): leave it pending, the next
                // pass sees it at its new place.
            }
            Err((id, e, true)) => {
                log::warn!("analysis failed for {id}: {e}");
                // Record the attempt so a broken file is not retried on every pass.
                tx.execute(
                    "INSERT INTO analysis(file_id, version, analyzed_at, face_version) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(file_id) DO UPDATE SET version = excluded.version, analyzed_at = excluded.analyzed_at,
                        face_version = excluded.face_version",
                    params![id, ANALYSIS_VERSION, now, FACE_VERSION],
                )?;
                stats.failed += 1;
            }
        }
    }
    tx.commit()?;
    for (id, img) in crops {
        let _ = cache.store_face(id, &img);
    }
    Ok(())
}

/// Shared pause switch: set by the app while a user job runs or on battery power.
#[derive(Clone, Default)]
pub struct PauseFlag(Arc<AtomicBool>);

impl PauseFlag {
    pub fn set(&self, paused: bool) {
        self.0.store(paused, Ordering::Relaxed);
    }
    pub fn is_paused(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Analyse every pending file on connected drives. `engine` is `Some` when the people feature is on.
#[allow(clippy::too_many_arguments)]
pub fn run(
    db: &Db,
    cache: &ThumbCache,
    engine: Option<&Mutex<FaceEngine>>,
    threads: usize,
    stop: &StopFlag,
    pause: &PauseFlag,
    progress: ProgressFn,
) -> Result<AnalysisStats> {
    let mut conn = db.conn()?;
    crate::volumes::refresh(&conn)?;
    let faces = engine.is_some();
    let mut stats = AnalysisStats {
        provider: engine.and_then(|e| e.lock().ok().map(|e| e.provider.clone())).unwrap_or_else(|| "-".into()),
        ..Default::default()
    };
    let total = pending(&conn, faces)?.online.max(0) as u64;
    let done = AtomicU64::new(0);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads.max(1)).build()?;
    let started = Instant::now();
    let sql = format!(
        "SELECT {FILE_COLS} FROM files f JOIN volumes v ON v.id = f.volume_id LEFT JOIN analysis a ON a.file_id = f.id
         WHERE v.online = 1 AND {} ORDER BY f.taken_at DESC, f.id DESC LIMIT {CHUNK}",
        pending_where(faces)
    );
    loop {
        if stop.is_stopped() {
            stats.cancelled = true;
            break;
        }
        while pause.is_paused() && !stop.is_stopped() {
            progress(Progress::new("analysis", "paused", done.load(Ordering::Relaxed), total, ""));
            std::thread::sleep(Duration::from_millis(500));
        }
        let rows: Vec<FileRow> = {
            let mut st = conn.prepare_cached(&sql)?;
            let rows = st.query_map([], FileRow::from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        if rows.is_empty() {
            break;
        }
        let outs: Vec<Result<FileOut, (i64, String, bool)>> = pool.install(|| {
            rows.par_iter()
                .map(|f| {
                    let r = analyze_one(f, cache, engine).map_err(|e| (f.id, format!("{e:#}"), f.abs_path().exists()));
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    let eta = if d > 0 { (started.elapsed().as_secs_f64() / d as f64 * (total.saturating_sub(d)) as f64) as u64 } else { 0 };
                    progress(Progress::new("analysis", "analyze", d, total, format!("{}|{eta}", f.name)));
                    r
                })
                .collect()
        });
        save(&mut conn, cache, outs, &mut stats)?;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Luma};

    fn checker(w: u32, h: u32, cell: u32) -> GrayImage {
        ImageBuffer::from_fn(w, h, |x, y| Luma([if ((x / cell) + (y / cell)) % 2 == 0 { 20 } else { 230 }]))
    }

    #[test]
    fn sharp_vs_blurred_vs_bokeh() {
        let sharp = checker(512, 384, 8);
        let blurred = image::imageops::blur(&sharp, 6.0);
        // "Bokeh": sharp subject in one region, the rest heavily blurred.
        let mut bokeh = blurred.clone();
        for y in 140..260 {
            for x in 200..330 {
                bokeh.put_pixel(x, y, *sharp.get_pixel(x, y));
            }
        }
        let (s, b, k) = (sharpness(&sharp), sharpness(&blurred), sharpness(&bokeh));
        assert!(s > 20.0 * b, "sharp {s} vs blurred {b}");
        assert!(k > 0.3 * s, "bokeh portrait must not look blurry: {k} vs sharp {s}");
    }

    #[test]
    fn dhash_stable_under_resize() {
        let img = DynamicImage::ImageLuma8(ImageBuffer::from_fn(640, 480, |x, y| Luma([((x * 7 + y * 3) % 256) as u8])));
        let small = img.resize(320, 240, image::imageops::FilterType::Triangle);
        assert!(hamming(dhash(&img), dhash(&small)) <= 4);
        let other = DynamicImage::ImageLuma8(ImageBuffer::from_fn(640, 480, |x, y| Luma([((y * 11 + x) % 256) as u8])));
        assert!(hamming(dhash(&img), dhash(&other)) > 10);
    }

    #[test]
    fn embedding_blob_roundtrip() {
        let v = vec![0.5f32, -1.25, 3.0];
        assert_eq!(blob_to_embedding(&embedding_to_blob(&v)), v);
    }
}
