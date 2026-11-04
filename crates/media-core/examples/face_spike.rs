//! Spike: detect + embed faces in sample photos, print pairwise similarity and timing.
//!   face_spike <onnxruntime lib> <models dir> <image>...
use std::time::Instant;
use media_core::faces::{similarity, FaceEngine};

fn main() -> anyhow::Result<()> {
    let mut a = std::env::args().skip(1);
    let lib = a.next().unwrap();
    let models = a.next().unwrap();
    let t = Instant::now();
    let mut eng = FaceEngine::load(lib.as_ref(), models.as_ref(), false, 4)?;
    println!("load {:?} provider={}", t.elapsed(), eng.provider);
    let mut all: Vec<(String, Vec<f32>)> = Vec::new();
    for path in a {
        let t0 = Instant::now();
        let img = image::open(&path)?.to_rgb8();
        let t1 = Instant::now();
        let faces = eng.detect(&img, 0.8)?;
        let t2 = Instant::now();
        let name = std::path::Path::new(&path).file_name().unwrap().to_string_lossy().to_string();
        for (i, f) in faces.iter().enumerate() {
            let e = eng.embed(&img, f)?;
            println!("  {name}#{i} box=({:.0},{:.0},{:.0}x{:.0}) score={:.3}", f.x, f.y, f.w, f.h, f.score);
            all.push((format!("{name}#{i}"), e));
        }
        println!("{name} {}x{} faces={} decode={:?} detect={:?} embed={:?}", img.width(), img.height(), faces.len(), t1 - t0, t2 - t1, t2.elapsed());
    }
    println!("\nsimilarity:");
    for i in 0..all.len() {
        for j in i + 1..all.len() {
            println!("  {:>22} ~ {:<22} {:.3}", all[i].0, all[j].0, similarity(&all[i].1, &all[j].1));
        }
    }
    Ok(())
}
