//! SQLite storage. Every long-running job opens its own connection (WAL mode), so UI queries
//! keep working while a scan or hash job is writing.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS volumes (
    id          INTEGER PRIMARY KEY,
    uid         TEXT NOT NULL UNIQUE,
    label       TEXT NOT NULL DEFAULT '',
    mount       TEXT NOT NULL DEFAULT '',
    removable   INTEGER NOT NULL DEFAULT 0,
    online      INTEGER NOT NULL DEFAULT 0,
    last_seen   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS sources (
    id          INTEGER PRIMARY KEY,
    volume_id   INTEGER NOT NULL REFERENCES volumes(id),
    rel_root    TEXT NOT NULL,
    last_scan   INTEGER NOT NULL DEFAULT 0,
    UNIQUE(volume_id, rel_root)
);

CREATE TABLE IF NOT EXISTS files (
    id          INTEGER PRIMARY KEY,
    volume_id   INTEGER NOT NULL REFERENCES volumes(id),
    rel_path    TEXT NOT NULL,
    name        TEXT NOT NULL,
    ext         TEXT NOT NULL,
    kind        TEXT NOT NULL,
    size        INTEGER NOT NULL,
    mtime       INTEGER NOT NULL,
    taken_at    INTEGER,
    date_source TEXT NOT NULL DEFAULT 'mtime',
    width       INTEGER,
    height      INTEGER,
    duration_ms INTEGER,
    camera      TEXT,
    lat         REAL,
    lon         REAL,
    orientation INTEGER,
    hash        TEXT,
    missing     INTEGER NOT NULL DEFAULT 0,
    quarantined INTEGER NOT NULL DEFAULT 0,
    scanned_at  INTEGER NOT NULL DEFAULT 0,
    UNIQUE(volume_id, rel_path)
);
CREATE INDEX IF NOT EXISTS files_taken ON files(taken_at DESC);
CREATE INDEX IF NOT EXISTS files_size ON files(size);
CREATE INDEX IF NOT EXISTS files_hash ON files(hash);
CREATE INDEX IF NOT EXISTS files_kind ON files(kind);

CREATE TABLE IF NOT EXISTS labels (
    file_id     INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    label       TEXT NOT NULL,
    source      TEXT NOT NULL,          -- 'rule' | 'user' | 'ml'
    PRIMARY KEY(file_id, label)
);
CREATE INDEX IF NOT EXISTS labels_label ON labels(label);

CREATE TABLE IF NOT EXISTS dup_groups (
    id          INTEGER PRIMARY KEY,
    kind        TEXT NOT NULL,          -- 'exact' | 'similar_image' | 'similar_video'
    created_at  INTEGER NOT NULL,
    resolved    INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS dup_members (
    group_id    INTEGER NOT NULL REFERENCES dup_groups(id) ON DELETE CASCADE,
    file_id     INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    keep        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(group_id, file_id)
);

CREATE TABLE IF NOT EXISTS batches (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,          -- 'dedupe' | 'organize'
    description TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    op_count    INTEGER NOT NULL DEFAULT 0,
    state       TEXT NOT NULL DEFAULT 'done'   -- 'running' | 'done' | 'undone' | 'purged'
);
CREATE TABLE IF NOT EXISTS journal (
    id          INTEGER PRIMARY KEY,
    batch_id    TEXT NOT NULL REFERENCES batches(id),
    op          TEXT NOT NULL,          -- 'move' | 'copy' | 'quarantine'
    file_id     INTEGER NOT NULL,
    src_abs     TEXT NOT NULL,
    dst_abs     TEXT NOT NULL,
    src_volume  INTEGER NOT NULL,
    src_rel     TEXT NOT NULL,
    dst_volume  INTEGER NOT NULL,
    dst_rel     TEXT NOT NULL,
    new_file_id INTEGER,
    ts          INTEGER NOT NULL,
    undone      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS journal_batch ON journal(batch_id);
"#;

#[derive(Clone)]
pub struct Db {
    path: PathBuf,
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = Self { path: path.as_ref().to_path_buf() };
        if let Some(parent) = db.path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = db.conn()?;
        conn.execute_batch(SCHEMA).context("creating schema")?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn conn(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path).with_context(|| format!("opening {}", self.path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA temp_store=MEMORY;",
        )?;
        Ok(conn)
    }
}

// ---------------------------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VolumeRow {
    pub id: i64,
    pub uid: String,
    pub label: String,
    pub mount: String,
    pub removable: bool,
    pub online: bool,
    pub last_seen: i64,
    pub file_count: i64,
    pub total_bytes: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceRow {
    pub id: i64,
    pub volume_id: i64,
    pub rel_root: String,
    pub last_scan: i64,
    pub volume_label: String,
    pub mount: String,
    pub online: bool,
    /// Absolute path at the volume's current (or last known) mount point.
    pub abs_path: String,
}

/// Full file record, as stored.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileRow {
    pub id: i64,
    pub volume_id: i64,
    pub rel_path: String,
    pub name: String,
    pub ext: String,
    pub kind: String,
    pub size: i64,
    pub mtime: i64,
    pub taken_at: Option<i64>,
    pub date_source: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_ms: Option<i64>,
    pub camera: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub orientation: Option<i64>,
    pub hash: Option<String>,
    pub missing: bool,
    pub quarantined: bool,
    /// Joined from volumes.
    pub mount: String,
    pub online: bool,
}

pub const FILE_COLS: &str = "f.id, f.volume_id, f.rel_path, f.name, f.ext, f.kind, f.size, f.mtime, f.taken_at, \
    f.date_source, f.width, f.height, f.duration_ms, f.camera, f.lat, f.lon, f.orientation, f.hash, f.missing, \
    f.quarantined, v.mount, v.online";

impl FileRow {
    pub fn from_row(r: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            volume_id: r.get(1)?,
            rel_path: r.get(2)?,
            name: r.get(3)?,
            ext: r.get(4)?,
            kind: r.get(5)?,
            size: r.get(6)?,
            mtime: r.get(7)?,
            taken_at: r.get(8)?,
            date_source: r.get(9)?,
            width: r.get(10)?,
            height: r.get(11)?,
            duration_ms: r.get(12)?,
            camera: r.get(13)?,
            lat: r.get(14)?,
            lon: r.get(15)?,
            orientation: r.get(16)?,
            hash: r.get(17)?,
            missing: r.get::<_, i64>(18)? != 0,
            quarantined: r.get::<_, i64>(19)? != 0,
            mount: r.get(20)?,
            online: r.get::<_, i64>(21)? != 0,
        })
    }

    /// Absolute path at the volume's current mount (valid only when `online`).
    pub fn abs_path(&self) -> PathBuf {
        crate::volumes::join_rel(Path::new(&self.mount), &self.rel_path)
    }
}

/// Light row for grids (keeps 500k-item timelines small in memory on the JS side).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GridItem {
    pub id: i64,
    pub kind: String,
    pub taken_at: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_ms: Option<i64>,
    pub online: bool,
}

// ---------------------------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FileFilter {
    pub kind: Option<String>,
    pub label: Option<String>,
    pub volume_id: Option<i64>,
    pub source_id: Option<i64>,
    pub year: Option<i32>,
    pub month: Option<u32>,
    pub text: Option<String>,
    /// 'high' (exif/video/user) | 'medium' (filename) | 'low' (folder/mtime)
    pub date_confidence: Option<String>,
    pub online_only: bool,
    pub ids: Option<Vec<i64>>,
}

impl FileFilter {
    /// Returns a WHERE clause (without the keyword) and its parameters.
    pub fn to_sql(&self, conn: &Connection) -> Result<(String, Vec<rusqlite::types::Value>)> {
        use rusqlite::types::Value;
        let mut w = vec!["f.missing = 0".to_string(), "f.quarantined = 0".to_string()];
        let mut p: Vec<Value> = Vec::new();
        if let Some(k) = &self.kind {
            if k == "screenshot" {
                w.push("EXISTS (SELECT 1 FROM labels l WHERE l.file_id = f.id AND l.label = 'screenshot')".into());
            } else {
                w.push("f.kind = ?".into());
                p.push(Value::Text(k.clone()));
            }
        }
        if let Some(l) = &self.label {
            w.push("EXISTS (SELECT 1 FROM labels l WHERE l.file_id = f.id AND l.label = ?)".into());
            p.push(Value::Text(l.clone()));
        }
        if let Some(v) = self.volume_id {
            w.push("f.volume_id = ?".into());
            p.push(Value::Integer(v));
        }
        if let Some(sid) = self.source_id {
            let (vid, root): (i64, String) =
                conn.query_row("SELECT volume_id, rel_root FROM sources WHERE id = ?", [sid], |r| Ok((r.get(0)?, r.get(1)?)))?;
            w.push("f.volume_id = ?".into());
            p.push(Value::Integer(vid));
            if !root.is_empty() {
                w.push("substr(f.rel_path, 1, length(?) + 1) = ? || '/'".into());
                p.push(Value::Text(root.clone()));
                p.push(Value::Text(root));
            }
        }
        if let Some(y) = self.year {
            w.push("CAST(strftime('%Y', f.taken_at, 'unixepoch') AS INTEGER) = ?".into());
            p.push(Value::Integer(y as i64));
        }
        if let Some(m) = self.month {
            w.push("CAST(strftime('%m', f.taken_at, 'unixepoch') AS INTEGER) = ?".into());
            p.push(Value::Integer(m as i64));
        }
        if let Some(t) = self.text.as_ref().filter(|t| !t.trim().is_empty()) {
            w.push("(f.rel_path LIKE ? ESCAPE '\\' OR f.camera LIKE ? ESCAPE '\\' OR EXISTS (SELECT 1 FROM labels l WHERE l.file_id = f.id AND l.label LIKE ? ESCAPE '\\'))".into());
            let pat = format!("%{}%", t.trim().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
            p.push(Value::Text(pat.clone()));
            p.push(Value::Text(pat.clone()));
            p.push(Value::Text(pat));
        }
        if let Some(c) = &self.date_confidence {
            let set = match c.as_str() {
                "high" => "('exif','video','user')",
                "medium" => "('filename')",
                _ => "('folder','mtime')",
            };
            w.push(format!("f.date_source IN {set}"));
        }
        if self.online_only {
            w.push("v.online = 1".into());
        }
        if let Some(ids) = &self.ids {
            if ids.is_empty() {
                w.push("0".into());
            } else {
                w.push(format!("f.id IN ({})", ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")));
            }
        }
        Ok((w.join(" AND "), p))
    }
}

// ---------------------------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------------------------

pub fn list_volumes(conn: &Connection) -> Result<Vec<VolumeRow>> {
    let mut st = conn.prepare(
        "SELECT v.id, v.uid, v.label, v.mount, v.removable, v.online, v.last_seen,
                (SELECT COUNT(*) FROM files f WHERE f.volume_id = v.id AND f.missing = 0 AND f.quarantined = 0),
                (SELECT COALESCE(SUM(size),0) FROM files f WHERE f.volume_id = v.id AND f.missing = 0 AND f.quarantined = 0)
         FROM volumes v ORDER BY v.mount",
    )?;
    let rows = st
        .query_map([], |r| {
            Ok(VolumeRow {
                id: r.get(0)?,
                uid: r.get(1)?,
                label: r.get(2)?,
                mount: r.get(3)?,
                removable: r.get::<_, i64>(4)? != 0,
                online: r.get::<_, i64>(5)? != 0,
                last_seen: r.get(6)?,
                file_count: r.get(7)?,
                total_bytes: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn list_sources(conn: &Connection) -> Result<Vec<SourceRow>> {
    let mut st = conn.prepare(
        "SELECT s.id, s.volume_id, s.rel_root, s.last_scan, v.label, v.mount, v.online
         FROM sources s JOIN volumes v ON v.id = s.volume_id ORDER BY v.mount, s.rel_root",
    )?;
    let rows = st
        .query_map([], |r| {
            let mount: String = r.get(5)?;
            let rel: String = r.get(2)?;
            Ok(SourceRow {
                id: r.get(0)?,
                volume_id: r.get(1)?,
                abs_path: crate::volumes::join_rel(Path::new(&mount), &rel).to_string_lossy().into_owned(),
                rel_root: rel,
                last_scan: r.get(3)?,
                volume_label: r.get(4)?,
                mount,
                online: r.get::<_, i64>(6)? != 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get_file(conn: &Connection, id: i64) -> Result<Option<FileRow>> {
    let sql = format!("SELECT {FILE_COLS} FROM files f JOIN volumes v ON v.id = f.volume_id WHERE f.id = ?");
    Ok(conn.query_row(&sql, [id], FileRow::from_row).optional()?)
}

pub fn query_files(conn: &Connection, filter: &FileFilter, offset: i64, limit: i64) -> Result<Vec<FileRow>> {
    let (w, p) = filter.to_sql(conn)?;
    let sql = format!(
        "SELECT {FILE_COLS} FROM files f JOIN volumes v ON v.id = f.volume_id WHERE {w}
         ORDER BY f.taken_at DESC, f.id DESC LIMIT {limit} OFFSET {offset}"
    );
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map(params_from_iter(p), FileRow::from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn query_grid(conn: &Connection, filter: &FileFilter, offset: i64, limit: i64) -> Result<Vec<GridItem>> {
    let (w, p) = filter.to_sql(conn)?;
    let sql = format!(
        "SELECT f.id, f.kind, f.taken_at, f.width, f.height, f.duration_ms, v.online
         FROM files f JOIN volumes v ON v.id = f.volume_id WHERE {w}
         ORDER BY f.taken_at DESC, f.id DESC LIMIT {limit} OFFSET {offset}"
    );
    let mut st = conn.prepare(&sql)?;
    let rows = st
        .query_map(params_from_iter(p), |r| {
            Ok(GridItem {
                id: r.get(0)?,
                kind: r.get(1)?,
                taken_at: r.get(2)?,
                width: r.get(3)?,
                height: r.get(4)?,
                duration_ms: r.get(5)?,
                online: r.get::<_, i64>(6)? != 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn query_ids(conn: &Connection, filter: &FileFilter) -> Result<Vec<i64>> {
    let (w, p) = filter.to_sql(conn)?;
    let sql = format!("SELECT f.id FROM files f JOIN volumes v ON v.id = f.volume_id WHERE {w} ORDER BY f.taken_at DESC, f.id DESC");
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map(params_from_iter(p), |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(rows)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonthCount {
    pub year: i32,
    pub month: u32,
    pub count: i64,
}

pub fn timeline_summary(conn: &Connection, filter: &FileFilter) -> Result<Vec<MonthCount>> {
    let (w, p) = filter.to_sql(conn)?;
    let sql = format!(
        "SELECT CAST(strftime('%Y', f.taken_at, 'unixepoch') AS INTEGER) y,
                CAST(strftime('%m', f.taken_at, 'unixepoch') AS INTEGER) m, COUNT(*)
         FROM files f JOIN volumes v ON v.id = f.volume_id WHERE {w}
         GROUP BY y, m ORDER BY y DESC, m DESC"
    );
    let mut st = conn.prepare(&sql)?;
    let rows = st
        .query_map(params_from_iter(p), |r| {
            Ok(MonthCount { year: r.get::<_, Option<i32>>(0)?.unwrap_or(0), month: r.get::<_, Option<u32>>(1)?.unwrap_or(0), count: r.get(2)? })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct LibraryStats {
    pub files: i64,
    pub bytes: i64,
    pub photos: i64,
    pub videos: i64,
    pub raws: i64,
    pub offline_files: i64,
    pub low_confidence_dates: i64,
    pub quarantined_files: i64,
    pub quarantined_bytes: i64,
}

pub fn library_stats(conn: &Connection) -> Result<LibraryStats> {
    Ok(conn.query_row(
        "SELECT
            COALESCE(SUM(CASE WHEN f.missing=0 AND f.quarantined=0 THEN 1 END),0),
            COALESCE(SUM(CASE WHEN f.missing=0 AND f.quarantined=0 THEN f.size END),0),
            COALESCE(SUM(CASE WHEN f.missing=0 AND f.quarantined=0 AND f.kind='photo' THEN 1 END),0),
            COALESCE(SUM(CASE WHEN f.missing=0 AND f.quarantined=0 AND f.kind='video' THEN 1 END),0),
            COALESCE(SUM(CASE WHEN f.missing=0 AND f.quarantined=0 AND f.kind='raw' THEN 1 END),0),
            COALESCE(SUM(CASE WHEN f.missing=0 AND f.quarantined=0 AND v.online=0 THEN 1 END),0),
            COALESCE(SUM(CASE WHEN f.missing=0 AND f.quarantined=0 AND f.date_source IN ('folder','mtime') THEN 1 END),0),
            COALESCE(SUM(CASE WHEN f.quarantined=1 THEN 1 END),0),
            COALESCE(SUM(CASE WHEN f.quarantined=1 THEN f.size END),0)
         FROM files f JOIN volumes v ON v.id = f.volume_id",
        [],
        |r| {
            Ok(LibraryStats {
                files: r.get(0)?,
                bytes: r.get(1)?,
                photos: r.get(2)?,
                videos: r.get(3)?,
                raws: r.get(4)?,
                offline_files: r.get(5)?,
                low_confidence_dates: r.get(6)?,
                quarantined_files: r.get(7)?,
                quarantined_bytes: r.get(8)?,
            })
        },
    )?)
}

pub fn set_file_hash(conn: &Connection, id: i64, hash: &str) -> Result<()> {
    conn.execute("UPDATE files SET hash = ? WHERE id = ?", params![hash, id])?;
    Ok(())
}
