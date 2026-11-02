//! End-to-end: three simulated drives -> scan -> thumbnails -> exact + similar duplicates ->
//! quarantine + undo -> organize into a library on another drive -> undo.

use std::fs;
use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageBuffer, Rgb};
use media_core::db::{self, FileFilter};
use media_core::dedupe::{self, DupKind, Similarity};
use media_core::organizer::{self, Mode, OrganizeOptions, Rename};
use media_core::scanner::{self, ScanOptions};
use media_core::{journal, labels, thumbs, volumes, Db, Progress, StopFlag};

fn quiet(_: Progress) {}

/// A deterministic, textured test picture (perceptual hashes need structure).
fn picture(seed: u32, w: u32, h: u32) -> DynamicImage {
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
fn exif_app1(datetime: &str, make: &str, model: &str) -> Vec<u8> {
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

fn write_jpeg(path: &Path, img: &DynamicImage, exif: Option<(&str, &str, &str)>) {
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

fn write_png(path: &Path, img: &DynamicImage) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    img.save(path).unwrap();
}

fn drive(root: &Path, name: &str) -> PathBuf {
    let d = root.join(name);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join(".mo-volume"), b"").unwrap();
    d
}

#[test]
fn full_workflow() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let disk_a = drive(root, "diskA");
    let disk_b = drive(root, "diskB");
    let disk_c = drive(root, "diskC");
    let cache = root.join("appdata");
    dedupe::init_czkawka(&cache);

    // --- Fixture -------------------------------------------------------------------------
    let p1 = picture(1, 1200, 900);
    write_jpeg(&disk_a.join("DCIM/IMG_0001.jpg"), &p1, Some(("2019:05:12 14:30:12", "Apple", "iPhone 13")));
    // Exact duplicate on another drive.
    fs::copy(disk_a.join("DCIM/IMG_0001.jpg"), {
        fs::create_dir_all(disk_b.join("backup")).unwrap();
        disk_b.join("backup/IMG_0001.jpg")
    })
    .unwrap();
    // Resized copy (similar, not identical), no EXIF.
    write_jpeg(&disk_b.join("resized/IMG_0001_small.jpg"), &p1.resize(600, 450, image::imageops::FilterType::Triangle), None);
    // Messenger file: date only in file name.
    write_jpeg(&disk_b.join("KakaoTalk/KakaoTalk_20210304_101112345.jpg"), &picture(7, 800, 600), None);
    // Screenshot (PNG).
    write_png(&disk_a.join("Pictures/Screenshots/Screenshot_20200102-030405.png"), &picture(11, 540, 1170));
    // Date only from folder name.
    write_jpeg(&disk_a.join("2015-08 여행/beach.jpg"), &picture(13, 800, 600), None);
    // Two different files that will collide on name in the library (same month).
    write_jpeg(&disk_a.join("cam1/DSC0001.jpg"), &picture(17, 800, 600), Some(("2019:05:20 10:00:00", "Sony", "ILCE-7M3")));
    write_jpeg(&disk_b.join("cam2/DSC0001.jpg"), &picture(19, 800, 600), Some(("2019:05:21 11:00:00", "Sony", "ILCE-7M3")));
    // Junk that must be ignored.
    fs::write(disk_a.join("DCIM/notes.txt"), b"not media").unwrap();
    fs::write(disk_a.join("DCIM/tiny.jpg"), b"\xFF\xD8tiny").unwrap();
    // System / app / hidden folders: real-looking photos that are NOT the user's.
    write_jpeg(&disk_a.join("Program Files/Vendor/splash.jpg"), &picture(31, 800, 600), None);
    write_jpeg(&disk_a.join("Users/me/AppData/Local/Cache/img.jpg"), &picture(32, 800, 600), None);
    fs::create_dir_all(disk_a.join("Games/MyGame/textures")).unwrap();
    for dll in ["engine.dll", "physics.dll", "audio.dll"] {
        fs::write(disk_a.join("Games/MyGame").join(dll), b"MZ").unwrap();
    }
    write_jpeg(&disk_a.join("Games/MyGame/textures/sky.jpg"), &picture(33, 1024, 1024), None);
    write_jpeg(&disk_b.join(".hidden/secret.jpg"), &picture(34, 800, 600), None);
    write_jpeg(&disk_b.join("DCIM/.thumbnails/thumb.jpg"), &picture(35, 800, 600), None);
    // A noisy 300px sticker: passes the byte check, fails the pixel check.
    let noisy = DynamicImage::ImageRgb8(ImageBuffer::from_fn(300, 300, |x, y| {
        let v = (x.wrapping_mul(2654435761) ^ y.wrapping_mul(40503)).wrapping_mul(2246822519) >> 24;
        Rgb([v as u8, v.wrapping_mul(3) as u8, v.wrapping_mul(7) as u8])
    }));
    write_jpeg(&disk_b.join("KakaoTalk/sticker.jpg"), &noisy, None);
    assert!(fs::metadata(disk_b.join("KakaoTalk/sticker.jpg")).unwrap().len() > 10 * 1024, "fixture: sticker must pass the byte check");

    let originals: Vec<(PathBuf, String)> = walk_media(root);

    // --- Index -------------------------------------------------------------------------------
    let db = Db::open(cache.join("library.db")).unwrap();
    let conn = db.conn().unwrap();
    let sa = volumes::add_source(&conn, &disk_a).unwrap();
    let sb = volumes::add_source(&conn, &disk_b).unwrap();
    assert_ne!(sa, sb);
    let vols = db::list_volumes(&conn).unwrap();
    assert_eq!(vols.len(), 2, "two simulated drives: {vols:?}");

    let stop = StopFlag::new();
    let stats = scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    let added: u64 = stats.iter().map(|s| s.added).sum();
    assert_eq!(added, 8, "{stats:?}");
    let sk = |f: fn(&media_core::userfiles::SkipCounts) -> u64| stats.iter().map(|s| f(&s.skipped)).sum::<u64>();
    assert_eq!(sk(|c| c.system_folders), 3, "Program Files, AppData, .thumbnails: {stats:?}");
    assert_eq!(sk(|c| c.hidden_folders), 1, ".hidden");
    assert_eq!(sk(|c| c.app_files), 1, "game texture");
    assert_eq!(sk(|c| c.too_small), 2, "tiny.jpg by bytes + sticker by pixels");

    let all = db::query_files(&conn, &FileFilter::default(), 0, 100).unwrap();
    let by_name = |n: &str| all.iter().find(|f| f.rel_path.ends_with(n)).unwrap_or_else(|| panic!("{n} missing")).clone();
    let img1 = by_name("DCIM/IMG_0001.jpg");
    assert_eq!(img1.date_source, "exif");
    assert_eq!(media_core::meta::fmt_ts(img1.taken_at.unwrap()), "2019-05-12 14:30");
    assert_eq!(img1.camera.as_deref(), Some("Apple iPhone 13"));
    assert_eq!((img1.width, img1.height), (Some(1200), Some(900)));
    let kakao = by_name("KakaoTalk_20210304_101112345.jpg");
    assert_eq!(kakao.date_source, "filename");
    let beach = by_name("beach.jpg");
    assert_eq!(beach.date_source, "folder");
    let shot = by_name("Screenshot_20200102-030405.png");
    let shot_labels: Vec<String> = labels::labels_for(&conn, shot.id).unwrap().into_iter().map(|l| l.label).collect();
    assert!(shot_labels.contains(&"screenshot".to_string()), "{shot_labels:?}");
    let k_labels: Vec<String> = labels::labels_for(&conn, kakao.id).unwrap().into_iter().map(|l| l.label).collect();
    assert!(k_labels.contains(&"src:kakaotalk".to_string()));

    // Filters + timeline.
    let screenshots = db::query_files(&conn, &FileFilter { kind: Some("screenshot".into()), ..Default::default() }, 0, 10).unwrap();
    assert_eq!(screenshots.len(), 1);
    let y2019 = db::query_files(&conn, &FileFilter { year: Some(2019), ..Default::default() }, 0, 10).unwrap();
    assert_eq!(y2019.len(), 4, "IMG_0001 x2 + two DSC0001");
    let tl = db::timeline_summary(&conn, &FileFilter::default()).unwrap();
    assert!(tl.iter().any(|m| m.year == 2019 && m.month == 5 && m.count == 4), "{tl:?}");

    // Metadata sort + range filters.
    let q = |f: FileFilter| db::query_files(&conn, &f, 0, 100).unwrap();
    let by_size = q(FileFilter { sort: Some("size".into()), ..Default::default() });
    assert!(by_size.windows(2).all(|w| w[0].size >= w[1].size), "size desc");
    let by_name = q(FileFilter { sort: Some("name".into()), desc: Some(false), ..Default::default() });
    let names: Vec<String> = by_name.iter().map(|f| f.name.to_lowercase()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "name asc");
    assert_eq!(q(FileFilter { mp_min: Some(1.0), ..Default::default() }).len(), 2, "only the two 1200x900 originals are >= 1 MP");
    assert_eq!(q(FileFilter { camera: Some("Apple iPhone 13".into()), ..Default::default() }).len(), 2);
    assert_eq!(q(FileFilter { ext: Some("png".into()), ..Default::default() }).len(), 1);
    assert_eq!(q(FileFilter { ext: Some("jpeg".into()), ..Default::default() }).len(), 7, "jpeg == jpg group");
    let may = |d: u32| media_core::meta::to_ts(chrono::NaiveDate::from_ymd_opt(2019, 5, d).unwrap().and_hms_opt(0, 0, 0).unwrap());
    assert_eq!(q(FileFilter { date_from: Some(may(1)), date_to: Some(may(31)), ..Default::default() }).len(), 4);
    assert_eq!(q(FileFilter { date_from: Some(may(13)), date_to: Some(may(31)), ..Default::default() }).len(), 2, "only the two DSC0001");
    let big = img1.size;
    assert!(q(FileFilter { size_min: Some(big), ..Default::default() }).iter().all(|f| f.size >= big));
    let opts = db::filter_options(&conn).unwrap();
    assert!(opts.cameras.iter().any(|c| c.value == "Apple iPhone 13" && c.count == 2), "{:?}", opts.cameras);
    assert!(opts.exts.iter().any(|e| e.value == "jpg" && e.count == 7));
    assert!(opts.years.windows(2).all(|w| w[0] > w[1]), "years newest first: {:?}", opts.years);
    assert!([2021, 2020, 2019, 2015].iter().all(|y| opts.years.contains(y)), "{:?}", opts.years);
    let st = db::filtered_stats(&conn, &FileFilter { kind: Some("photo".into()), ..Default::default() }).unwrap();
    assert_eq!(st.count, 8);
    assert_eq!(st.photos, 8);

    // Groups for the library grid: month / year / folder, with exact counts.
    let months = db::group_summary(&conn, &FileFilter::default(), "month").unwrap();
    assert_eq!(months.iter().map(|g| g.count).sum::<i64>(), 8);
    for g in &months {
        let tl_count = tl.iter().find(|m| Some(m.year) == g.year && Some(m.month) == g.month).map(|m| m.count);
        assert_eq!(tl_count, Some(g.count), "month group matches timeline: {g:?}");
        let ids = db::query_ids(&conn, &FileFilter { year: g.year, month: g.month, ..Default::default() }).unwrap();
        assert_eq!(ids.len() as i64, g.count, "selecting a month group returns exactly its files");
    }
    assert!(months.windows(2).all(|w| (w[0].year, w[0].month) >= (w[1].year, w[1].month)), "newest month first");
    let asc = db::group_summary(&conn, &FileFilter { sort: Some("taken".into()), desc: Some(false), ..Default::default() }, "month").unwrap();
    assert_eq!(asc.first().map(|g| g.key.clone()), months.last().map(|g| g.key.clone()), "oldest first when sorted ascending");
    let years = db::group_summary(&conn, &FileFilter::default(), "year").unwrap();
    assert_eq!(years.iter().find(|g| g.year == Some(2019)).map(|g| g.count), Some(4));
    let folders = db::group_summary(&conn, &FileFilter::default(), "folder").unwrap();
    assert_eq!(folders.len(), 8, "every fixture file sits in its own folder: {folders:?}");
    for g in &folders {
        let f = FileFilter { volume_id: g.volume_id, dir: g.dir.clone(), ..Default::default() };
        assert_eq!(db::query_ids(&conn, &f).unwrap().len() as i64, g.count, "{g:?}");
    }
    assert!(folders.iter().any(|g| g.dir.as_deref() == Some("Pictures/Screenshots")));
    let parent_only = FileFilter { volume_id: Some(shot.volume_id), dir: Some("Pictures".into()), ..Default::default() };
    assert_eq!(db::query_ids(&conn, &parent_only).unwrap().len(), 0, "a folder group does not include its subfolders");
    let all = db::group_summary(&conn, &FileFilter::default(), "none").unwrap();
    assert_eq!((all.len(), all[0].count), (1, 8));

    // Rescan is incremental: nothing changes.
    let stats = scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    assert_eq!(stats.iter().map(|s| s.unchanged).sum::<u64>(), 8);

    // User label + date correction.
    let mut c2 = db.conn().unwrap();
    labels::add_user_label(&mut c2, &[beach.id], "가족여행").unwrap();
    labels::set_taken_at(&mut c2, &[beach.id], media_core::meta::to_ts(chrono::NaiveDate::from_ymd_opt(2015, 8, 3).unwrap().and_hms_opt(9, 0, 0).unwrap())).unwrap();
    let beach = db::get_file(&conn, beach.id).unwrap().unwrap();
    assert_eq!(beach.date_source, "user");

    // --- Thumbnails --------------------------------------------------------------------------
    let tc = thumbs::ThumbCache::new(cache.join("cache"));
    let w = thumbs::warm(&db, &tc, &stop, &quiet).unwrap();
    assert_eq!((w.total, w.failed), (8, 0), "{w:?}");
    let t = image::open(tc.thumb_path(&img1)).unwrap();
    assert!(t.width() <= thumbs::THUMB_SIZE && t.height() <= thumbs::THUMB_SIZE);

    // --- Exact duplicates --------------------------------------------------------------------
    let ds = dedupe::run(&db, DupKind::Exact, Similarity::High, &stop, &quiet).unwrap();
    assert_eq!((ds.groups, ds.files), (1, 2), "{ds:?}");
    let groups = dedupe::list_groups(&conn, "exact", 0, 10).unwrap();
    assert_eq!(groups[0].members.iter().filter(|m| m.keep).count(), 1);
    let rep = dedupe::apply(&db, "exact", None, &stop, &quiet).unwrap();
    assert_eq!(rep.quarantined, 1, "{rep:?}");
    let q = db::query_files(&conn, &FileFilter::default(), 0, 100).unwrap();
    assert_eq!(q.len(), 7);
    assert_eq!(db::library_stats(&conn).unwrap().quarantined_files, 1);
    // Undo restores the file at its original path.
    let undo = journal::undo_batch(&conn, &rep.batch_id).unwrap();
    assert_eq!((undo.restored, undo.failed), (1, 0), "{undo:?}");
    assert_eq!(db::query_files(&conn, &FileFilter::default(), 0, 100).unwrap().len(), 8);
    assert!(disk_b.join("backup/IMG_0001.jpg").is_file());

    // --- Delete selected files (quarantine) + undo ------------------------------------------
    let victims = vec![kakao.id, shot.id];
    let rep = journal::delete_files(&db, &victims, &stop, &quiet).unwrap();
    assert_eq!((rep.deleted, rep.failed, rep.skipped_offline), (2, 0, 0), "{rep:?}");
    assert!(!disk_b.join("KakaoTalk/KakaoTalk_20210304_101112345.jpg").exists(), "moved out of the folder");
    assert_eq!(db::query_files(&conn, &FileFilter::default(), 0, 100).unwrap().len(), 6);
    let batch = journal::list_batches(&conn).unwrap().into_iter().find(|b| b.id == rep.batch_id).unwrap();
    assert_eq!((batch.kind.as_str(), batch.op_count), ("delete", 2));
    let undo = journal::undo_batch(&conn, &rep.batch_id).unwrap();
    assert_eq!((undo.restored, undo.failed), (2, 0), "{undo:?}");
    assert!(disk_b.join("KakaoTalk/KakaoTalk_20210304_101112345.jpg").is_file(), "restored in place");
    assert_eq!(db::query_files(&conn, &FileFilter::default(), 0, 100).unwrap().len(), 8);

    // --- Similar images (czkawka_core) -------------------------------------------------------
    let ds = dedupe::run(&db, DupKind::SimilarImage, Similarity::High, &stop, &quiet).unwrap();
    let sg = dedupe::list_groups(&conn, "similar_image", 0, 10).unwrap();
    assert!(ds.groups >= 1, "{ds:?}");
    let g = sg.iter().find(|g| g.members.iter().any(|m| m.file.rel_path.ends_with("IMG_0001_small.jpg"))).expect("resized copy grouped");
    assert_eq!(g.members.len(), 3, "original + identical backup + resized copy");
    let keeper = g.members.iter().find(|m| m.keep).unwrap();
    assert_eq!(keeper.file.width, Some(1200), "highest resolution is suggested as keeper");

    // --- Organize into a library on diskC ----------------------------------------------------
    let lib = disk_c.join("Library");
    let opts = OrganizeOptions { target_dir: lib.to_string_lossy().into(), template: "{year}/{year}-{month}".into(), rename: Rename::Keep, mode: Mode::Move, ..Default::default() };
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    assert!(!lib.exists(), "preview must not create anything on disk");
    let s = &plan.summary;
    assert_eq!(s.total, 8);
    // Identical IMG_0001 from diskB targets the same path as diskA's: the second is skipped, not renamed.
    assert_eq!(s.skip_duplicate, 1, "{s:?}");
    assert_eq!(s.moves, 7);
    assert_eq!(s.renamed, 1, "only the two *different* DSC0001 files need a suffix: {s:?}");
    assert_eq!(s.uncertain_dates, 1, "only the resized copy (no EXIF, no date in name); beach was corrected by the user");
    let rep = organizer::execute(&db, &plan, &stop, &quiet).unwrap();
    assert_eq!((rep.done, rep.failed), (7, 0), "{:?}", rep.errors);
    assert!(lib.join("2019/2019-05/IMG_0001.jpg").is_file());
    assert!(!lib.join("2019/2019-05/IMG_0001_1.jpg").exists(), "no duplicate created in the library");
    assert!(
        disk_b.join("backup/IMG_0001.jpg").is_file() ^ disk_a.join("DCIM/IMG_0001.jpg").is_file(),
        "exactly one identical copy moved; the skipped one stays for the duplicate tool"
    );
    assert!(lib.join("2019/2019-05/DSC0001.jpg").is_file());
    assert!(lib.join("2019/2019-05/DSC0001_1.jpg").is_file());
    assert!(lib.join("2021/2021-03/KakaoTalk_20210304_101112345.jpg").is_file());
    assert!(lib.join("2015/2015-08/beach.jpg").is_file());
    assert!(walkdir::WalkDir::new(lib.join("_날짜확인필요")).into_iter().flatten().any(|e| e.file_name() == "IMG_0001_small.jpg"), "uncertain date goes to review folder");
    assert!(!disk_a.join("cam1").exists(), "emptied source folders are pruned");
    assert!(disk_a.join("DCIM/notes.txt").exists(), "non-media untouched");
    // Index follows the files; a rescan finds nothing missing.
    let stats = scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    assert_eq!(stats.iter().map(|s| s.missing).sum::<u64>(), 0, "{stats:?}");
    assert_eq!(db::query_files(&conn, &FileFilter::default(), 0, 100).unwrap().len(), 8);
    // Thumbnails survive the move (keyed by id/size/mtime).
    let moved = db::get_file(&conn, kakao.id).unwrap().unwrap();
    assert!(moved.rel_path.starts_with("Library/"), "{}", moved.rel_path);
    assert!(tc.has_thumb(&moved));

    // Re-planning the organized library is a no-op.
    let plan2 = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    assert_eq!(plan2.summary.in_place, 7, "{:?}", plan2.summary);
    assert_eq!(plan2.summary.skip_duplicate, 1, "{:?}", plan2.summary);

    // --- Undo the whole organize batch -------------------------------------------------------
    let undo = journal::undo_batch(&conn, &rep.batch_id).unwrap();
    assert_eq!(undo.failed, 0, "{:?}", undo.errors);
    let after = walk_media(root);
    assert_eq!(after, originals, "every byte back where it was");
    assert!(!lib.join("2019").exists(), "library folders created by the batch are removed");
}

/// (path, blake3) of every media file under root, excluding app data.
fn walk_media(root: &Path) -> Vec<(PathBuf, String)> {
    let mut v: Vec<(PathBuf, String)> = walkdir::WalkDir::new(root)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter(|e| !e.path().starts_with(root.join("appdata")))
        .filter(|e| media_core::media::kind_for_ext(&media_core::media::ext_of(&e.file_name().to_string_lossy())).is_some())
        .filter(|e| e.metadata().map(|m| m.len() > 100).unwrap_or(false))
        .map(|e| (e.path().strip_prefix(root).unwrap().to_path_buf(), journal::hash_file(e.path()).unwrap()))
        .collect();
    v.sort();
    v
}
