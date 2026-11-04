//! Place names from GPS and automatic trips, all offline.
//!
//!   * place — nearest GeoNames town of 1000+ people (`reverse_geocoder`, CC-BY 4.0 data) for every
//!             photo/video with GPS, stored in `places` so search and the viewer can use it.
//!   * trips — per year, "home" is the town with GPS photos on the most different days; a run of
//!             photos far from home with no long gap is a trip ("2024년 6월 Honolulu 여행").
//!
//! Everything comes from the database, so it also covers drives that are not connected.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use anyhow::Result;
use chrono::Datelike;
use reverse_geocoder::ReverseGeocoder;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::meta;

/// Farther than this from home = away (50 miles).
const AWAY_KM: f64 = 80.0;
/// A longer pause between two away photos ends the trip.
const TRIP_GAP_SECS: i64 = 36 * 3600;
/// Minimum photos in a trip (copies counted once), and how many of them must have GPS.
const TRIP_MIN_FILES: usize = 10;
const TRIP_MIN_LOCATED: usize = 3;
/// Nearest town farther than this (sea, wilderness): keep only the state / region.
const TOWN_MAX_KM: f64 = 50.0;

fn geocoder() -> &'static ReverseGeocoder {
    static G: OnceLock<ReverseGeocoder> = OnceLock::new();
    G.get_or_init(ReverseGeocoder::new)
}

pub fn haversine_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (la1, lo1, la2, lo2) = (a.0.to_radians(), a.1.to_radians(), b.0.to_radians(), b.1.to_radians());
    let h = ((la2 - la1) / 2.0).sin().powi(2) + la1.cos() * la2.cos() * ((lo2 - lo1) / 2.0).sin().powi(2);
    2.0 * 6371.0 * h.sqrt().asin()
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct Place {
    pub city: String,
    pub admin1: String,
    pub cc: String,
}

impl Place {
    /// "Seattle, Washington" (US) / "Paris, Île-de-France, FR".
    pub fn label(&self) -> String {
        label(&self.city, &self.admin1, &self.cc)
    }
}

pub fn label(city: &str, admin1: &str, cc: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !city.is_empty() {
        parts.push(city);
    }
    if !admin1.is_empty() && admin1 != city {
        parts.push(admin1);
    }
    if !cc.is_empty() && cc != "US" {
        parts.push(cc);
    }
    parts.join(", ")
}

fn valid(lat: f64, lon: f64) -> bool {
    lat.is_finite() && lon.is_finite() && lat.abs() <= 90.0 && lon.abs() <= 180.0 && !(lat == 0.0 && lon == 0.0)
}

/// Nearest town for a coordinate.
pub fn lookup(lat: f64, lon: f64) -> Option<(Place, f64)> {
    if !valid(lat, lon) {
        return None;
    }
    let r = geocoder().search((lat, lon)).record;
    let dist = haversine_km((lat, lon), (r.lat, r.lon));
    let city = if dist <= TOWN_MAX_KM { r.name.clone() } else { String::new() };
    Some((Place { city, admin1: r.admin1.clone(), cc: r.cc.clone() }, dist))
}

// ---------------------------------------------------------------------------------------------
// Refresh: geocode new files, rebuild trips
// ---------------------------------------------------------------------------------------------

/// One timed file, input to trip detection.
#[derive(Clone, Debug)]
pub struct Item {
    pub id: i64,
    pub taken: i64,
    pub size: i64,
    pub photo: bool,
    pub loc: Option<(f64, f64)>,
    pub place: Option<Place>,
    pub sharpness: f64,
}

#[derive(Clone, Debug)]
pub struct Trip {
    pub title: String,
    pub start: i64,
    pub end: i64,
    pub count: usize,
    pub place: Place,
    pub cover: i64,
    pub files: Vec<i64>,
}

#[derive(Clone, Debug)]
pub struct Home {
    pub place: Place,
    pub loc: (f64, f64),
}

fn year_of(ts: i64) -> i32 {
    meta::from_ts(ts).map(|d| d.year()).unwrap_or(0)
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

/// Home per year: the town with GPS photos on the most different days (a week-long trip with many
/// photos still loses to everyday life), at the median of those photos' coordinates.
pub fn homes(items: &[Item]) -> HashMap<i32, Home> {
    let mut days: HashMap<(i32, Place), HashSet<i64>> = HashMap::new();
    for it in items {
        if let (Some(_), Some(p)) = (it.loc, &it.place) {
            days.entry((year_of(it.taken), p.clone())).or_default().insert(it.taken.div_euclid(86_400));
        }
    }
    let mut best: HashMap<i32, (usize, Place)> = HashMap::new();
    for ((y, p), d) in days {
        let e = best.entry(y).or_insert((0, Place::default()));
        if d.len() > e.0 || (d.len() == e.0 && p.label() < e.1.label()) {
            *e = (d.len(), p);
        }
    }
    best.into_iter()
        .map(|(y, (_, p))| {
            let (mut la, mut lo): (Vec<f64>, Vec<f64>) = items
                .iter()
                .filter(|it| year_of(it.taken) == y && it.place.as_ref() == Some(&p))
                .filter_map(|it| it.loc)
                .unzip();
            (y, Home { loc: (median(&mut la), median(&mut lo)), place: p })
        })
        .collect()
}

/// Home for a year, borrowing the nearest year that has one.
fn home_for(homes: &HashMap<i32, Home>, year: i32) -> Option<&Home> {
    homes.get(&year).or_else(|| homes.iter().min_by_key(|(y, _)| ((**y - year).abs(), **y)).map(|(_, h)| h))
}

/// Trips in time-sorted items.
pub fn detect_trips(items: &[Item], homes: &HashMap<i32, Home>) -> Vec<Trip> {
    let mut trips = Vec::new();
    // (first index, last away index) of the run being built
    let mut cur: Option<(usize, usize)> = None;
    for (i, it) in items.iter().enumerate() {
        let Some(loc) = it.loc else { continue };
        let Some(home) = home_for(homes, year_of(it.taken)) else { continue };
        let away = haversine_km(loc, home.loc) > AWAY_KM;
        cur = match cur {
            Some((s, last)) if away && it.taken - items[last].taken <= TRIP_GAP_SECS => Some((s, i)),
            Some((s, last)) => {
                trips.extend(make_trip(&items[s..=last], home));
                away.then_some((i, i))
            }
            None => away.then_some((i, i)),
        };
    }
    if let Some((s, last)) = cur {
        if let Some(home) = home_for(homes, year_of(items[s].taken)) {
            trips.extend(make_trip(&items[s..=last], home));
        }
    }
    trips
}

fn make_trip(members: &[Item], home: &Home) -> Option<Trip> {
    // Backup copies (same time + size) count once.
    let mut seen = HashSet::new();
    let unique: Vec<&Item> = members.iter().filter(|it| seen.insert((it.taken, it.size))).collect();
    let located: Vec<&Place> = unique.iter().filter(|it| it.loc.is_some()).filter_map(|it| it.place.as_ref()).collect();
    if unique.len() < TRIP_MIN_FILES || located.len() < TRIP_MIN_LOCATED {
        return None;
    }
    let top = |key: &dyn Fn(&Place) -> String| -> (String, f64) {
        let mut n: HashMap<String, usize> = HashMap::new();
        for p in &located {
            *n.entry(key(p)).or_default() += 1;
        }
        let (k, c) = n.into_iter().max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0))).unwrap_or_default();
        (k, c as f64 / located.len() as f64)
    };
    let (city, city_share) = top(&|p| p.city.clone());
    let (admin1, admin_share) = top(&|p| p.admin1.clone());
    let (cc, cc_share) = top(&|p| p.cc.clone());
    let name = if city_share >= 0.6 && !city.is_empty() {
        city.clone()
    } else if admin_share >= 0.6 && !admin1.is_empty() {
        admin1.clone()
    } else if cc_share >= 0.6 && cc != home.place.cc {
        cc.clone()
    } else if !city.is_empty() {
        city.clone()
    } else {
        admin1.clone()
    };
    let start = members[0].taken;
    let title = match meta::from_ts(start) {
        Some(d) => format!("{}년 {}월 {name} 여행", d.year(), d.month()),
        None => format!("{name} 여행"),
    };
    // Cover: the sharpest analyzed photo, else the middle one.
    let cover = unique
        .iter()
        .filter(|it| it.photo && it.sharpness > 0.0)
        .max_by(|a, b| a.sharpness.total_cmp(&b.sharpness))
        .map(|it| it.id)
        .unwrap_or(unique[unique.len() / 2].id);
    Some(Trip {
        title,
        start,
        end: members[members.len() - 1].taken,
        count: unique.len(),
        place: Place { city: if city_share >= 0.6 { city } else { String::new() }, admin1, cc },
        cover,
        files: members.iter().map(|it| it.id).collect(),
    })
}

/// Geocode files that have GPS but no place yet, then rebuild trips. Cheap after the first run
/// (the town list is only loaded when there is something new to look up).
pub fn refresh(conn: &mut Connection) -> Result<()> {
    let todo: Vec<(i64, f64, f64)> = {
        let mut st = conn.prepare(
            "SELECT f.id, f.lat, f.lon FROM files f LEFT JOIN places p ON p.file_id = f.id
             WHERE p.file_id IS NULL AND f.missing = 0 AND f.lat IS NOT NULL AND f.lon IS NOT NULL
               AND ABS(f.lat) <= 90 AND ABS(f.lon) <= 180 AND NOT (f.lat = 0 AND f.lon = 0)",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let tx = conn.transaction()?;
    {
        let mut ins = tx.prepare("INSERT OR REPLACE INTO places(file_id, city, admin1, cc, source, dist_km) VALUES (?, ?, ?, ?, 'gps', ?)")?;
        for (id, lat, lon) in todo {
            if let Some((p, d)) = lookup(lat, lon) {
                ins.execute(params![id, p.city, p.admin1, p.cc, d])?;
            }
        }
    }

    let items: Vec<Item> = {
        let mut st = tx.prepare(
            "SELECT f.id, f.taken_at, f.size, f.kind IN ('photo', 'raw'), f.lat, f.lon, p.city, p.admin1, p.cc, COALESCE(a.sharpness, 0)
             FROM files f LEFT JOIN places p ON p.file_id = f.id LEFT JOIN analysis a ON a.file_id = f.id
             WHERE f.missing = 0 AND f.quarantined = 0 AND f.taken_at IS NOT NULL AND f.date_source IN ('exif', 'video', 'user')
             ORDER BY f.taken_at, f.id",
        )?;
        let rows = st
            .query_map([], |r| {
                let place = match r.get::<_, Option<String>>(6)? {
                    Some(city) => Some(Place { city, admin1: r.get(7)?, cc: r.get(8)? }),
                    None => None,
                };
                let (lat, lon): (Option<f64>, Option<f64>) = (r.get(4)?, r.get(5)?);
                Ok(Item {
                    id: r.get(0)?,
                    taken: r.get(1)?,
                    size: r.get(2)?,
                    photo: r.get(3)?,
                    loc: if place.is_some() { lat.zip(lon) } else { None },
                    place,
                    sharpness: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let homes = homes(&items);
    let trips = detect_trips(&items, &homes);

    tx.execute("DELETE FROM trip_files", [])?;
    tx.execute("DELETE FROM trips", [])?;
    {
        let mut ins_t = tx.prepare(
            "INSERT INTO trips(title, start_ts, end_ts, cc, admin1, city, count, cover_file_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )?;
        let mut ins_f = tx.prepare("INSERT OR IGNORE INTO trip_files(trip_id, file_id) VALUES (?, ?)")?;
        for t in &trips {
            ins_t.execute(params![t.title, t.start, t.end, t.place.cc, t.place.admin1, t.place.city, t.count as i64, t.cover])?;
            let tid = tx.last_insert_rowid();
            for f in &t.files {
                ins_f.execute(params![tid, f])?;
            }
        }
    }
    // Latest home, shown on the trips screen.
    let latest = homes.iter().max_by_key(|(y, _)| **y).map(|(_, h)| h.place.label()).unwrap_or_default();
    tx.execute(
        "INSERT INTO settings(key, value) VALUES ('places_home', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [latest],
    )?;
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Queries for the UI
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct TripRow {
    pub id: i64,
    pub title: String,
    pub start_ts: i64,
    pub end_ts: i64,
    pub count: i64,
    pub cover_file_id: Option<i64>,
    pub place: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct TripsSummary {
    pub trips: Vec<TripRow>,
    /// Detected home (latest year), "" if unknown.
    pub home: String,
    /// Photos/videos with a place vs all of them.
    pub located: i64,
    pub total: i64,
}

pub fn summary(conn: &Connection) -> Result<TripsSummary> {
    let mut st = conn.prepare("SELECT id, title, start_ts, end_ts, count, cover_file_id, city, admin1, cc FROM trips ORDER BY start_ts DESC")?;
    let trips = st
        .query_map([], |r| {
            let (city, admin1, cc): (Option<String>, Option<String>, Option<String>) = (r.get(6)?, r.get(7)?, r.get(8)?);
            Ok(TripRow {
                id: r.get(0)?,
                title: r.get(1)?,
                start_ts: r.get(2)?,
                end_ts: r.get(3)?,
                count: r.get(4)?,
                cover_file_id: r.get(5)?,
                place: label(city.as_deref().unwrap_or(""), admin1.as_deref().unwrap_or(""), cc.as_deref().unwrap_or("")),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let home: String = conn.query_row("SELECT value FROM settings WHERE key = 'places_home'", [], |r| r.get(0)).optional()?.unwrap_or_default();
    let (located, total): (i64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(p.file_id IS NOT NULL), 0), COUNT(*) FROM files f LEFT JOIN places p ON p.file_id = f.id
         WHERE f.missing = 0 AND f.quarantined = 0",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(TripsSummary { trips, home, located, total })
}

/// "Seattle, Washington" for one file, if it has a place.
pub fn place_of(conn: &Connection, file_id: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT city, admin1, cc FROM places WHERE file_id = ?", [file_id], |r| {
            Ok(label(&r.get::<_, String>(0)?, &r.get::<_, String>(1)?, &r.get::<_, String>(2)?))
        })
        .optional()?)
}

/// Trip title per file (for the `{trip}` folder token).
pub fn trip_titles(conn: &Connection) -> Result<HashMap<i64, String>> {
    let mut st = conn.prepare("SELECT tf.file_id, t.title FROM trip_files tf JOIN trips t ON t.id = tf.trip_id")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<HashMap<_, _>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAN_JOSE: (f64, f64) = (37.3382, -121.8863);
    const HONOLULU: (f64, f64) = (21.3069, -157.8583);
    const MAUI: (f64, f64) = (20.8893, -156.4729);
    const SANTA_CRUZ: (f64, f64) = (36.9741, -122.0308); // ~45 km from San Jose

    fn ts(y: i32, m: u32, d: u32, h: u32) -> i64 {
        meta::to_ts(chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, 0, 0).unwrap())
    }

    fn item(id: i64, taken: i64, loc: Option<(f64, f64)>) -> Item {
        Item { id, taken, size: 1000 + id, photo: true, loc, place: loc.and_then(|(a, b)| lookup(a, b)).map(|p| p.0), sharpness: 0.0 }
    }

    /// Everyday life at home: one photo on each of 40 days of 2024.
    fn everyday(start_id: i64) -> Vec<Item> {
        (0..40).map(|d| item(start_id + d, ts(2024, 1, 1, 12) + d * 2 * 86_400, Some(SAN_JOSE))).collect()
    }

    fn sorted(mut v: Vec<Item>) -> Vec<Item> {
        v.sort_by_key(|it| (it.taken, it.id));
        v
    }

    #[test]
    fn names_places() {
        assert_eq!(lookup(47.6062, -122.3321).unwrap().0.label(), "Seattle, Washington");
        let h = lookup(HONOLULU.0, HONOLULU.1).unwrap().0;
        assert_eq!((h.admin1.as_str(), h.cc.as_str()), ("Hawaii", "US"));
        let p = lookup(48.8566, 2.3522).unwrap().0;
        assert_eq!(p.cc, "FR");
        assert!(p.label().ends_with(", FR"), "{}", p.label());
        assert!(lookup(0.0, 0.0).is_none());
        // Mid-Pacific: no town nearby, region only.
        assert!(lookup(30.0, -140.0).unwrap().0.city.is_empty());
        assert!((haversine_km(SAN_JOSE, SANTA_CRUZ) - 45.0).abs() < 5.0);
    }

    #[test]
    fn home_is_where_the_days_are() {
        // 40 everyday days at home vs a 4-day trip with 80 photos.
        let mut v = everyday(0);
        for i in 0..80 {
            v.push(item(1000 + i, ts(2024, 6, 10, 9) + i * 3600, Some(HONOLULU)));
        }
        let h = homes(&sorted(v));
        assert_eq!(h[&2024].place.city, "San Jose");
    }

    #[test]
    fn finds_trips() {
        let mut v = everyday(0);
        // Hawaii: 4 days, 20 photos (+ 3 without GPS during the trip, + backup copies of 5).
        for i in 0..20 {
            v.push(item(100 + i, ts(2024, 6, 10, 8) + i * 4 * 3600, Some(if i < 14 { HONOLULU } else { MAUI })));
        }
        for i in 0..3 {
            v.push(item(200 + i, ts(2024, 6, 11, 10) + i * 60, None));
        }
        for i in 0..5 {
            let mut c = item(300 + i, ts(2024, 6, 10, 8) + i * 4 * 3600, Some(HONOLULU));
            c.size = 1000 + 100 + i; // same time + size as the original = a copy
            v.push(c);
        }
        // Day trip to Santa Cruz (< 80 km): not a trip.
        for i in 0..15 {
            v.push(item(400 + i, ts(2024, 8, 3, 9) + i * 600, Some(SANTA_CRUZ)));
        }
        // Two Hawaii visits 3 days apart with nothing in between: two trips.
        for i in 0..12 {
            v.push(item(500 + i, ts(2024, 10, 1, 8) + i * 3600, Some(HONOLULU)));
            v.push(item(600 + i, ts(2024, 10, 4, 8) + i * 3600, Some(HONOLULU)));
        }
        let v = sorted(v);
        let trips = detect_trips(&v, &homes(&v));
        assert_eq!(trips.len(), 3, "{trips:#?}");
        let t = &trips[0];
        assert_eq!(t.count, 23, "copies count once, GPS-less photos inside the trip count");
        assert!(t.files.contains(&201));
        assert_eq!(t.title, "2024년 6월 Honolulu 여행");
        assert!(trips[1].title.starts_with("2024년 10월"));
    }

    #[test]
    fn multi_city_trip_uses_the_state() {
        let mut v = everyday(0);
        for i in 0..12 {
            v.push(item(100 + i, ts(2024, 6, 10, 8) + i * 4 * 3600, Some(if i % 2 == 0 { HONOLULU } else { MAUI })));
        }
        let v = sorted(v);
        let trips = detect_trips(&v, &homes(&v));
        assert_eq!(trips.len(), 1);
        assert_eq!(trips[0].title, "2024년 6월 Hawaii 여행");
    }
}
