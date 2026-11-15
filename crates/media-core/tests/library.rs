//! "라이브러리로 모으기": every case of the library layout on a pretend E: (library drive) and C:.

use std::fs;
use std::path::Path;

use media_core::journal;
use media_core::library::LibraryOptions;
use media_core::organizer::{self, Action, OrganizeOptions};
use media_core::scanner::{self, ScanOptions};
use media_core::{volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

fn jpeg(p: &Path, seed: u32, date: &str) {
    write_jpeg(p, &picture(seed, 800, 600), Some((date, "Canon", "EOS R6")));
}

#[test]
fn library_layout_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let c = drive(tmp.path(), "diskC");

    // An event with sub-folders spanning the new year, a note file inside it.
    jpeg(&e.join("옛날/2023-12-28 유럽 여행/파리/a1.jpg"), 1, "2023:12:28 10:00:00");
    jpeg(&e.join("옛날/2023-12-28 유럽 여행/로마/a2.jpg"), 2, "2024:01:03 10:00:00");
    fs::write(e.join("옛날/2023-12-28 유럽 여행/notes.txt"), "itinerary").unwrap();
    // An identical copy on E (removed) and on C (left alone).
    fs::create_dir_all(e.join("옛날/backup")).unwrap();
    fs::copy(e.join("옛날/2023-12-28 유럽 여행/파리/a1.jpg"), e.join("옛날/backup/a1 copy.jpg")).unwrap();
    fs::create_dir_all(c.join("Pictures")).unwrap();
    fs::copy(e.join("옛날/2023-12-28 유럽 여행/파리/a1.jpg"), c.join("Pictures/a1.jpg")).unwrap();
    // Undated scans in a dated event folder: the folder's month, kept on re-plan.
    write_png(&e.join("옛날/2009-07 제주도/scan.png"), &picture(13, 900, 700));
    // A collection over years: sorted by date, not an event.
    jpeg(&e.join("Family/f1.jpg"), 3, "2019:03:01 10:00:00");
    jpeg(&e.join("Family/f2.jpg"), 4, "2021:07:01 10:00:00");
    // Camera dump: JPG + RAW + XMP, and a Live Photo (JPG + MOV + AAE).
    jpeg(&e.join("DCIM/100CANON/IMG_1.JPG"), 5, "2024:06:10 10:00:00");
    fs::write(e.join("DCIM/100CANON/IMG_1.CR2"), vec![7u8; 300_000]).unwrap();
    fs::write(e.join("DCIM/100CANON/IMG_1.xmp"), "<x:xmpmeta/>").unwrap();
    jpeg(&e.join("DCIM/100APPLE/IMG_7.JPG"), 6, "2024:06:11 10:00:00");
    fs::write(e.join("DCIM/100APPLE/IMG_7.MOV"), vec![1u8; 50_000]).unwrap();
    fs::write(e.join("DCIM/100APPLE/IMG_7.AAE"), "<plist/>").unwrap();
    // Received, screenshot, undated, a big video, a Google Takeout album.
    write_jpeg(&e.join("KakaoTalk/KakaoTalk_20210304_101112345.jpg"), &picture(8, 800, 600), None);
    write_png(&e.join("Screenshots/Screenshot_20230101-101010.png"), &picture(9, 1080, 2340));
    write_png(&e.join("misc/noexif.png"), &picture(10, 900, 700));
    fs::create_dir_all(e.join("Videos")).unwrap();
    fs::write(e.join("Videos/clip.mp4"), vec![2u8; 60_000]).unwrap();
    write_jpeg(&e.join("Takeout/Google 포토/Hawaii 2018/IMG_x.jpg"), &picture(11, 800, 600), None);
    fs::write(
        e.join("Takeout/Google 포토/Hawaii 2018/IMG_x.jpg.json"),
        r#"{"title":"IMG_x.jpg","photoTakenTime":{"timestamp":"1530792000"},"geoData":{"latitude":21.3,"longitude":-157.8}}"#,
    )
    .unwrap();
    // A photo only on C: copied in.
    jpeg(&c.join("Pictures/c_only.jpg"), 12, "2022:02:02 10:00:00");

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    volumes::add_source(&conn, &c).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();

    let lib = e.join("사진");
    let opts = OrganizeOptions {
        target_dir: lib.to_string_lossy().into_owned(),
        layout: "library".into(),
        library: LibraryOptions { big_video_bytes: 55_000, ..Default::default() },
        ..Default::default()
    };
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    let by_src = |rel: &str| plan.items.iter().find(|i| i.src.replace('\\', "/").ends_with(rel)).unwrap_or_else(|| panic!("{rel} not planned"));
    let dst = |rel: &str| -> String {
        let it = by_src(rel);
        it.dst.replace('\\', "/").strip_prefix(&format!("{}/", lib.to_string_lossy().replace('\\', "/"))).unwrap_or(&it.dst).to_string()
    };

    assert_eq!(dst("파리/a1.jpg"), "2023/2023-12/유럽 여행/파리/a1.jpg", "event kept whole, folder date wins");
    assert_eq!(dst("로마/a2.jpg"), "2023/2023-12/유럽 여행/로마/a2.jpg");
    assert_eq!(dst("2009-07 제주도/scan.png"), "2009/2009-07/제주도/scan.png", "undated scan follows its folder's date");
    assert_eq!(by_src("backup/a1 copy.jpg").action, Action::Remove, "copy on E removed");
    assert_eq!(by_src("diskC/Pictures/a1.jpg").action, Action::Leave, "copy on C left alone");
    assert_eq!(dst("Family/f1.jpg"), "2019/2019-03/f1.jpg", "a years-long collection is sorted by date");
    assert_eq!(dst("Family/f2.jpg"), "2021/2021-07/f2.jpg");
    assert_eq!(dst("100CANON/IMG_1.JPG"), "2024/2024-06/IMG_1.JPG");
    assert_eq!(dst("100CANON/IMG_1.CR2"), "2024/2024-06/RAW/IMG_1.CR2", "RAW under its JPG");
    assert_eq!(dst("100APPLE/IMG_7.MOV"), "2024/2024-06/IMG_7.MOV", "Live Photo video beside its photo");
    assert_eq!(dst("KakaoTalk/KakaoTalk_20210304_101112345.jpg"), "받은 사진/2021/2021-03/KakaoTalk_20210304_101112345.jpg");
    assert_eq!(dst("Screenshots/Screenshot_20230101-101010.png"), "2023/2023-01/스크린샷/Screenshot_20230101-101010.png");
    assert_eq!(dst("misc/noexif.png"), "_날짜확인필요/misc/noexif.png");
    assert_eq!(dst("Videos/clip.mp4"), "큰 동영상/_날짜확인필요/Videos/clip.mp4");
    assert_eq!(dst("Hawaii 2018/IMG_x.jpg"), "2018/2018-07/Hawaii 2018/IMG_x.jpg", "Takeout album with the JSON date");
    assert_eq!(by_src("diskC/Pictures/c_only.jpg").action, Action::Copy);
    assert_eq!(dst("diskC/Pictures/c_only.jpg"), "2022/2022-02/c_only.jpg");
    assert!(plan.summary.leftover_files >= 1, "notes.txt stays behind: {:?}", plan.summary);

    let rep = organizer::execute(&db, &plan, &stop, &quiet).unwrap();
    assert_eq!((rep.failed, rep.removed), (0, 1), "{:?}", rep.errors);
    assert!(rep.sidecars >= 3, "xmp, aae, json moved: {}", rep.sidecars);
    assert!(lib.join("2024/2024-06/RAW/IMG_1.xmp").is_file(), "xmp goes with the RAW");
    let found: Vec<_> = walkdir_files(&e).into_iter().filter(|p| p.contains("IMG_7")).collect();
    assert!(lib.join("2024/2024-06/IMG_7.AAE").is_file(), "{found:?} {:?}", rep.errors);
    assert!(lib.join("2018/2018-07/Hawaii 2018/IMG_x.jpg.json").is_file());
    assert!(lib.join("2022/2022-02/c_only.jpg").is_file());
    assert!(c.join("Pictures/c_only.jpg").is_file(), "C is left as it is");
    assert!(c.join("Pictures/a1.jpg").is_file());
    assert!(!e.join("옛날/backup/a1 copy.jpg").exists());
    assert!(e.join("옛날/2023-12-28 유럽 여행/notes.txt").is_file(), "other files stay");

    // Planning again: everything is already where it belongs.
    let again = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    assert_eq!((again.summary.moves, again.summary.copies, again.summary.remove), (0, 0, 0), "{:?}", again.summary);

    // One undo puts everything back.
    let undo = journal::undo_batch(&conn, &rep.batch_id).unwrap();
    assert_eq!(undo.failed, 0, "{undo:?}");
    assert!(e.join("옛날/2023-12-28 유럽 여행/파리/a1.jpg").is_file());
    assert!(e.join("옛날/backup/a1 copy.jpg").is_file());
    assert!(e.join("DCIM/100CANON/IMG_1.xmp").is_file());
    assert!(!lib.join("2022/2022-02/c_only.jpg").exists());
}

fn walkdir_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p.to_string_lossy().into_owned());
            }
        }
    }
    out
}
