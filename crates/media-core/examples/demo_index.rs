//! Register folders and scan them into an app database (what "Add folder" + "Scan" do in the UI).
//!
//!   cargo run -p media-core --example demo_index -- <data_dir> <cache_dir> <folder>...

use std::path::PathBuf;

use media_core::scanner::{self, ScanOptions};
use media_core::{dedupe, thumbs, volumes, Db, StopFlag};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let data = PathBuf::from(args.next().expect("data_dir"));
    let cache = PathBuf::from(args.next().expect("cache_dir"));
    dedupe::init_czkawka(&cache);
    let db = Db::open(data.join("library.db"))?;
    let conn = db.conn()?;
    for f in args {
        let id = volumes::add_source(&conn, std::path::Path::new(&f))?;
        println!("source {id}: {f}");
    }
    let stop = StopFlag::new();
    let quiet = |_p: media_core::Progress| {};
    let stats = scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet)?;
    println!("scan: {}", serde_json::to_string(&stats)?);
    let w = thumbs::warm(&db, &thumbs::ThumbCache::new(&cache), &stop, &quiet)?;
    println!("thumbs: {}", serde_json::to_string(&w)?);
    Ok(())
}
