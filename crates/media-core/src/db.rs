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

CREATE TABLE IF NOT EXISTS settings (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL
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

/// Ordered schema upgrades; index + 1 = `PRAGMA user_version` after applying.
/// Never edit a released step — append a new one. Installed apps auto-update and must keep data.
const MIGRATIONS: &[&str] = &[
    // 1: photo analysis, cleanup suggestions, places & trips, people
    r#"
    CREATE TABLE IF NOT EXISTS analysis (
        file_id      INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
        version      INTEGER NOT NULL,
        sharpness    REAL,
        dhash        INTEGER,
        face_count   INTEGER,
        face_version INTEGER NOT NULL DEFAULT 0,
        analyzed_at  INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS dismissals (
        file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
        kind    TEXT NOT NULL,
        PRIMARY KEY(file_id, kind)
    );
    CREATE TABLE IF NOT EXISTS bursts (
        file_id  INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
        burst_id INTEGER NOT NULL,
        rank     INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS bursts_id ON bursts(burst_id);
    CREATE TABLE IF NOT EXISTS places (
        file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
        city    TEXT NOT NULL,
        admin1  TEXT NOT NULL,
        cc      TEXT NOT NULL,
        source  TEXT NOT NULL,
        dist_km REAL
    );
    CREATE INDEX IF NOT EXISTS places_key ON places(cc, admin1, city);
    CREATE TABLE IF NOT EXISTS trips (
        id            INTEGER PRIMARY KEY,
        title         TEXT NOT NULL,
        start_ts      INTEGER NOT NULL,
        end_ts        INTEGER NOT NULL,
        cc            TEXT,
        admin1        TEXT,
        city          TEXT,
        count         INTEGER NOT NULL,
        cover_file_id INTEGER
    );
    CREATE TABLE IF NOT EXISTS trip_files (
        trip_id INTEGER NOT NULL REFERENCES trips(id) ON DELETE CASCADE,
        file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
        PRIMARY KEY(trip_id, file_id)
    );
    CREATE INDEX IF NOT EXISTS trip_files_file ON trip_files(file_id);
    CREATE TABLE IF NOT EXISTS persons (
        id            INTEGER PRIMARY KEY,
        name          TEXT,
        cover_face_id INTEGER,
        hidden        INTEGER NOT NULL DEFAULT 0,
        created_at    INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS faces (
        id        INTEGER PRIMARY KEY,
        file_id   INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
        x REAL NOT NULL, y REAL NOT NULL, w REAL NOT NULL, h REAL NOT NULL,
        score     REAL NOT NULL,
        sharpness REAL,
        model     TEXT NOT NULL,
        embedding BLOB NOT NULL,
        person_id INTEGER REFERENCES persons(id) ON DELETE SET NULL,
        locked    INTEGER NOT NULL DEFAULT 0,
        hidden    INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS faces_file ON faces(file_id);
    CREATE INDEX IF NOT EXISTS faces_person ON faces(person_id);
    CREATE TABLE IF NOT EXISTS face_rejects (
        face_id   INTEGER NOT NULL REFERENCES faces(id) ON DELETE CASCADE,
        person_id INTEGER NOT NULL REFERENCES persons(id) ON DELETE CASCADE,
        PRIMARY KEY(face_id, person_id)
    );
    "#,
    // 2: cleanup suggestions (recomputed after each analysis pass)
    r#"
    CREATE TABLE IF NOT EXISTS suggestions (
        file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
        kind    TEXT NOT NULL,
        detail  TEXT NOT NULL DEFAULT '',
        PRIMARY KEY(file_id, kind)
    );
    "#,
    // 3: never re-read a known file — moved/renamed/copied files are recognised by content
    // (size + mtime + 64 KiB prehash) and keep or copy everything already derived from them.
    // People: answered "different person" pairs.
    r#"
    ALTER TABLE files ADD COLUMN prehash TEXT;
    ALTER TABLE files ADD COLUMN clone_of INTEGER;
    CREATE INDEX IF NOT EXISTS files_size_mtime ON files(size, mtime);
    ALTER TABLE faces ADD COLUMN src_face INTEGER;
    CREATE TABLE IF NOT EXISTS person_pairs_no (
        a INTEGER NOT NULL,
        b INTEGER NOT NULL,
        PRIMARY KEY(a, b)
    );
    "#,
    // 4: which copy of a byte-identical file is the original (derived; rebuilt any time).
    r#"
    CREATE TABLE IF NOT EXISTS dup_roles (
        file_id    INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
        role       TEXT NOT NULL,
        group_size INTEGER NOT NULL
    );
    "#,
    // 5: broken EXIF sizes (e.g. 4294967295) made width × height overflow; forget them.
    r#"
    UPDATE files SET width = NULL, height = NULL
     WHERE width > 100000 OR height > 100000 OR width <= 0 OR height <= 0;
    "#,
    // 6: duplicate members keep their rank and, for similar photos, what kind of variant they are.
    r#"
    ALTER TABLE dup_members ADD COLUMN rank INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE dup_members ADD COLUMN variant TEXT;
    "#,
    // 7: content identity (64 KiB fingerprint, else full hash) is looked up by this expression.
    r#"
    CREATE INDEX IF NOT EXISTS files_content ON files(COALESCE(prehash, hash));
    "#,
];

pub fn schema_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Apply pending migrations, each in its own transaction.
pub fn migrate(conn: &Connection) -> Result<()> {
    let mut v = schema_version(conn)? as usize;
    while v < MIGRATIONS.len() {
        // Statement by statement, so that a column a migration adds that is already there (a
        // migration run again) does not stop the upgrade.
        let tx = conn.unchecked_transaction()?;
        for stmt in MIGRATIONS[v].split(";\n").map(str::trim).filter(|s| !s.is_empty()) {
            if let Err(e) = tx.execute_batch(stmt) {
                if !e.to_string().contains("duplicate column name") {
                    return Err(anyhow::Error::from(e).context(format!("migration {}", v + 1)));
                }
            }
        }
        tx.execute_batch(&format!("PRAGMA user_version = {};", v + 1))?;
        tx.commit().with_context(|| format!("migration {}", v + 1))?;
        v += 1;
    }
    Ok(())
}

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
        migrate(&conn).context("upgrading database")?;
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
    /// The indexed file this one is a byte-identical copy of (its thumbnails can be reused).
    #[serde(default)]
    pub clone_of: Option<i64>,
}

/// Number of columns in `FILE_COLS` (extra columns selected after it start at this index).
pub const FILE_COLS_LEN: usize = 23;

/// SQLite "dirname" of `f.rel_path` (volume-relative, no trailing slash, "" at the volume root).
/// rtrim(path, <all non-slash chars of path>) strips the file name, then the trailing slash.
pub const DIR_SQL: &str = "rtrim(rtrim(f.rel_path, replace(f.rel_path, '/', '')), '/')";

pub const FILE_COLS: &str = "f.id, f.volume_id, f.rel_path, f.name, f.ext, f.kind, f.size, f.mtime, f.taken_at, \
    f.date_source, f.width, f.height, f.duration_ms, f.camera, f.lat, f.lon, f.orientation, f.hash, f.missing, \
    f.quarantined, v.mount, v.online, f.clone_of";

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
            clone_of: r.get(22)?,
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
    pub size: i64,
    pub mtime: i64,
    pub name: String,
    /// Photos in this file's burst (None if not in a burst) and this file's rank (0 = best).
    pub burst_size: Option<i64>,
    pub burst_rank: Option<i64>,
    pub burst_id: Option<i64>,
    /// Byte-identical copies of this file on other drives (backup check).
    pub copies: i64,
    /// Marked ♥ (the user label "favorite").
    pub favorite: bool,
    /// "original" / "copy" when byte-identical copies of this file exist (see `identity::refresh_roles`).
    pub dup_role: Option<String>,
}

/// SQL: file `g` has the same content as file `f` — the full hashes when both are known,
/// otherwise the 64 KiB content fingerprint (size + first 64 KiB), which is stored at scan time.
pub const SAME_CONTENT_SQL: &str = "g.size = f.size AND (CASE WHEN g.hash IS NOT NULL AND f.hash IS NOT NULL THEN g.hash = f.hash \
    ELSE g.prehash IS NOT NULL AND g.prehash = f.prehash END)";

/// SQL: a live copy of `f` exists on another drive.
fn copy_elsewhere_sql() -> String {
    format!(
        "EXISTS (SELECT 1 FROM files g WHERE g.id != f.id AND g.volume_id != f.volume_id AND g.missing = 0 AND g.quarantined = 0 AND {SAME_CONTENT_SQL})"
    )
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
    /// Capture date range, inclusive, as wall-clock timestamps (see `meta::to_ts`).
    pub date_from: Option<i64>,
    pub date_to: Option<i64>,
    /// File size range in bytes.
    pub size_min: Option<i64>,
    pub size_max: Option<i64>,
    /// Resolution range in megapixels.
    pub mp_min: Option<f64>,
    pub mp_max: Option<f64>,
    /// Video length range in seconds.
    pub dur_min: Option<i64>,
    pub dur_max: Option<i64>,
    pub camera: Option<String>,
    pub ext: Option<String>,
    pub has_gps: Option<bool>,
    /// Files directly inside this volume-relative folder ("" = volume root). Use with `volume_id`.
    pub dir: Option<String>,
    /// Cleanup suggestion kind ("blurry" | "burst_extra" | "screenshot" | "large_video" |
    /// "short_video"), excluding
    /// ones the user dismissed.
    pub suggest: Option<String>,
    /// Only photos that are part of a burst / of this burst.
    pub in_burst: bool,
    pub burst_id: Option<i64>,
    /// Files of this automatic trip.
    pub trip_id: Option<i64>,
    /// Backup check: "elsewhere" = a copy exists on another drive, "only_here" = no copy on any
    /// other drive.
    pub copies: Option<String>,
    /// Photos in which all of these people appear.
    pub person_ids: Option<Vec<i64>>,
    /// Show each burst as its best shot only.
    pub stack_bursts: bool,
    /// 'taken' (default) | 'mtime' | 'size' | 'name' | 'resolution' | 'duration'
    pub sort: Option<String>,
    /// Default: true (newest / largest first).
    pub desc: Option<bool>,
}

impl FileFilter {
    /// ORDER BY clause for the chosen sort (always with a stable tie-breaker).
    pub fn order_sql(&self) -> String {
        let dir = if self.desc.unwrap_or(true) { "DESC" } else { "ASC" };
        let key = match self.sort.as_deref().unwrap_or("taken") {
            "mtime" => "f.mtime",
            "size" => "f.size",
            "name" => "f.name COLLATE NOCASE",
            "resolution" => "(COALESCE(f.width, 0) * COALESCE(f.height, 0))",
            "duration" => "COALESCE(f.duration_ms, 0)",
            _ => "f.taken_at",
        };
        format!("{key} {dir}, f.id {dir}")
    }

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
            w.push("(f.rel_path LIKE ? ESCAPE '\\' OR f.camera LIKE ? ESCAPE '\\' OR EXISTS (SELECT 1 FROM labels l WHERE l.file_id = f.id AND l.label LIKE ? ESCAPE '\\') \
                    OR EXISTS (SELECT 1 FROM places pl WHERE pl.file_id = f.id AND (pl.city LIKE ? ESCAPE '\\' OR pl.admin1 LIKE ? ESCAPE '\\')) \
                    OR EXISTS (SELECT 1 FROM faces fn JOIN persons pn ON pn.id = fn.person_id WHERE fn.file_id = f.id AND pn.name LIKE ? ESCAPE '\\'))".into());
            let pat = format!("%{}%", t.trim().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
            for _ in 0..6 {
                p.push(Value::Text(pat.clone()));
            }
        }
        if let Some(c) = &self.date_confidence {
            let set = match c.as_str() {
                "high" => "('exif','video','user')",
                "medium" => "('filename')",
                _ => "('folder','mtime')",
            };
            w.push(format!("f.date_source IN {set}"));
        }
        if let Some(v) = self.date_from {
            w.push("f.taken_at >= ?".into());
            p.push(Value::Integer(v));
        }
        if let Some(v) = self.date_to {
            w.push("f.taken_at <= ?".into());
            p.push(Value::Integer(v));
        }
        if let Some(v) = self.size_min {
            w.push("f.size >= ?".into());
            p.push(Value::Integer(v));
        }
        if let Some(v) = self.size_max {
            w.push("f.size <= ?".into());
            p.push(Value::Integer(v));
        }
        if let Some(v) = self.mp_min {
            w.push("COALESCE(f.width, 0) * COALESCE(f.height, 0) >= ?".into());
            p.push(Value::Integer((v * 1_000_000.0) as i64));
        }
        if let Some(v) = self.mp_max {
            w.push("COALESCE(f.width, 0) * COALESCE(f.height, 0) <= ?".into());
            p.push(Value::Integer((v * 1_000_000.0) as i64));
        }
        if let Some(v) = self.dur_min {
            w.push("f.duration_ms >= ?".into());
            p.push(Value::Integer(v * 1000));
        }
        if let Some(v) = self.dur_max {
            w.push("f.duration_ms <= ?".into());
            p.push(Value::Integer(v * 1000));
        }
        if let Some(c) = self.camera.as_ref().filter(|c| !c.is_empty()) {
            w.push("f.camera = ?".into());
            p.push(Value::Text(c.clone()));
        }
        if let Some(e) = self.ext.as_ref().filter(|e| !e.is_empty()) {
            // "jpg" also matches "jpeg"/"jpe"/"jfif"; "tif" matches "tiff"; "heic" matches "heif".
            let group: &[&str] = match e.to_lowercase().as_str() {
                "jpg" | "jpeg" => &["jpg", "jpeg", "jpe", "jfif"],
                "tif" | "tiff" => &["tif", "tiff"],
                "heic" | "heif" => &["heic", "heif"],
                _ => &[],
            };
            if group.is_empty() {
                w.push("f.ext = ?".into());
                p.push(Value::Text(e.to_lowercase()));
            } else {
                w.push(format!("f.ext IN ({})", group.iter().map(|g| format!("'{g}'")).collect::<Vec<_>>().join(",")));
            }
        }
        if let Some(d) = &self.dir {
            w.push(format!("{DIR_SQL} = ?"));
            p.push(Value::Text(d.trim_matches('/').to_string()));
        }
        if let Some(k) = &self.suggest {
            // Screenshots and large videos come straight from labels / sizes; the rest from `suggestions`.
            w.push(match k.as_str() {
                "screenshot" => "EXISTS (SELECT 1 FROM labels l WHERE l.file_id = f.id AND l.label = 'screenshot')".to_string(),
                "large_video" => format!("f.kind = 'video' AND f.size >= {}", crate::cleanup::LARGE_VIDEO_BYTES),
                "short_video" => format!("f.kind = 'video' AND f.duration_ms > 0 AND f.duration_ms < {}", crate::cleanup::SHORT_VIDEO_MS),
                _ => {
                    p.push(Value::Text(k.clone()));
                    "EXISTS (SELECT 1 FROM suggestions s WHERE s.file_id = f.id AND s.kind = ?)".to_string()
                }
            });
            w.push("NOT EXISTS (SELECT 1 FROM dismissals d WHERE d.file_id = f.id AND d.kind = ?)".into());
            p.push(Value::Text(k.clone()));
        }
        if self.in_burst {
            w.push("EXISTS (SELECT 1 FROM bursts b WHERE b.file_id = f.id)".into());
        }
        for pid in self.person_ids.iter().flatten() {
            w.push("EXISTS (SELECT 1 FROM faces fp WHERE fp.file_id = f.id AND fp.person_id = ?)".into());
            p.push(Value::Integer(*pid));
        }
        match self.copies.as_deref() {
            Some("elsewhere") => w.push(copy_elsewhere_sql()),
            Some("only_here") => w.push(format!("NOT {}", copy_elsewhere_sql())),
            _ => {}
        }
        if let Some(t) = self.trip_id {
            w.push("EXISTS (SELECT 1 FROM trip_files tf WHERE tf.file_id = f.id AND tf.trip_id = ?)".into());
            p.push(Value::Integer(t));
        }
        if let Some(b) = self.burst_id {
            w.push("EXISTS (SELECT 1 FROM bursts b WHERE b.file_id = f.id AND b.burst_id = ?)".into());
            p.push(Value::Integer(b));
        }
        if self.stack_bursts {
            w.push("NOT EXISTS (SELECT 1 FROM bursts b WHERE b.file_id = f.id AND b.rank > 0)".into());
        }
        match self.has_gps {
            Some(true) => w.push("f.lat IS NOT NULL AND f.lon IS NOT NULL".into()),
            Some(false) => w.push("(f.lat IS NULL OR f.lon IS NULL)".into()),
            None => {}
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
         ORDER BY {} LIMIT {limit} OFFSET {offset}",
        filter.order_sql()
    );
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map(params_from_iter(p), FileRow::from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn query_grid(conn: &Connection, filter: &FileFilter, offset: i64, limit: i64) -> Result<Vec<GridItem>> {
    let (w, p) = filter.to_sql(conn)?;
    let sql = format!(
        "SELECT f.id, f.kind, f.taken_at, f.width, f.height, f.duration_ms, v.online, f.size, f.mtime, f.name,
                (SELECT COUNT(*) FROM bursts b2 WHERE b2.burst_id = bb.burst_id), bb.rank, bb.burst_id,
                (SELECT COUNT(*) FROM files g WHERE g.id != f.id AND g.volume_id != f.volume_id AND g.missing = 0 AND g.quarantined = 0 AND {same}),
                EXISTS (SELECT 1 FROM labels lf WHERE lf.file_id = f.id AND lf.label = 'favorite'),
                (SELECT dr.role FROM dup_roles dr WHERE dr.file_id = f.id)
         FROM files f JOIN volumes v ON v.id = f.volume_id LEFT JOIN bursts bb ON bb.file_id = f.id WHERE {w}
         ORDER BY {} LIMIT {limit} OFFSET {offset}",
        filter.order_sql(),
        same = SAME_CONTENT_SQL,
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
                size: r.get(7)?,
                mtime: r.get(8)?,
                name: r.get(9)?,
                burst_size: r.get::<_, Option<i64>>(10)?.filter(|n| *n > 0),
                burst_rank: r.get(11)?,
                burst_id: r.get(12)?,
                copies: r.get(13)?,
                favorite: r.get::<_, i64>(14)? != 0,
                dup_role: r.get(15)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn query_ids(conn: &Connection, filter: &FileFilter) -> Result<Vec<i64>> {
    let (w, p) = filter.to_sql(conn)?;
    let sql = format!("SELECT f.id FROM files f JOIN volumes v ON v.id = f.volume_id WHERE {w} ORDER BY {}", filter.order_sql());
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

// ---------------------------------------------------------------------------------------------
// Filter helpers for the UI
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct ValueCount {
    pub value: String,
    pub count: i64,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct FilterOptions {
    pub cameras: Vec<ValueCount>,
    pub exts: Vec<ValueCount>,
    pub years: Vec<i32>,
    pub max_size: i64,
    pub with_gps: i64,
}

/// Values that actually exist in the library, so dropdowns never offer empty choices.
pub fn filter_options(conn: &Connection) -> Result<FilterOptions> {
    let live = "f.missing = 0 AND f.quarantined = 0";
    let vc = |sql: &str| -> Result<Vec<ValueCount>> {
        let mut st = conn.prepare(sql)?;
        let rows = st.query_map([], |r| Ok(ValueCount { value: r.get(0)?, count: r.get(1)? }))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    };
    let cameras = vc(&format!(
        "SELECT f.camera, COUNT(*) FROM files f WHERE {live} AND f.camera IS NOT NULL AND f.camera != '' GROUP BY f.camera ORDER BY COUNT(*) DESC"
    ))?;
    let exts = vc(&format!(
        "SELECT CASE WHEN f.ext IN ('jpeg','jpe','jfif') THEN 'jpg' WHEN f.ext = 'tiff' THEN 'tif' WHEN f.ext = 'heif' THEN 'heic' ELSE f.ext END e,
                COUNT(*) FROM files f WHERE {live} GROUP BY e ORDER BY COUNT(*) DESC"
    ))?;
    let years: Vec<i32> = {
        let mut st = conn.prepare(&format!(
            "SELECT DISTINCT CAST(strftime('%Y', f.taken_at, 'unixepoch') AS INTEGER) y FROM files f WHERE {live} AND f.taken_at IS NOT NULL ORDER BY y DESC"
        ))?;
        let rows = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i32>>>()?;
        rows
    };
    let (max_size, with_gps): (i64, i64) = conn.query_row(
        &format!("SELECT COALESCE(MAX(f.size), 0), COALESCE(SUM(f.lat IS NOT NULL), 0) FROM files f WHERE {live}"),
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(FilterOptions { cameras, exts, years, max_size, with_gps })
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct FilteredStats {
    pub count: i64,
    pub bytes: i64,
    pub photos: i64,
    pub videos: i64,
    pub raws: i64,
    pub first_taken: Option<i64>,
    pub last_taken: Option<i64>,
}

/// Count and total size of whatever the current filter matches.
pub fn filtered_stats(conn: &Connection, filter: &FileFilter) -> Result<FilteredStats> {
    let (w, p) = filter.to_sql(conn)?;
    let sql = format!(
        "SELECT COUNT(*), COALESCE(SUM(f.size), 0), COALESCE(SUM(f.kind = 'photo'), 0), COALESCE(SUM(f.kind = 'video'), 0),
                COALESCE(SUM(f.kind = 'raw'), 0), MIN(f.taken_at), MAX(f.taken_at)
         FROM files f JOIN volumes v ON v.id = f.volume_id WHERE {w}"
    );
    Ok(conn.query_row(&sql, params_from_iter(p), |r| {
        Ok(FilteredStats {
            count: r.get(0)?,
            bytes: r.get(1)?,
            photos: r.get(2)?,
            videos: r.get(3)?,
            raws: r.get(4)?,
            first_taken: r.get(5)?,
            last_taken: r.get(6)?,
        })
    })?)
}

// ---------------------------------------------------------------------------------------------
// Groups (month / year / folder) for the library grid
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Default)]
pub struct GroupRow {
    /// Stable key, e.g. "m:2024-6", "y:2024", "d:3:DCIM/100CANON", "all".
    pub key: String,
    pub count: i64,
    pub bytes: i64,
    pub year: Option<i32>,
    pub month: Option<u32>,
    pub volume_id: Option<i64>,
    pub dir: Option<String>,
    pub mount: Option<String>,
    pub volume_label: Option<String>,
    pub burst_id: Option<i64>,
    /// Earliest capture time in the group (burst groups).
    pub first_taken: Option<i64>,
}

/// Groups that the current filter produces, in display order, with exact counts.
/// `group_by`: "month" | "year" | "folder" | anything else = one group.
pub fn group_summary(conn: &Connection, filter: &FileFilter, group_by: &str) -> Result<Vec<GroupRow>> {
    let (w, p) = filter.to_sql(conn)?;
    // Newest first, unless the user sorts by capture date ascending.
    let time_dir = if filter.sort.as_deref().unwrap_or("taken") == "taken" && filter.desc == Some(false) { "ASC" } else { "DESC" };
    let rows = match group_by {
        "month" | "year" => {
            let with_month = group_by == "month";
            let sql = format!(
                "SELECT CAST(strftime('%Y', f.taken_at, 'unixepoch') AS INTEGER) y,
                        {m} m, COUNT(*), COALESCE(SUM(f.size), 0)
                 FROM files f JOIN volumes v ON v.id = f.volume_id WHERE {w}
                 GROUP BY y, m ORDER BY y {time_dir}, m {time_dir}",
                m = if with_month { "CAST(strftime('%m', f.taken_at, 'unixepoch') AS INTEGER)" } else { "0" }
            );
            let mut st = conn.prepare(&sql)?;
            let rows = st
                .query_map(params_from_iter(p), |r| {
                    let y: Option<i32> = r.get(0)?;
                    let m: Option<u32> = r.get(1)?;
                    let month = if with_month { m } else { None };
                    Ok(GroupRow {
                        key: match month {
                            Some(mm) => format!("m:{}-{mm}", y.unwrap_or(0)),
                            None => format!("y:{}", y.unwrap_or(0)),
                        },
                        count: r.get(2)?,
                        bytes: r.get(3)?,
                        year: y,
                        month,
                        volume_id: None,
                        dir: None,
                        mount: None,
                        volume_label: None,
                        ..Default::default()
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        }
        "burst" => {
            let sql = format!(
                "SELECT b.burst_id, COUNT(*), COALESCE(SUM(f.size), 0), MIN(f.taken_at)
                 FROM files f JOIN volumes v ON v.id = f.volume_id JOIN bursts b ON b.file_id = f.id WHERE {w}
                 GROUP BY b.burst_id ORDER BY MIN(f.taken_at) {time_dir}"
            );
            let mut st = conn.prepare(&sql)?;
            let rows = st
                .query_map(params_from_iter(p), |r| {
                    let bid: i64 = r.get(0)?;
                    let first: Option<i64> = r.get(3)?;
                    Ok(GroupRow {
                        key: format!("b:{bid}"),
                        count: r.get(1)?,
                        bytes: r.get(2)?,
                        year: None,
                        month: None,
                        burst_id: Some(bid),
                        first_taken: first,
                        ..Default::default()
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        }
        "folder" => {
            let sql = format!(
                "SELECT f.volume_id, {DIR_SQL} d, v.mount, v.label, COUNT(*), COALESCE(SUM(f.size), 0)
                 FROM files f JOIN volumes v ON v.id = f.volume_id WHERE {w}
                 GROUP BY f.volume_id, d ORDER BY v.mount COLLATE NOCASE, d COLLATE NOCASE"
            );
            let mut st = conn.prepare(&sql)?;
            let rows = st
                .query_map(params_from_iter(p), |r| {
                    let vid: i64 = r.get(0)?;
                    let d: String = r.get(1)?;
                    Ok(GroupRow {
                        key: format!("d:{vid}:{d}"),
                        count: r.get(4)?,
                        bytes: r.get(5)?,
                        year: None,
                        month: None,
                        volume_id: Some(vid),
                        dir: Some(d),
                        mount: Some(r.get(2)?),
                        volume_label: Some(r.get(3)?),
                        ..Default::default()
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        }
        _ => {
            let st = filtered_stats(conn, filter)?;
            if st.count == 0 {
                Vec::new()
            } else {
                vec![GroupRow {
                    key: "all".into(),
                    count: st.count,
                    bytes: st.bytes,
                    year: None,
                    month: None,
                    volume_id: None,
                    dir: None,
                    mount: None,
                    volume_label: None,
                    ..Default::default()
                }]
            }
        }
    };
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_old_database_and_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            // A v0.1 database: base schema only, user_version 0, one indexed file.
            let c = Connection::open(&path).unwrap();
            c.execute_batch(SCHEMA).unwrap();
            c.execute("INSERT INTO volumes(uid, label, mount) VALUES ('mount:/x', 'x', '/x')", []).unwrap();
            c.execute("INSERT INTO files(volume_id, rel_path, name, ext, kind, size, mtime) VALUES (1, 'a.jpg', 'a.jpg', 'jpg', 'photo', 10, 0)", []).unwrap();
            assert_eq!(schema_version(&c).unwrap(), 0);
        }
        let db = Db::open(&path).unwrap();
        let c = db.conn().unwrap();
        assert_eq!(schema_version(&c).unwrap() as usize, MIGRATIONS.len());
        let n: i64 = c.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "existing data survives");
        for t in ["analysis", "bursts", "places", "trips", "trip_files", "dismissals", "persons", "faces", "face_rejects"] {
            c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get::<_, i64>(0)).unwrap_or_else(|e| panic!("{t}: {e}"));
        }
        // Opening again is a no-op.
        drop(c);
        Db::open(&path).unwrap();
        // Deleting a file cascades to its analysis.
        let c = db.conn().unwrap();
        c.execute("INSERT INTO analysis(file_id, version, analyzed_at) VALUES (1, 1, 0)", []).unwrap();
        c.execute("DELETE FROM files WHERE id = 1", []).unwrap();
        let n: i64 = c.query_row("SELECT COUNT(*) FROM analysis", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn dirname_sql() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE f(rel_path TEXT)").unwrap();
        let cases = [
            ("a.jpg", ""),
            ("DCIM/a.jpg", "DCIM"),
            ("DCIM/100CANON/IMG_0001.JPG", "DCIM/100CANON"),
            ("2015-08 여행/해변 사진.jpg", "2015-08 여행"),
            ("x.y/z.w/file.name.with.dots.png", "x.y/z.w"),
        ];
        for (path, want) in cases {
            let got: String = c.query_row(&format!("SELECT {DIR_SQL} FROM (SELECT ? AS rel_path) f"), [path], |r| r.get(0)).unwrap();
            assert_eq!(got, want, "{path}");
        }
    }
}
