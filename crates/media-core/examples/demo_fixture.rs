//! Build a messy multi-drive demo library for manual testing / screenshots.
//!
//!   cargo run -p media-core --example demo_fixture -- <root-dir>
//!
//! Creates <root>/{diskC,usbA,usbB}, each marked as a separate "volume" with `.mo-volume`.
//! Videos are generated only when ffmpeg is on PATH.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use image::{DynamicImage, ImageBuffer, Rgb};

fn picture(seed: u32, w: u32, h: u32) -> DynamicImage {
    let s = seed as f32;
    let (hr, hg, hb) = ((s * 0.37).sin().abs(), (s * 0.61).cos().abs(), (s * 0.13).sin().abs());
    let img = ImageBuffer::from_fn(w, h, |x, y| {
        let fx = x as f32 / w as f32;
        let fy = y as f32 / h as f32;
        // Sky-ish gradient, a "horizon", and a sun: looks vaguely like a photo in a grid.
        let horizon = 0.55 + 0.1 * (fx * 6.0 + s).sin();
        let (mut r, mut g, mut b) = if fy < horizon {
            (120.0 + 100.0 * hr * (1.0 - fy), 150.0 + 80.0 * hg * (1.0 - fy), 200.0 + 55.0 * hb)
        } else {
            (60.0 + 90.0 * hr * fy, 90.0 + 100.0 * hg, 50.0 + 60.0 * hb * fy)
        };
        let (cx, cy) = (0.2 + 0.6 * ((s * 0.7).sin() * 0.5 + 0.5), 0.25);
        let d = ((fx - cx).powi(2) + (fy - cy).powi(2) * 0.6).sqrt();
        if d < 0.08 {
            r = 255.0;
            g = 230.0;
            b = 150.0;
        }
        let n = (((x * 7919 + y * 104729 + seed * 31) % 23) as f32) - 11.0;
        Rgb([(r + n).clamp(0.0, 255.0) as u8, (g + n).clamp(0.0, 255.0) as u8, (b + n).clamp(0.0, 255.0) as u8])
    });
    DynamicImage::ImageRgb8(img)
}

fn exif_app1(datetime: &str, make: &str, model: &str) -> Vec<u8> {
    let mut t: Vec<u8> = Vec::new();
    let p16 = |v: &mut Vec<u8>, x: u16| v.extend_from_slice(&x.to_le_bytes());
    let p32 = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_le_bytes());
    let (mk, md, dt) = (format!("{make}\0").into_bytes(), format!("{model}\0").into_bytes(), format!("{datetime}\0").into_bytes());
    let exif_off = 8 + 42u32;
    let data = exif_off + 18;
    t.extend_from_slice(b"II");
    p16(&mut t, 42);
    p32(&mut t, 8);
    p16(&mut t, 3);
    for (tag, typ, cnt, val) in [(0x010fu16, 2u16, mk.len() as u32, data), (0x0110, 2, md.len() as u32, data + mk.len() as u32), (0x8769, 4, 1, exif_off)] {
        p16(&mut t, tag);
        p16(&mut t, typ);
        p32(&mut t, cnt);
        p32(&mut t, val);
    }
    p32(&mut t, 0);
    p16(&mut t, 1);
    p16(&mut t, 0x9003);
    p16(&mut t, 2);
    p32(&mut t, dt.len() as u32);
    p32(&mut t, data + mk.len() as u32 + md.len() as u32);
    p32(&mut t, 0);
    t.extend_from_slice(&mk);
    t.extend_from_slice(&md);
    t.extend_from_slice(&dt);
    let mut seg = vec![0xFF, 0xE1];
    seg.extend_from_slice(&((8 + t.len()) as u16).to_be_bytes());
    seg.extend_from_slice(b"Exif\0\0");
    seg.extend_from_slice(&t);
    seg
}

fn jpeg(path: &Path, img: &DynamicImage, exif: Option<(&str, &str, &str)>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg).unwrap();
    if let Some((d, m, o)) = exif {
        let mut out = vec![0xFF, 0xD8];
        out.extend(exif_app1(d, m, o));
        out.extend_from_slice(&buf[2..]);
        buf = out;
    }
    fs::write(path, buf).unwrap();
}

/// A picture with fine texture, so focus can be measured (a smooth gradient has none).
fn textured(seed: u32, w: u32, h: u32) -> DynamicImage {
    let scene = picture(seed, w, h).to_rgb8();
    DynamicImage::ImageRgb8(ImageBuffer::from_fn(w, h, |x, y| {
        let p = scene.get_pixel(x, y).0;
        let t = if ((x / 3) + (y / 3)) % 2 == 0 { 28i16 } else { -28 };
        let c = |v: u8| (v as i16 + t).clamp(0, 255) as u8;
        Rgb([c(p[0]), c(p[1]), c(p[2])])
    }))
}

fn blurred(img: &DynamicImage, sigma: f32) -> DynamicImage {
    DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(image::imageops::blur(&img.to_rgba8(), sigma)).to_rgb8())
}

fn drive(root: &Path, name: &str) -> PathBuf {
    let d = root.join(name);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join(".mo-volume"), b"").unwrap();
    d
}

fn video(path: &Path, seconds: u32, size: &str, extra: &[&str], src: Option<&Path>) -> bool {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut c = Command::new("ffmpeg");
    c.args(["-hide_banner", "-loglevel", "error", "-y"]);
    match src {
        Some(s) => {
            c.arg("-i").arg(s);
        }
        None => {
            c.args(["-f", "lavfi", "-i", &format!("testsrc2=duration={seconds}:size={size}:rate=30")]);
        }
    }
    c.args(["-vf", &format!("scale={}", size.replace('x', ":"))]).args(extra).arg(path);
    c.status().map(|s| s.success()).unwrap_or(false)
}

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).expect("usage: demo_fixture <root>"));
    let c = drive(&root, "diskC");
    let a = drive(&root, "usbA");
    let b = drive(&root, "usbB");
    let cams = [("Apple", "iPhone 13"), ("samsung", "SM-S918N"), ("SONY", "ILCE-7M3"), ("Canon", "Canon EOS R6")];
    let mut seed = 0u32;
    let mut next = || {
        seed += 1;
        seed
    };

    // Phone camera roll on C:, spread across years.
    for (y, months) in [(2017, [3, 8]), (2019, [5, 12]), (2021, [1, 7]), (2023, [4, 10]), (2024, [6, 9])] {
        for m in months {
            for d in 1..=6u32 {
                let (mk, md) = cams[(y as usize + m as usize + d as usize) % cams.len()];
                let s = next();
                let dt = format!("{y}:{m:02}:{:02} {:02}:{:02}:00", d * 4, 9 + d, (s * 7) % 60);
                jpeg(&c.join(format!("Users/me/Pictures/Camera Roll/IMG_{:04}.jpg", s)), &picture(s, 1600, 1200), Some((&dt, mk, md)));
            }
        }
    }
    // A USB backup that duplicates part of the camera roll (exact copies).
    for i in [3u32, 9, 15, 21, 27] {
        let src = c.join(format!("Users/me/Pictures/Camera Roll/IMG_{i:04}.jpg"));
        let dst = a.join(format!("백업/폰사진/IMG_{i:04}.jpg"));
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::copy(src, dst).unwrap();
    }
    // Messenger exports: resized copies (similar images) with the date only in the file name.
    for (i, (y, m, d)) in [(2019, 5, 8), (2021, 7, 12), (2023, 10, 4)].iter().enumerate() {
        let s = [5u32, 23, 41][i];
        let img = picture(s, 1600, 1200).resize(800, 600, image::imageops::FilterType::Triangle);
        jpeg(&c.join(format!("Users/me/Documents/카카오톡 받은 파일/KakaoTalk_{y}{m:02}{d:02}_1530{:02}123.jpg", i)), &img, None);
    }
    for i in 0..4u32 {
        jpeg(&a.join(format!("WhatsApp Images/IMG-2022081{}-WA000{}.jpg", i, i)), &picture(next(), 1024, 768), None);
    }
    // Screenshots.
    for i in 0..5u32 {
        let p = c.join(format!("Users/me/Pictures/Screenshots/Screenshot_2024061{}-1012{:02}.png", i, i));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        picture(next() + 500, 1080, 2340).resize_exact(540, 1170, image::imageops::FilterType::Nearest).save(&p).unwrap();
    }
    // Old scans with only a folder date.
    for i in 0..6u32 {
        jpeg(&b.join(format!("옛날사진/2009-07 제주도/scan{:02}.jpg", i)), &picture(next(), 1200, 800), None);
    }
    // Downloads with no date at all.
    for i in 0..3u32 {
        jpeg(&c.join(format!("Users/me/Downloads/image ({i}).jpg")), &picture(next(), 900, 900), None);
    }
    // Two different photos that collide on name.
    jpeg(&a.join("DCIM/100CANON/IMG_0001.jpg"), &picture(900, 1600, 1067), Some(("2024:06:15 10:00:00", "Canon", "Canon EOS R6")));
    jpeg(&b.join("DCIM/100CANON/IMG_0001.jpg"), &picture(901, 1600, 1067), Some(("2024:06:16 11:00:00", "Canon", "Canon EOS R6")));

    // A day out on the phone (cleanup suggestions): a 5-shot burst with one shaken frame, sharp
    // photos through the day, and two blurry ones.
    let day = a.join("DCIM/101APPLE");
    let base = textured(700, 1600, 1200);
    for i in 0..5u32 {
        let src = if i == 3 { blurred(&base, 4.0) } else { base.clone() }.to_rgb8();
        let img = DynamicImage::ImageRgb8(ImageBuffer::from_fn(src.width(), src.height(), |x, y| {
            let p = src.get_pixel(x, y).0;
            Rgb([p[0].saturating_add(i as u8 * 2), p[1], p[2]])
        }));
        jpeg(&day.join(format!("IMG_50{i}.jpg")), &img, Some((&format!("2024:06:15 14:20:0{}", i * 2), "Apple", "iPhone 15")));
    }
    for i in 0..6u32 {
        jpeg(&day.join(format!("IMG_51{i}.jpg")), &textured(710 + i, 1600, 1200), Some((&format!("2024:06:15 {:02}:10:00", 9 + i * 2), "Apple", "iPhone 15")));
    }
    for (i, sigma) in [(0u32, 5.0f32), (1, 6.0)] {
        jpeg(&day.join(format!("IMG_52{i}.jpg")), &blurred(&textured(720 + i, 1600, 1200), sigma), Some((&format!("2024:06:15 {:02}:45:00", 12 + i * 5), "Apple", "iPhone 15")));
    }

    // Videos.
    let v1 = c.join("Users/me/Videos/VID_20230415_120000.mp4");
    if video(&v1, 6, "1280x720", &["-c:v", "libx264", "-pix_fmt", "yuv420p"], None) {
        video(&a.join("백업/영상/VID_20230415_120000_small.mp4"), 0, "640x360", &["-c:v", "libx264", "-crf", "30", "-pix_fmt", "yuv420p"], Some(&v1));
        video(&b.join("캠코더/00012.MTS"), 4, "1280x720", &["-c:v", "mpeg2video", "-f", "mpegts"], None);
        video(&c.join("Users/me/Videos/clip_hevc.mov"), 3, "1280x720", &["-c:v", "libx265", "-tag:v", "hvc1", "-pix_fmt", "yuv420p"], None);
    } else {
        eprintln!("ffmpeg not found: skipping videos");
    }
    println!("{}", root.display());
}
