//! Metadata extraction (pure Rust, no exiftool) and capture-date inference.
//!
//! Date fallback chain (ported from phockup / figfoto, extended for Korean phone exports):
//!   1. EXIF DateTimeOriginal / CreateDate (images) or track CreateDate (videos)  -> "exif" / "video"
//!   2. Date embedded in the file name (IMG_20190512_143012, KakaoTalk_..., PXL_..., WA...)  -> "filename"
//!   3. Date in a parent folder name (2019, 2019-05, 2019.05.12 제주도)  -> "folder"
//!   4. File modification time  -> "mtime"

use std::cell::RefCell;
use std::path::Path;

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike};
use nom_exif::{ExifTag, MediaKind, MediaParser, MediaSource, TrackInfoTag};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;

use crate::media::Kind;

#[derive(Clone, Debug, Default, Serialize)]
pub struct Meta {
    pub taken_at: Option<i64>,
    pub date_source: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_ms: Option<i64>,
    pub camera: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub orientation: Option<i64>,
}

/// Wall-clock time stored as if it were UTC: grouping by year/month in SQL then matches what
/// the user saw on the camera clock, regardless of the PC's time zone.
pub fn to_ts(dt: NaiveDateTime) -> i64 {
    dt.and_utc().timestamp()
}

pub fn from_ts(ts: i64) -> Option<NaiveDateTime> {
    DateTime::from_timestamp(ts, 0).map(|d| d.naive_utc())
}

fn plausible(dt: &NaiveDateTime) -> bool {
    let y = dt.year();
    let max_year = Local::now().year() + 1;
    (1971..=max_year).contains(&y)
}

thread_local! {
    static PARSER: RefCell<MediaParser> = RefCell::new(MediaParser::new());
}

#[derive(Default)]
struct Embedded {
    taken: Option<NaiveDateTime>,
    source: &'static str,
    width: Option<i64>,
    height: Option<i64>,
    duration_ms: Option<i64>,
    camera: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    orientation: Option<i64>,
}

fn exif_datetime(v: &nom_exif::EntryValue) -> Option<NaiveDateTime> {
    match v.as_datetime()? {
        // Aware: keep the photographer's local wall time.
        nom_exif::ExifDateTime::Aware(dt) => Some(dt.naive_local()),
        nom_exif::ExifDateTime::Naive(n) => Some(n),
    }
}

fn camera_string(make: Option<&str>, model: Option<&str>) -> Option<String> {
    let make = make.map(str::trim).filter(|s| !s.is_empty());
    let model = model.map(str::trim).filter(|s| !s.is_empty());
    match (make, model) {
        (Some(mk), Some(md)) if md.to_lowercase().starts_with(&mk.to_lowercase()) => Some(md.to_string()),
        (Some(mk), Some(md)) => Some(format!("{mk} {md}")),
        (None, Some(md)) => Some(md.to_string()),
        (Some(mk), None) => Some(mk.to_string()),
        _ => None,
    }
}

fn read_embedded(path: &Path) -> Option<Embedded> {
    let ms = MediaSource::open(path).ok()?;
    PARSER.with(|p| {
        let mut parser = p.borrow_mut();
        let mut e = Embedded::default();
        match ms.kind() {
            MediaKind::Image => {
                let iter = parser.parse_exif(ms).ok()?;
                let exif: nom_exif::Exif = iter.into();
                for tag in [ExifTag::DateTimeOriginal, ExifTag::CreateDate, ExifTag::ModifyDate] {
                    if let Some(dt) = exif.get(tag).and_then(exif_datetime).filter(plausible) {
                        e.taken = Some(dt);
                        e.source = "exif";
                        break;
                    }
                }
                e.width = exif
                    .get(ExifTag::ExifImageWidth)
                    .or_else(|| exif.get(ExifTag::ImageWidth))
                    .and_then(|v| v.as_u32())
                    .map(|v| v as i64);
                e.height = exif
                    .get(ExifTag::ExifImageHeight)
                    .or_else(|| exif.get(ExifTag::ImageHeight))
                    .and_then(|v| v.as_u32())
                    .map(|v| v as i64);
                e.orientation = exif.get(ExifTag::Orientation).and_then(|v| v.as_u16()).map(|v| v as i64);
                e.camera = camera_string(
                    exif.get(ExifTag::Make).and_then(|v| v.as_str()),
                    exif.get(ExifTag::Model).and_then(|v| v.as_str()),
                );
                if let Some(g) = exif.gps_info() {
                    e.lat = g.latitude_decimal();
                    e.lon = g.longitude_decimal();
                }
            }
            MediaKind::Track => {
                let info = parser.parse_track(ms).ok()?;
                if let Some(v) = info.get(TrackInfoTag::CreateDate) {
                    let dt = match v.as_datetime() {
                        // MP4/MOV store UTC; show it in the PC's local time zone.
                        Some(nom_exif::ExifDateTime::Aware(dt)) => Some(dt.with_timezone(&Local).naive_local()),
                        Some(nom_exif::ExifDateTime::Naive(n)) => Some(n),
                        None => None,
                    };
                    if let Some(dt) = dt.filter(plausible) {
                        e.taken = Some(dt);
                        e.source = "video";
                    }
                }
                e.width = info.get(TrackInfoTag::Width).and_then(|v| v.as_u32()).map(|v| v as i64);
                e.height = info.get(TrackInfoTag::Height).and_then(|v| v.as_u32()).map(|v| v as i64);
                e.duration_ms = info.get(TrackInfoTag::DurationMs).and_then(|v| v.as_u64()).map(|v| v as i64);
                e.camera = camera_string(
                    info.get(TrackInfoTag::Make).and_then(|v| v.as_str()),
                    info.get(TrackInfoTag::Model).and_then(|v| v.as_str()),
                );
                if let Some(g) = info.gps_info() {
                    e.lat = g.latitude_decimal();
                    e.lon = g.longitude_decimal();
                }
            }
        }
        Some(e)
    })
}

/// Extract everything we index for one file. Never fails: missing data just stays `None`.
pub fn extract(path: &Path, kind: Kind, mtime: i64) -> Meta {
    let mut m = Meta::default();
    let emb = std::panic::catch_unwind(|| read_embedded(path)).ok().flatten();
    if let Some(e) = emb {
        if let Some(t) = e.taken {
            m.taken_at = Some(to_ts(t));
            m.date_source = e.source.to_string();
        }
        m.width = e.width.filter(|v| *v > 0);
        m.height = e.height.filter(|v| *v > 0);
        m.duration_ms = e.duration_ms;
        m.camera = e.camera;
        m.lat = e.lat.filter(|v| v.is_finite() && *v != 0.0);
        m.lon = e.lon.filter(|v| v.is_finite() && *v != 0.0);
        m.orientation = e.orientation;
    }
    if kind != Kind::Video && (m.width.is_none() || m.height.is_none()) {
        if let Ok(sz) = imagesize::size(path) {
            m.width = Some(sz.width as i64);
            m.height = Some(sz.height as i64);
        }
    }
    // EXIF orientation 5..8 means the stored pixels are rotated 90 degrees.
    if matches!(m.orientation, Some(5..=8)) {
        std::mem::swap(&mut m.width, &mut m.height);
    }
    if m.taken_at.is_none() {
        let (ts, src) = infer_date(path, mtime);
        m.taken_at = Some(ts);
        m.date_source = src.to_string();
    }
    m
}

// ---------------------------------------------------------------------------------------------
// Date inference from names
// ---------------------------------------------------------------------------------------------

static RE_DATETIME: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?x)(?:^|[^0-9])
        ((?:19|20)\d{2}) [-_.]? (0[1-9]|1[0-2]) [-_.]? (0[1-9]|[12]\d|3[01])
        [\s_\-T.]* ([01]\d|2[0-3]) [-_.:h]? ([0-5]\d) [-_.:m]? ([0-5]\d)
        (?:[^0-9]|\d{1,3}(?:[^0-9]|$)|$)",
    )
    .unwrap()
});

static RE_DATE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:^|[^0-9])((?:19|20)\d{2})[-_.]?(0[1-9]|1[0-2])[-_.]?(0[1-9]|[12]\d|3[01])(?:[^0-9]|$)").unwrap()
});

/// 13-digit millisecond Unix timestamps used by many messengers / web downloads (2013..2027).
static RE_UNIX_MS: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?:^|[^0-9])(1[3-7]\d{11})(?:[^0-9]|$)").unwrap());

static RE_FOLDER_YMD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\D{0,20}?((?:19|20)\d{2})(?:[-_. 년]*(0?[1-9]|1[0-2])(?:[-_. 월]*(0?[1-9]|[12]\d|3[01]))?)?(?:\D|$)").unwrap()
});

pub fn date_from_filename(name: &str) -> Option<NaiveDateTime> {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    if let Some(c) = RE_DATETIME.captures(stem) {
        let n = |i: usize| c.get(i).unwrap().as_str().parse::<u32>().unwrap();
        if let Some(dt) = NaiveDate::from_ymd_opt(n(1) as i32, n(2), n(3)).and_then(|d| d.and_hms_opt(n(4), n(5), n(6))) {
            if plausible(&dt) {
                return Some(dt);
            }
        }
    }
    if let Some(c) = RE_DATE.captures(stem) {
        let n = |i: usize| c.get(i).unwrap().as_str().parse::<u32>().unwrap();
        if let Some(dt) = NaiveDate::from_ymd_opt(n(1) as i32, n(2), n(3)).and_then(|d| d.and_hms_opt(12, 0, 0)) {
            if plausible(&dt) {
                return Some(dt);
            }
        }
    }
    if let Some(c) = RE_UNIX_MS.captures(stem) {
        let ms: i64 = c[1].parse().ok()?;
        let dt = Local.timestamp_millis_opt(ms).single()?.naive_local();
        if plausible(&dt) {
            return Some(dt);
        }
    }
    None
}

pub fn date_from_folders(path: &Path) -> Option<NaiveDateTime> {
    let mut dir = path.parent();
    let mut depth = 0;
    while let Some(d) = dir {
        if depth >= 4 {
            break;
        }
        if let Some(name) = d.file_name().map(|s| s.to_string_lossy().into_owned()) {
            if let Some(c) = RE_FOLDER_YMD.captures(&name) {
                let y: i32 = c[1].parse().ok()?;
                let mo: u32 = c.get(2).map(|m| m.as_str().parse().unwrap_or(1)).unwrap_or(1);
                let da: u32 = c.get(3).map(|m| m.as_str().parse().unwrap_or(1)).unwrap_or(1);
                // "2019" alone inside a "05" folder: combine with the child month folder if any.
                if let Some(dt) = NaiveDate::from_ymd_opt(y, mo, da).and_then(|d| d.and_hms_opt(12, 0, 0)) {
                    if plausible(&dt) {
                        return Some(dt);
                    }
                }
            }
        }
        dir = d.parent();
        depth += 1;
    }
    None
}

/// Fallback chain when no embedded date exists. Returns (timestamp, source).
pub fn infer_date(path: &Path, mtime: i64) -> (i64, &'static str) {
    let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if let Some(dt) = date_from_filename(&name) {
        return (to_ts(dt), "filename");
    }
    let mtime_local = Local.timestamp_opt(mtime, 0).single().map(|d| d.naive_local());
    if let Some(dt) = date_from_folders(path) {
        // A folder date is month/day precision; if mtime falls in the same month, mtime is better.
        if let Some(m) = mtime_local {
            if m.year() == dt.year() && m.month() == dt.month() {
                return (to_ts(m), "folder");
            }
        }
        return (to_ts(dt), "folder");
    }
    (mtime_local.map(to_ts).unwrap_or(mtime), "mtime")
}

pub fn confidence(date_source: &str) -> &'static str {
    match date_source {
        "exif" | "video" | "user" => "high",
        "filename" => "medium",
        _ => "low",
    }
}

pub fn fmt_ts(ts: i64) -> String {
    from_ts(ts).map(|d| format!("{:04}-{:02}-{:02} {:02}:{:02}", d.year(), d.month(), d.day(), d.hour(), d.minute())).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ymdhms(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    #[test]
    fn filenames() {
        let cases = [
            ("IMG_20190512_143012.jpg", "2019-05-12 14:30:12"),
            ("PXL_20230101_120000123.jpg", "2023-01-01 12:00:00"),
            ("KakaoTalk_20210304_101112345.jpg", "2021-03-04 10:11:12"),
            ("Screenshot_20200102-030405_Chrome.jpg", "2020-01-02 03:04:05"),
            ("2018-07-08 09.10.11.jpg", "2018-07-08 09:10:11"),
            ("VID_20171231_235959.mp4", "2017-12-31 23:59:59"),
            ("IMG-20190512-WA0001.jpg", "2019-05-12 12:00:00"),
            ("20160606.png", "2016-06-06 12:00:00"),
        ];
        for (name, want) in cases {
            assert_eq!(date_from_filename(name), Some(ymdhms(want)), "{name}");
        }
        assert_eq!(date_from_filename("DSC01234.JPG"), None);
        assert_eq!(date_from_filename("IMG_1234.HEIC"), None);
        assert_eq!(date_from_filename("photo_99999999.jpg"), None);
    }

    #[test]
    fn folders() {
        assert_eq!(date_from_folders(Path::new("/x/2019-05 제주도/a.jpg")), Some(ymdhms("2019-05-01 12:00:00")));
        assert_eq!(date_from_folders(Path::new("/x/2015/a.jpg")), Some(ymdhms("2015-01-01 12:00:00")));
        assert_eq!(date_from_folders(Path::new("/x/2014년 3월 15일/a.jpg")), Some(ymdhms("2014-03-15 12:00:00")));
        assert_eq!(date_from_folders(Path::new("/x/Camera/a.jpg")), None);
    }
}
