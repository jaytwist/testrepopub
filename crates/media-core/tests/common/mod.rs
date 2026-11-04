//! Shared test fixtures: synthetic photos with real EXIF, simulated drives.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageBuffer, Rgb};

/// A deterministic, textured test picture (perceptual hashes need structure).
pub fn picture(seed: u32, w: u32, h: u32) -> DynamicImage {
    let img = ImageBuffer::from_fn(w, h, |x, y| {
        let fx = x as f32 / w as f32;
        let fy = y as f32 / h as f32;
        let s = seed as f32;
        let r = ((fx * 6.0 + s).sin() * 127.0 + 128.0) as u8;
        let g = ((fy * (4.0 + s * 0.7)).cos() * 127.0 + 128.0) as u8;
        let b = (((fx + fy) * (3.0 + s)).sin() * 127.0 + 128.0) as u8;
        Rgb([r, g, b])
    });
    DynamicImage::ImageRgb8(img)
}

/// Minimal EXIF APP1 segment carrying DateTimeOriginal + Make/Model.
pub fn exif_app1(datetime: &str, make: &str, model: &str) -> Vec<u8> {
    exif_app1_gps(datetime, make, model, None)
}

/// EXIF APP1 with DateTimeOriginal, Make/Model and optionally GPS (decimal degrees).
pub fn exif_app1_gps(datetime: &str, make: &str, model: &str, gps: Option<(f64, f64)>) -> Vec<u8> {
    let mut tiff: Vec<u8> = Vec::new();
    let le16 = |v: &mut Vec<u8>, x: u16| v.extend_from_slice(&x.to_le_bytes());
    let le32 = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_le_bytes());
    let make_b = format!("{make}\0").into_bytes();
    let model_b = format!("{model}\0").into_bytes();
    let dt_b = format!("{datetime}\0").into_bytes();
    let n0: u32 = if gps.is_some() { 4 } else { 3 };
    // Layout: header(8) | IFD0 | ExifIFD (18) | GPS IFD (54, optional) | data
    let ifd0_off = 8u32;
    let exif_off = ifd0_off + 2 + 12 * n0 + 4;
    let gps_off = exif_off + 18;
    let data_off = gps_off + if gps.is_some() { 54 } else { 0 };
    let make_off = data_off;
    let model_off = make_off + make_b.len() as u32;
    let dt_off = model_off + model_b.len() as u32;
    let lat_off = dt_off + dt_b.len() as u32;
    let lon_off = lat_off + 24;
    tiff.extend_from_slice(b"II");
    le16(&mut tiff, 42);
    le32(&mut tiff, ifd0_off);
    le16(&mut tiff, n0 as u16);
    let mut entries = vec![(0x010fu16, 2u16, make_b.len() as u32, make_off), (0x0110, 2, model_b.len() as u32, model_off), (0x8769, 4, 1, exif_off)];
    if gps.is_some() {
        entries.push((0x8825, 4, 1, gps_off));
    }
    for (tag, typ, count, val) in entries {
        le16(&mut tiff, tag);
        le16(&mut tiff, typ);
        le32(&mut tiff, count);
        le32(&mut tiff, val);
    }
    le32(&mut tiff, 0);
    le16(&mut tiff, 1);
    le16(&mut tiff, 0x9003);
    le16(&mut tiff, 2);
    le32(&mut tiff, dt_b.len() as u32);
    le32(&mut tiff, dt_off);
    le32(&mut tiff, 0);
    if let Some((lat, lon)) = gps {
        le16(&mut tiff, 4);
        let refs = [(0x0001u16, if lat >= 0.0 { b'N' } else { b'S' }), (0x0003, if lon >= 0.0 { b'E' } else { b'W' })];
        for (i, (tag, r)) in refs.iter().enumerate() {
            le16(&mut tiff, *tag);
            le16(&mut tiff, 2);
            le32(&mut tiff, 2);
            tiff.extend_from_slice(&[*r, 0, 0, 0]);
            le16(&mut tiff, tag + 1);
            le16(&mut tiff, 5);
            le32(&mut tiff, 3);
            le32(&mut tiff, if i == 0 { lat_off } else { lon_off });
        }
        le32(&mut tiff, 0);
    }
    tiff.extend_from_slice(&make_b);
    tiff.extend_from_slice(&model_b);
    tiff.extend_from_slice(&dt_b);
    if let Some((lat, lon)) = gps {
        for v in [lat.abs(), lon.abs()] {
            let deg = v.floor();
            let min = ((v - deg) * 60.0).floor();
            let sec = ((v - deg) * 60.0 - min) * 60.0;
            for (n, d) in [(deg as u32, 1u32), (min as u32, 1), ((sec * 1000.0).round() as u32, 1000)] {
                le32(&mut tiff, n);
                le32(&mut tiff, d);
            }
        }
    }

    let mut seg = vec![0xFF, 0xE1];
    let len = (2 + 6 + tiff.len()) as u16;
    seg.extend_from_slice(&len.to_be_bytes());
    seg.extend_from_slice(b"Exif\0\0");
    seg.extend_from_slice(&tiff);
    seg
}

/// JPEG with EXIF date/camera and GPS.
pub fn write_jpeg_gps(path: &Path, img: &DynamicImage, datetime: &str, gps: (f64, f64)) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg).unwrap();
    let mut out = vec![0xFF, 0xD8];
    out.extend_from_slice(&exif_app1_gps(datetime, "Apple", "iPhone 14", Some(gps)));
    out.extend_from_slice(&buf[2..]);
    fs::write(path, out).unwrap();
}

pub fn write_jpeg(path: &Path, img: &DynamicImage, exif: Option<(&str, &str, &str)>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg).unwrap();
    if let Some((dt, mk, md)) = exif {
        // Insert APP1 right after SOI.
        let app1 = exif_app1(dt, mk, md);
        let mut out = vec![0xFF, 0xD8];
        out.extend_from_slice(&app1);
        out.extend_from_slice(&buf[2..]);
        buf = out;
    }
    fs::write(path, buf).unwrap();
}

pub fn write_png(path: &Path, img: &DynamicImage) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    img.save(path).unwrap();
}

pub fn drive(root: &Path, name: &str) -> PathBuf {
    let d = root.join(name);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join(".mo-volume"), b"").unwrap();
    d
}

