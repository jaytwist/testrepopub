//! Thumbnails and previews, cached on disk so unplugged drives still show their photos.
//!
//! Decode chain (first that works wins):
//!   1. pure-Rust `image` crate (JPEG/PNG/WebP/GIF/BMP/TIFF), EXIF orientation applied
//!   2. Windows Shell thumbnailer (HEIC, RAW, videos — whatever codecs Windows has)
//!   3. bundled ffmpeg (videos, HEIC on PCs without the HEIF extension)

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use image::{DynamicImage, ImageDecoder};
use rayon::prelude::*;
use serde::Serialize;

use crate::db::{FileRow, FILE_COLS};
use crate::media::{is_browser_image, is_rust_decodable};
use crate::{Db, Progress, ProgressFn, StopFlag};

pub const THUMB_SIZE: u32 = 360;
pub const PREVIEW_SIZE: u32 = 2048;

/// Prevent a console window from flashing for every ffmpeg call on Windows.
pub fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

#[derive(Clone)]
pub struct ThumbCache {
    dir: PathBuf,
}

impl ThumbCache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn keyed(&self, sub: &str, f: &FileRow, ext: &str) -> PathBuf {
        // Content identity: a changed file (size/mtime) gets a new cache entry.
        self.dir.join(sub).join(format!("{:02x}", f.id % 256)).join(format!("{}-{}-{}.{ext}", f.id, f.size, f.mtime))
    }

    pub fn thumb_path(&self, f: &FileRow) -> PathBuf {
        self.keyed("thumbs", f, "jpg")
    }

    pub fn has_thumb(&self, f: &FileRow) -> bool {
        self.thumb_path(f).is_file()
    }

    /// Thumbnail JPEG path, generating it if needed (requires the drive to be online).
    pub fn ensure_thumb(&self, f: &FileRow) -> Result<PathBuf> {
        let out = self.thumb_path(f);
        if out.is_file() {
            return Ok(out);
        }
        if !f.online {
            bail!("offline");
        }
        let img = render(&f.abs_path(), &f.kind, &f.ext, THUMB_SIZE)?;
        save_jpeg(&img.thumbnail(THUMB_SIZE, THUMB_SIZE), &out, 80)?;
        Ok(out)
    }

    /// A file the webview can display: the original for JPEG/PNG/WebP/GIF, otherwise a cached
    /// 2048px JPEG rendering (HEIC, RAW, TIFF). Offline files fall back to the thumbnail.
    pub fn ensure_preview(&self, f: &FileRow) -> Result<PathBuf> {
        if f.kind == "video" {
            return if f.online { Ok(f.abs_path()) } else { self.ensure_thumb(f) };
        }
        // Chromium/WebView2 applies EXIF orientation itself (CSS image-orientation: from-image).
        if f.online && is_browser_image(&f.ext) {
            return Ok(f.abs_path());
        }
        let out = self.keyed("previews", f, "jpg");
        if out.is_file() {
            return Ok(out);
        }
        if !f.online {
            return self.ensure_thumb(f);
        }
        let img = render(&f.abs_path(), &f.kind, &f.ext, PREVIEW_SIZE)?;
        let img = if img.width() > PREVIEW_SIZE || img.height() > PREVIEW_SIZE { img.resize(PREVIEW_SIZE, PREVIEW_SIZE, image::imageops::FilterType::Triangle) } else { img };
        save_jpeg(&img, &out, 88)?;
        Ok(out)
    }

    /// H.264/AAC 720p transcode for videos the webview cannot play (HEVC, AVI, WMV, MTS...).
    pub fn ensure_video_preview(&self, f: &FileRow) -> Result<PathBuf> {
        for ext in ["mp4", "webm"] {
            let p = self.keyed("videos", f, ext);
            if p.is_file() {
                return Ok(p);
            }
        }
        if !f.online {
            bail!("offline");
        }
        // Encoder chain: libx264 (GPL ffmpeg builds) -> Media Foundation H.264 (Windows, LGPL builds)
        // -> VP9/WebM (libvpx is BSD, present in every build). WebView2 plays all three.
        let attempts: [(&str, &[&str]); 3] = [
            ("mp4", &["-c:v", "libx264", "-preset", "veryfast", "-crf", "26", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "128k", "-movflags", "+faststart"]),
            ("mp4", &["-c:v", "h264_mf", "-b:v", "3M", "-pix_fmt", "nv12", "-c:a", "aac", "-b:a", "128k", "-movflags", "+faststart"]),
            ("webm", &["-c:v", "libvpx-vp9", "-deadline", "realtime", "-cpu-used", "8", "-b:v", "2M", "-c:a", "libopus", "-b:a", "96k"]),
        ];
        let mut last_err = String::new();
        for (ext, args) in attempts {
            let out = self.keyed("videos", f, ext);
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p)?;
            }
            let tmp = out.with_extension(format!("part.{ext}"));
            let mut cmd = Command::new("ffmpeg");
            cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
                .arg(f.abs_path())
                .args(["-vf", "scale='min(1280,iw)':-2"])
                .args(args)
                .arg(&tmp)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            hide_console(&mut cmd);
            let o = cmd.output().context("ffmpeg를 찾을 수 없습니다")?;
            if o.status.success() {
                std::fs::rename(&tmp, &out)?;
                return Ok(out);
            }
            let _ = std::fs::remove_file(&tmp);
            last_err = String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("").to_string();
        }
        bail!("ffmpeg 변환 실패: {last_err}")
    }

    pub fn size_on_disk(&self) -> u64 {
        walkdir::WalkDir::new(&self.dir).into_iter().flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
    }
}

fn save_jpeg(img: &DynamicImage, out: &Path, quality: u8) -> Result<()> {
    if let Some(p) = out.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = out.with_extension("part");
    {
        let file = std::fs::File::create(&tmp)?;
        let mut w = std::io::BufWriter::new(file);
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut w, quality);
        img.to_rgb8().write_with_encoder(enc)?;
    }
    std::fs::rename(&tmp, out)?;
    Ok(())
}

fn decode_rust(path: &Path) -> Result<DynamicImage> {
    let mut dec = image::ImageReader::open(path)?.with_guessed_format()?.into_decoder()?;
    let orientation = dec.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(dec)?;
    img.apply_orientation(orientation);
    Ok(img)
}

/// Render any supported media file to an image no larger than ~`max` px.
pub fn render(path: &Path, kind: &str, ext: &str, max: u32) -> Result<DynamicImage> {
    let mut errs = Vec::new();
    if kind != "video" && is_rust_decodable(ext) {
        match decode_rust(path) {
            Ok(i) => return Ok(i),
            Err(e) => errs.push(format!("image: {e}")),
        }
    }
    #[cfg(windows)]
    match shell::thumbnail(path, max) {
        Ok(i) => return Ok(i),
        Err(e) => errs.push(format!("shell: {e}")),
    }
    match ffmpeg_frame(path, kind == "video", max) {
        Ok(i) => return Ok(i),
        Err(e) => errs.push(format!("ffmpeg: {e}")),
    }
    Err(anyhow!("미리보기를 만들 수 없습니다 ({})", errs.join("; ")))
}

/// Grab one frame (videos: ~1 s in; stills such as HEIC: the image itself) through ffmpeg.
pub fn ffmpeg_frame(path: &Path, is_video: bool, max: u32) -> Result<DynamicImage> {
    let run = |seek: Option<&str>| -> Result<Vec<u8>> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error"]);
        if let Some(s) = seek {
            cmd.args(["-ss", s]);
        }
        cmd.arg("-i")
            .arg(path)
            .args(["-frames:v", "1", "-vf", &format!("scale='min({max},iw)':-2"), "-f", "image2pipe", "-vcodec", "mjpeg", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        hide_console(&mut cmd);
        let mut child = cmd.spawn().context("ffmpeg를 찾을 수 없습니다")?;
        let mut buf = Vec::new();
        child.stdout.take().unwrap().read_to_end(&mut buf)?;
        child.wait()?;
        if buf.is_empty() {
            bail!("no frame");
        }
        Ok(buf)
    };
    let bytes = if is_video { run(Some("1")).or_else(|_| run(None))? } else { run(None)? };
    Ok(image::load_from_memory(&bytes)?)
}

#[cfg(windows)]
mod shell {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use anyhow::{bail, Result};
    use image::{DynamicImage, RgbaImage};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        HGDIOBJ,
    };
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_RESIZETOFIT, SIIGBF_THUMBNAILONLY};

    /// Ask Windows for a thumbnail using whatever codecs/extensions are installed (HEIF, RAW, video).
    pub fn thumbnail(path: &Path, max: u32) -> Result<DynamicImage> {
        unsafe {
            // Each rayon worker initializes COM once; repeated calls return S_FALSE, which is fine.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
            let factory: IShellItemImageFactory = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)?;
            let hbmp = factory.GetImage(SIZE { cx: max as i32, cy: max as i32 }, SIIGBF_RESIZETOFIT | SIIGBF_THUMBNAILONLY)?;
            let gdi: HGDIOBJ = hbmp.into();
            let mut bm = BITMAP::default();
            let got = GetObjectW(gdi, std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut BITMAP as *mut c_void));
            if got == 0 || bm.bmWidth <= 0 || bm.bmHeight == 0 {
                let _ = DeleteObject(gdi);
                bail!("empty bitmap");
            }
            let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
            let mut bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down rows
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut buf = vec![0u8; (w * h * 4) as usize];
            let hdc = CreateCompatibleDC(None);
            let lines = GetDIBits(hdc, hbmp, 0, h as u32, Some(buf.as_mut_ptr() as *mut c_void), &mut bi, DIB_RGB_COLORS);
            let _ = DeleteDC(hdc);
            let _ = DeleteObject(gdi);
            if lines == 0 {
                bail!("GetDIBits failed");
            }
            for px in buf.chunks_exact_mut(4) {
                px.swap(0, 2); // BGRA -> RGBA
                px[3] = 255;
            }
            match RgbaImage::from_raw(w as u32, h as u32, buf) {
                Some(img) => Ok(DynamicImage::ImageRgba8(img)),
                None => bail!("bad bitmap size"),
            }
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct WarmStats {
    pub generated: u64,
    pub failed: u64,
    pub total: u64,
}

/// Pre-generate thumbnails for every online file that lacks one, so drives can be unplugged
/// afterwards and still browsed.
pub fn warm(db: &Db, cache: &ThumbCache, stop: &StopFlag, progress: ProgressFn) -> Result<WarmStats> {
    let conn = db.conn()?;
    let sql = format!(
        "SELECT {FILE_COLS} FROM files f JOIN volumes v ON v.id = f.volume_id
         WHERE v.online = 1 AND f.missing = 0 AND f.quarantined = 0 ORDER BY f.taken_at DESC"
    );
    let mut st = conn.prepare(&sql)?;
    let rows: Vec<FileRow> = st.query_map([], FileRow::from_row)?.filter_map(|r| r.ok()).filter(|f| !cache.has_thumb(f)).collect();
    drop(st);
    drop(conn);
    let total = rows.len() as u64;
    let done = AtomicU64::new(0);
    let failed = AtomicU64::new(0);
    progress(Progress::new("thumbs", "thumbs", 0, total, ""));
    rows.par_iter().for_each(|f| {
        if stop.is_stopped() {
            return;
        }
        if cache.ensure_thumb(f).is_err() {
            failed.fetch_add(1, Ordering::Relaxed);
        }
        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
        if d % 25 == 0 || d == total {
            progress(Progress::new("thumbs", "thumbs", d, total, f.name.clone()));
        }
    });
    let failed = failed.load(Ordering::Relaxed);
    Ok(WarmStats { generated: done.load(Ordering::Relaxed) - failed, failed, total })
}
