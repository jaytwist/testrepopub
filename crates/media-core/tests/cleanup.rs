//! Cleanup suggestions: bursts (best shot kept), blurry photos (relative to the same day),
//! duplicate copies not mistaken for bursts, dismissals remembered, burst grouping/stacking.

use std::fs;

use image::{DynamicImage, ImageBuffer, Rgb};
use media_core::db::{self, FileFilter};
use media_core::scanner::{self, ScanOptions};
use media_core::{analysis, cleanup, thumbs, volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

/// Photo-like image: a scene with large shapes (what dHash sees) plus fine texture (what the focus
/// score sees), unlike a smooth gradient.
fn textured(seed: u32) -> DynamicImage {
    let scene = picture(seed, 1200, 900).to_rgb8();
    DynamicImage::ImageRgb8(ImageBuffer::from_fn(1200, 900, |x, y| {
        let p = scene.get_pixel(x, y).0;
        let t = if ((x / 3) + (y / 3)) % 2 == 0 { 28i16 } else { -28 };
        let c = |v: u8| (v as i16 + t).clamp(0, 255) as u8;
        Rgb([c(p[0]), c(p[1]), c(p[2])])
    }))
}

fn blurred(img: &DynamicImage, sigma: f32) -> DynamicImage {
    DynamicImage::ImageRgba8(image::imageops::blur(&img.to_rgba8(), sigma)).to_rgb8().into()
}

#[test]
fn bursts_and_blur() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let a = drive(root, "diskA");
    let b = drive(root, "diskB");
    let cache = root.join("appdata");

    // A burst: 4 shots one second apart, same camera; the 3rd is shaken (blurred).
    let base = textured(1);
    for i in 0..4u32 {
        let src = if i == 2 { blurred(&base, 4.0) } else { base.clone() }.to_rgb8();
        // Tiny exposure differences so the shots are not byte-identical.
        let img = DynamicImage::ImageRgb8(ImageBuffer::from_fn(src.width(), src.height(), |x, y| {
            let p = src.get_pixel(x, y).0;
            Rgb([p[0].saturating_add(i as u8), p[1], p[2]])
        }));
        write_jpeg(&a.join(format!("DCIM/IMG_10{i}.jpg")), &img, Some((&format!("2023:08:15 10:00:0{i}"), "Apple", "iPhone 13")));
    }
    // An exact copy of shot 0 on another drive: a duplicate, not a 5th burst shot.
    fs::create_dir_all(b.join("backup")).unwrap();
    fs::copy(a.join("DCIM/IMG_100.jpg"), b.join("backup/IMG_100.jpg")).unwrap();
    // Six ordinary sharp photos the same day (reference for "blurry").
    for i in 0..6u32 {
        write_jpeg(&a.join(format!("DCIM/IMG_20{i}.jpg")), &textured(10 + i), Some((&format!("2023:08:15 1{}:30:00", i + 1), "Apple", "iPhone 13")));
    }
    // One isolated blurry photo.
    write_jpeg(&a.join("DCIM/IMG_300.jpg"), &blurred(&textured(30), 5.0), Some(("2023:08:15 18:00:00", "Apple", "iPhone 13")));

    // A screenshot (by name).
    write_png(&a.join("Pictures/Screenshot_20230815.png"), &picture(40, 1080, 2340));

    let db = Db::open(cache.join("library.db")).unwrap();
    let mut conn = db.conn().unwrap();
    volumes::add_source(&conn, &a).unwrap();
    volumes::add_source(&conn, &b).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    let tc = thumbs::ThumbCache::new(cache.join("cache"));
    let st = analysis::run(&db, &tc, None, 2, &stop, &analysis::PauseFlag::default(), &quiet).unwrap();
    assert_eq!((st.processed, st.failed), (13, 0), "{st:?}");
    cleanup::refresh(&mut conn).unwrap();

    let name_of = |id: i64| db::get_file(&conn, id).unwrap().unwrap().rel_path;

    // --- bursts ---
    let groups = db::group_summary(&conn, &FileFilter::default(), "burst").unwrap();
    assert_eq!(groups.len(), 1, "{groups:?}");
    assert_eq!(groups[0].count, 4, "the copy on diskB is not part of the burst");
    let extras = cleanup::burst_extra_ids(&conn).unwrap();
    assert_eq!(extras.len(), 3);
    let best: i64 = conn.query_row("SELECT file_id FROM bursts WHERE rank = 0", [], |r| r.get(0)).unwrap();
    assert_ne!(name_of(best), "DCIM/IMG_102.jpg", "the shaken shot is never the best");
    assert!(extras.iter().any(|id| name_of(*id) == "DCIM/IMG_102.jpg"));
    // Grid: burst size on each member; stacking shows one tile for the burst.
    let burst_items = db::query_grid(&conn, &FileFilter { in_burst: true, ..Default::default() }, 0, 50).unwrap();
    assert!(burst_items.iter().all(|g| g.burst_size == Some(4)), "{burst_items:?}");
    let stacked = db::query_grid(&conn, &FileFilter { in_burst: true, stack_bursts: true, ..Default::default() }, 0, 50).unwrap();
    assert_eq!(stacked.len(), 1);
    assert_eq!(stacked[0].id, best);

    // --- blurry ---
    let blurry = db::query_files(&conn, &FileFilter { suggest: Some("blurry".into()), ..Default::default() }, 0, 50).unwrap();
    let names: Vec<String> = blurry.iter().map(|f| f.rel_path.clone()).collect();
    assert!(names.contains(&"DCIM/IMG_300.jpg".to_string()), "{names:?}");
    assert!(!names.iter().any(|n| n.starts_with("DCIM/IMG_20")), "sharp photos are never blurry: {names:?}");
    let reasons = cleanup::reasons(&conn, blurry.iter().find(|f| f.rel_path == "DCIM/IMG_300.jpg").unwrap().id).unwrap();
    assert!(reasons.iter().any(|(k, d)| k == "blurry" && d.contains("같은 날")), "{reasons:?}");

    // --- summary + dismissals ---
    let s1 = cleanup::summary(&conn).unwrap();
    assert_eq!(s1.burst_groups, 1);
    assert_eq!(s1.burst_extras.count, 3);
    assert_eq!(s1.blurry.count as usize, blurry.len());
    let img300 = blurry.iter().find(|f| f.rel_path == "DCIM/IMG_300.jpg").unwrap().id;
    cleanup::dismiss(&mut conn, &[img300], "blurry").unwrap();
    // Dismissals survive a recompute.
    cleanup::refresh(&mut conn).unwrap();
    let s2 = cleanup::summary(&conn).unwrap();
    assert_eq!(s2.blurry.count, s1.blurry.count - 1);
    let still = db::query_ids(&conn, &FileFilter { suggest: Some("blurry".into()), ..Default::default() }).unwrap();
    assert!(!still.contains(&img300));

    // --- screenshots / large videos use the same suggest filter and dismissals ---
    let shots = db::query_ids(&conn, &FileFilter { suggest: Some("screenshot".into()), ..Default::default() }).unwrap();
    assert_eq!(shots.len(), 1);
    assert_eq!(s2.screenshots.count, 1);
    cleanup::dismiss(&mut conn, &shots, "screenshot").unwrap();
    assert!(db::query_ids(&conn, &FileFilter { suggest: Some("screenshot".into()), ..Default::default() }).unwrap().is_empty());
    assert_eq!(cleanup::summary(&conn).unwrap().screenshots.count, 0);
    assert!(db::query_ids(&conn, &FileFilter { suggest: Some("large_video".into()), ..Default::default() }).unwrap().is_empty());
}
