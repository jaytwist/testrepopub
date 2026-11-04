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
    let mut tiff: Vec<u8> = Vec::new();
    let le16 = |v: &mut Vec<u8>, x: u16| v.extend_from_slice(&x.to_le_bytes());
    let le32 = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_le_bytes());
    let make_b = format!("{make}\0").into_bytes();
    let model_b = format!("{model}\0").into_bytes();
    let dt_b = format!("{datetime}\0").into_bytes();
    // Layout: header(8) | IFD0 (2 + 3*12 + 4 = 42) | ExifIFD (2 + 12 + 4 = 18) | data
    let ifd0_off = 8u32;
    let exif_off = ifd0_off + 42;
    let data_off = exif_off + 18;
    let make_off = data_off;
    let model_off = make_off + make_b.len() as u32;
    let dt_off = model_off + model_b.len() as u32;
    tiff.extend_from_slice(b"II");
    le16(&mut tiff, 42);
    le32(&mut tiff, ifd0_off);
    le16(&mut tiff, 3);
    for (tag, typ, count, val) in [(0x010fu16, 2u16, make_b.len() as u32, make_off), (0x0110, 2, model_b.len() as u32, model_off), (0x8769, 4, 1, exif_off)] {
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
    tiff.extend_from_slice(&make_b);
    tiff.extend_from_slice(&model_b);
    tiff.extend_from_slice(&dt_b);

    let mut seg = vec![0xFF, 0xE1];
    let len = (2 + 6 + tiff.len()) as u16;
    seg.extend_from_slice(&len.to_be_bytes());
    seg.extend_from_slice(b"Exif\0\0");
    seg.extend_from_slice(&tiff);
    seg
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

