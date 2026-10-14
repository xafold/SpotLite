//! The on-disk index: one SQLite file with files, their segments (with float16 vectors), and an FTS5 table. Vectors
//! live in the same database as everything else, so a crash can never leave them out of step with the rows that
//! describe them. The schema is DigUp's (version 5).
use crate::error::Result;
use crate::model::{FileKind, INDEXING_ORDER_SQL, Modality, SegmentKind};
use crate::paths;
use crate::text;
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAIN_FILE: &str = "index.sqlite";
/// The code index is a file of its own beside it: code search never moves the rest, and turning it off deletes one
/// file.
pub const CODE_FILE: &str = "code.sqlite";
const SCHEMA_VERSION: i64 = 5;

/// Keyword search's tokenizer. Marks (M*) count as letters, so Bengali vowel signs and Arabic harakat stay inside
/// their words.
const FTS_COLUMNS: &str =
    "name, body, original UNINDEXED, tokenize = \"porter unicode61 remove_diacritics 2 categories 'L* N* Co M*'\"";

/// A file as the index knows it.
#[derive(Clone, Debug)]
pub struct FileRecord {
    pub id: i64,
    pub path: String,
    pub kind: FileKind,
    pub size: i64,
    pub mtime: f64,
    pub inode: u64,
    pub device: u64,
}

/// A file the crawler decided to index.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub path: String,
    pub kind: FileKind,
    pub size: i64,
    pub mtime: f64,
    pub inode: u64,
    pub device: u64,
}

#[derive(Default, Debug)]
pub struct Counts {
    pub by_kind_and_state: BTreeMap<String, BTreeMap<String, i64>>,
    pub vectors: i64,
    pub segments: i64,
    /// Long files still being read, and their pages and chunks left.
    pub unfinished: i64,
    pub left: i64,
}

pub type Info = BTreeMap<String, String>;

pub struct Store {
    pub file: PathBuf,
    pub db: Connection,
    /// When set, `pending`, `pending_count` and `counts` see only files under these folders (the ones connected now).
    scope_sql: String,
    scope_values: Vec<Sql>,
    scoped: bool,
}

const FILE_COLUMNS: &str = "id, path, kind, size, mtime, inode, device";

fn record(row: &rusqlite::Row) -> rusqlite::Result<FileRecord> {
    Ok(FileRecord {
        id: row.get(0)?,
        path: row.get(1)?,
        kind: FileKind::parse(&row.get::<_, String>(2)?).unwrap_or(FileKind::Doc),
        size: row.get(3)?,
        mtime: row.get(4)?,
        inode: row.get::<_, i64>(5)? as u64,
        device: row.get::<_, i64>(6)? as u64,
    })
}

/// The ranges of the UNIQUE path index that hold the files under `folder`: as given, and as its real path when that's
/// spelled differently (a link).
pub fn path_ranges(folder: &str) -> Vec<(String, String)> {
    let mut spellings = vec![folder.to_string()];
    let real = paths::real_path(folder);
    if real != folder {
        spellings.push(real);
    }
    spellings.into_iter().map(|path| {
        let prefix = if path.ends_with('/') { path } else { format!("{path}/") };
        let upper = format!("{prefix}\u{10FFFF}");
        (prefix, upper)
    }).collect()
}

/// "Screenshot 2026-10-01 at 9.41.07 PM.png" → searchable words.
fn name_text(path: &str) -> String {
    paths::file_name(path).to_string()
}

/// The folders a file sits in, relative to the home folder ("Downloads Invoices"), as low-weight keywords.
fn folder_text(path: &str) -> String {
    let home = paths::home_string();
    let folder = paths::parent(path);
    let folder = folder.strip_prefix(&home).unwrap_or(folder);
    folder.split('/').filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" ")
}

/// `body` and `original` for keyword search: the normalized text, and the text as written if that differs.
pub fn keyword_text(text: &str) -> (String, Option<String>) {
    let normalized = text::normalized(text);
    let original = (normalized != text).then(|| text.to_string());
    (normalized, original)
}

pub fn half(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|&v| half::f16::from_f32(v).to_le_bytes()).collect()
}

impl Store {
    pub fn open(directory: &Path, name: &str) -> Result<Store> {
        std::fs::create_dir_all(directory)?;
        let file = directory.join(name);
        let db = Connection::open(&file)?;
        db.busy_timeout(Duration::from_secs(5))?;
        // Two processes opening a new index at once: the switch to WAL can fail at once with "database is locked"
        // rather than wait, so it's tried again for up to five seconds.
        let mut tries = 0;
        loop {
            match db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;") {
                Ok(()) => break,
                Err(_) if tries < 50 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(error.into()),
            }
        }
        let store = Store { file, db, scope_sql: String::new(),
                            scope_values: vec![], scoped: false };
        store.migrate()?;
        Ok(store)
    }

    pub fn is_code(&self) -> bool {
        self.file.file_name().is_some_and(|name| name == CODE_FILE)
    }

    /// Bytes on disk: the database and its write-ahead log.
    pub fn size_of(file: &Path) -> u64 {
        let wal = PathBuf::from(format!("{}-wal", file.display()));
        [file.to_path_buf(), wal].iter().map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum()
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 = self.db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(format!("this index was made by a newer SpotLite (schema {version})").into());
        }
        if version == SCHEMA_VERSION {
            return Ok(());
        }
        if version != 0 {
            return Err(format!("this index has an old schema ({version}); delete {} to start over",
                               self.file.display()).into());
        }
        // The shell's helpers can open the index at the same moment: one creates it while the other waits.
        self.db.busy_timeout(Duration::from_secs(60))?;
        let created = self.transaction(|| {
            let version: i64 = self.db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if version == 0 {
                self.db.execute_batch(&format!("
                    CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                    CREATE TABLE files(
                        id INTEGER PRIMARY KEY,
                        path TEXT NOT NULL UNIQUE,
                        kind TEXT NOT NULL,
                        size INTEGER NOT NULL,
                        mtime REAL NOT NULL,
                        inode INTEGER NOT NULL,
                        device INTEGER NOT NULL,
                        state TEXT NOT NULL DEFAULT 'pending',
                        detail TEXT,
                        info TEXT,
                        indexed_at REAL
                    );
                    CREATE INDEX files_inode ON files(device, inode);
                    CREATE INDEX files_state ON files(state, kind);
                    CREATE TABLE segments(
                        id INTEGER PRIMARY KEY,
                        file INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                        kind TEXT NOT NULL,
                        modality TEXT NOT NULL,
                        loc REAL,
                        loc_end REAL,
                        excerpt TEXT,
                        vec BLOB
                    );
                    CREATE INDEX segments_file ON segments(file);
                    CREATE VIRTUAL TABLE fts USING fts5({FTS_COLUMNS});
                    PRAGMA user_version = {SCHEMA_VERSION};"))?;
            }
            Ok(())
        });
        self.db.busy_timeout(Duration::from_secs(5))?;
        created
    }

    /// Runs `body` in one write transaction; rolled back if it fails.
    pub fn transaction<T>(&self, body: impl FnOnce() -> Result<T>) -> Result<T> {
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        match body() {
            Ok(value) => {
                self.db.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.db.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub fn set_scope(&mut self, scope: Option<&[String]>) {
        let Some(scope) = scope else {
            (self.scope_sql, self.scope_values, self.scoped) = (String::new(), vec![], false);
            return;
        };
        let ranges: Vec<(String, String)> = scope.iter().flat_map(|folder| path_ranges(folder)).collect();
        self.scope_sql = if ranges.is_empty() {
            " AND 0".into()
        } else {
            format!(" AND ({})", vec!["(path >= ? AND path < ?)"; ranges.len()].join(" OR "))
        };
        self.scope_values = ranges.into_iter().flat_map(|(from, to)| [Sql::Text(from), Sql::Text(to)]).collect();
        self.scoped = true;
    }

    // MARK: Meta

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT value FROM meta WHERE key = ?", [key], |row| row.get(0)).optional()?)
    }

    pub fn set_meta(&self, key: &str, value: Option<&str>) -> Result<()> {
        match value {
            Some(value) => self.db.execute(
                "INSERT INTO meta(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [key, value])?,
            None => self.db.execute("DELETE FROM meta WHERE key = ?", [key])?,
        };
        Ok(())
    }

    // MARK: Files

    /// Every file under one of `roots`.
    pub fn files_under(&self, roots: &[String]) -> Result<Vec<FileRecord>> {
        let mut out = vec![];
        let mut seen = HashSet::new();   // each file once, also where two spellings of a folder meet
        let mut statement = self.db.prepare_cached(&format!("SELECT {FILE_COLUMNS} FROM files WHERE path >= ? AND path < ?"))?;
        for root in roots {
            // A folder inside another one listed is already covered.
            if roots.iter().any(|other| crate::rules::is_inside(root, other)) {
                continue;
            }
            for (from, to) in path_ranges(root) {
                for row in statement.query_map([from, to], record)? {
                    let row = row?;
                    if seen.insert(row.id) {
                        out.push(row);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Adds a file (pending) and returns its id.
    pub fn insert(&self, file: &Candidate) -> Result<i64> {
        self.db.prepare_cached("INSERT INTO files(path, kind, size, mtime, inode, device) VALUES(?, ?, ?, ?, ?, ?)")?
            .execute(params![file.path, file.kind.name(), file.size, file.mtime, file.inode as i64, file.device as i64])?;
        let id = self.db.last_insert_rowid();
        self.db.prepare_cached("INSERT INTO segments(file, kind, modality) VALUES(?, 'name', 'none')")?.execute([id])?;
        self.db.prepare_cached("INSERT INTO fts(rowid, name, body) VALUES(?, ?, ?)")?.execute(params![
            self.db.last_insert_rowid(), text::normalized(&name_text(&file.path)), text::normalized(&folder_text(&file.path))])?;
        Ok(id)
    }

    /// The file changed on disk: drop what was extracted from it and queue it again.
    pub fn requeue_as(&self, id: i64, file: &Candidate) -> Result<()> {
        self.delete_segments(id, true)?;
        self.db.execute("UPDATE files SET kind = ?, size = ?, mtime = ?, inode = ?, device = ?, state = 'pending', \
                         detail = NULL, info = NULL, indexed_at = NULL WHERE id = ?",
                        params![file.kind.name(), file.size, file.mtime, file.inode as i64, file.device as i64, id])?;
        Ok(())
    }

    /// Read and embed the file again, as it is (a better way to read it arrived).
    fn requeue(&self, id: i64) -> Result<()> {
        self.delete_segments(id, true)?;
        self.db.execute("UPDATE files SET state = 'pending', detail = NULL, info = NULL, indexed_at = NULL WHERE id = ?",
                        [id])?;
        Ok(())
    }

    /// Files read once more because they're read better now, once per index: when OCR is there, screenshots read
    /// without it and PDFs with pages read as pictures (their words come from OCR now). Returns how many files.
    /// Also videos read before sparse keyframes were decoded in full: those that came out with no frames.
    pub fn read_again_once(&self) -> Result<usize> {
        let mut again = 0;
        if crate::ocr::available() && self.meta("linux_ocr")?.is_none() {
            again += self.requeue_once("linux_ocr",
                "SELECT id FROM files WHERE kind = 'screenshot' AND state = 'done' AND coalesce(json_extract(info, '$.ocr_chars'), '0') = '0' \
                 UNION SELECT DISTINCT file FROM segments WHERE kind = 'page_image'")?;
        }
        if self.meta("video_frames")?.is_none() {
            again += self.requeue_once("video_frames",
                "SELECT id FROM files WHERE kind = 'video' AND (state = 'skipped' OR \
                 (state = 'done' AND json_extract(info, '$.frames') = '0'))")?;
        }
        Ok(again)
    }

    /// The files `query` finds go back in the queue, and `key` is noted so it happens once.
    fn requeue_once(&self, key: &str, query: &str) -> Result<usize> {
        let files = self.db.prepare(query)?.query_map([], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        self.transaction(|| {
            for &id in &files {
                self.requeue(id)?;
            }
            self.set_meta(key, Some("1"))
        })?;
        Ok(files.len())
    }

    /// Same inode, size and date at a new path: a move or rename. Only the path changes; nothing is re-embedded.
    pub fn move_to(&self, id: i64, path: &str) -> Result<()> {
        self.db.execute("UPDATE files SET path = ? WHERE id = ?", params![path, id])?;
        self.db.execute("UPDATE fts SET name = ?, body = ? WHERE rowid IN (SELECT id FROM segments WHERE file = ? AND \
                         kind = 'name')",
                        params![text::normalized(&name_text(path)), text::normalized(&folder_text(path)), id])?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<()> {
        self.db.prepare_cached("DELETE FROM fts WHERE rowid IN (SELECT id FROM segments WHERE file = ?)")?.execute([id])?;
        self.db.prepare_cached("DELETE FROM files WHERE id = ?")?.execute([id])?;
        Ok(())
    }

    /// Removes every file that isn't under one of `roots` (folders taken out of the app). Returns how many went.
    pub fn prune(&self, roots: &[String]) -> Result<usize> {
        let prefixes: Vec<String> = roots.iter().flat_map(|root| path_ranges(root)).map(|(from, _)| from).collect();
        let mut gone = vec![];
        let mut statement = self.db.prepare("SELECT id, path FROM files")?;
        for row in statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))? {
            let (id, path) = row?;
            if !prefixes.iter().any(|prefix| path.starts_with(prefix)) {
                gone.push(id);
            }
        }
        if !gone.is_empty() {
            self.transaction(|| gone.iter().try_for_each(|&id| self.delete(id)))?;
        }
        Ok(gone.len())
    }

    /// Queues every file again and forgets the fingerprint (after the model or settings changed).
    pub fn reset_all(&self) -> Result<()> {
        self.transaction(|| {
            self.db.execute_batch("
                DELETE FROM fts WHERE rowid IN (SELECT id FROM segments WHERE kind != 'name');
                DELETE FROM segments WHERE kind != 'name';
                UPDATE files SET state = 'pending', detail = NULL, info = NULL, indexed_at = NULL;")?;
            self.set_meta("fingerprint", None)
        })
    }

    /// Files waiting to be embedded, in indexing order; `only` narrows it to some files (just-added ones).
    pub fn pending(&self, limit: Option<usize>, only: Option<&HashSet<i64>>) -> Result<Vec<FileRecord>> {
        if only.is_some_and(HashSet::is_empty) {
            return Ok(vec![]);
        }
        let filter = only.map(|ids| format!(" AND id IN ({})",
                                            ids.iter().map(i64::to_string).collect::<Vec<_>>().join(","))).unwrap_or_default();
        let sql = format!("SELECT {FILE_COLUMNS} FROM files WHERE state = 'pending'{filter}{} ORDER BY {INDEXING_ORDER_SQL}, \
                           mtime DESC LIMIT ?", self.scope_sql);
        let mut values = self.scope_values.clone();
        values.push(Sql::Integer(limit.map_or(-1, |l| l as i64)));
        let mut statement = self.db.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), record)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn pending_count(&self) -> Result<i64> {
        Ok(self.db.query_row(&format!("SELECT COUNT(*) FROM files WHERE state = 'pending'{}", self.scope_sql),
                             params_from_iter(self.scope_values.iter()), |row| row.get(0))?)
    }

    /// Long files with pages or chunks still to read after their first step, in indexing order.
    pub fn unfinished(&self) -> Result<Vec<FileRecord>> {
        let sql = format!("SELECT {FILE_COLUMNS} FROM files WHERE state = 'partial'{} ORDER BY {INDEXING_ORDER_SQL}, \
                           mtime DESC", self.scope_sql);
        let mut statement = self.db.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(self.scope_values.iter()), record)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// How many long files are unfinished, and how many of their pages and chunks are left to read.
    pub fn unfinished_count(&self) -> Result<(i64, i64)> {
        Ok(self.db.query_row(&format!(
            "SELECT COUNT(*), coalesce(SUM(MAX(0, CAST(json_extract(info, '$.read_end') AS INTEGER) \
             - CAST(json_extract(info, '$.read_to') AS INTEGER))), 0) FROM files WHERE state = 'partial'{}", self.scope_sql),
            params_from_iter(self.scope_values.iter()), |row| Ok((row.get(0)?, row.get(1)?)))?)
    }

    /// What indexing learned about a file (`finish`'s `info`).
    pub fn info(&self, id: i64) -> Result<Info> {
        let json: Option<String> = self.db.query_row("SELECT info FROM files WHERE id = ?", [id], |row| row.get(0))
            .optional()?.flatten();
        Ok(json.and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default())
    }

    /// The last page or chunk stored for a file (where a long file's reading got to, when its info doesn't say).
    pub fn last_place(&self, id: i64) -> Result<Option<usize>> {
        let last: Option<f64> = self.db.query_row(
            "SELECT MAX(loc) FROM segments WHERE file = ? AND kind IN ('page', 'page_image', 'chunk')", [id],
            |row| row.get(0))?;
        Ok(last.map(|l| l as usize))
    }

    pub fn finish(&self, id: i64, state: &str, detail: Option<&str>, info: &Info) -> Result<()> {
        let json = (!info.is_empty()).then(|| serde_json::to_string(info).unwrap());
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
        self.db.prepare_cached("UPDATE files SET state = ?, detail = ?, info = ?, indexed_at = ? WHERE id = ?")?
            .execute(params![state, detail, json, now, id])?;
        Ok(())
    }

    /// A file the indexing helper had to stop on (reading it ran away): it isn't read again until it changes. A long
    /// file that was being finished keeps what was read of it. False when no file has that path.
    pub fn mark_stopped(&self, path: &str, why: &str) -> Result<bool> {
        let found: Option<(i64, String)> = self.db.query_row(
            "SELECT id, state FROM files WHERE path = ?", [path], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
        let Some((id, state)) = found else { return Ok(false) };
        if state == "partial" {
            self.finish(id, "done", Some(&format!("The rest couldn't be read: it {why}")), &Info::new())?;
        } else {
            self.finish(id, "failed", Some(&format!("It {why}")), &Info::new())?;
        }
        Ok(true)
    }

    // MARK: Segments

    /// Adds a segment; `text` goes into keyword search (normalized), `vector` into meaning search.
    #[allow(clippy::too_many_arguments)]
    pub fn add_segment(&self, file: i64, kind: SegmentKind, modality: Modality, loc: Option<f64>, loc_end: Option<f64>,
                       excerpt: Option<&str>, text: Option<&str>, vector: Option<&[f32]>) -> Result<()> {
        let modality = if vector.is_none() { Modality::None } else { modality };
        self.db.prepare_cached("INSERT INTO segments(file, kind, modality, loc, loc_end, excerpt, vec) \
                                VALUES(?, ?, ?, ?, ?, ?, ?)")?
            .execute(params![file, kind.name(), modality.name(), loc, loc_end, excerpt, vector.map(half)])?;
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            let (body, original) = keyword_text(text);
            self.db.prepare_cached("INSERT INTO fts(rowid, name, body, original) VALUES(?, '', ?, ?)")?
                .execute(params![self.db.last_insert_rowid(), body, original])?;
        }
        Ok(())
    }

    fn delete_segments(&self, file: i64, keeping_name: bool) -> Result<()> {
        let filter = if keeping_name { "AND kind != 'name'" } else { "" };
        self.db.execute(&format!("DELETE FROM fts WHERE rowid IN (SELECT id FROM segments WHERE file = ? {filter})"), [file])?;
        self.db.execute(&format!("DELETE FROM segments WHERE file = ? {filter}"), [file])?;
        Ok(())
    }

    // MARK: Stats

    pub fn counts(&self) -> Result<Counts> {
        let mut counts = Counts::default();
        let mut statement = self.db.prepare(&format!(
            "SELECT kind, state, COUNT(*) FROM files WHERE 1{} GROUP BY kind, state", self.scope_sql))?;
        let rows = statement.query_map(params_from_iter(self.scope_values.iter()),
                                       |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?)))?;
        for row in rows {
            let (kind, state, count) = row?;
            counts.by_kind_and_state.entry(kind).or_default().insert(state, count);
        }
        (counts.unfinished, counts.left) = self.unfinished_count()?;
        let in_scope = if self.scoped { format!(" AND file IN (SELECT id FROM files WHERE 1{})", self.scope_sql) } else {
            String::new()
        };
        (counts.segments, counts.vectors) = self.db.query_row(
            &format!("SELECT COUNT(*), COUNT(vec) FROM segments WHERE kind != 'name'{in_scope}"),
            params_from_iter(self.scope_values.iter()), |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(counts)
    }

    /// Each file's kind, for search.
    pub fn file_kinds(&self) -> Result<HashMap<i64, FileKind>> {
        let mut statement = self.db.prepare("SELECT id, kind FROM files")?;
        let rows = statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?;
        let mut kinds = HashMap::new();
        for row in rows {
            let (id, kind) = row?;
            if let Some(kind) = FileKind::parse(&kind) {
                kinds.insert(id, kind);
            }
        }
        Ok(kinds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_files_segments_and_words() {
        let folder = std::env::temp_dir().join(format!("spotlite-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let store = Store::open(&folder, MAIN_FILE).unwrap();
        let file = Candidate { path: "/tmp/x/Invoice K7Q2LM.pdf".into(), kind: FileKind::Pdf, size: 10, mtime: 1.5,
                               inode: 7, device: 1 };
        let id = store.insert(&file).unwrap();
        assert_eq!(store.pending(None, None).unwrap().len(), 1);
        store.add_segment(id, SegmentKind::Page, Modality::Text, Some(1.0), None, Some("x"), Some("the lease"),
                          Some(&[0.5; 768])).unwrap();
        store.finish(id, "done", None, &Info::from([("pages".into(), "1".into())])).unwrap();
        let counts = store.counts().unwrap();
        assert_eq!((counts.vectors, counts.segments), (1, 1));
        assert_eq!(store.info(id).unwrap()["pages"], "1");
        let found: i64 = store.db.query_row("SELECT COUNT(*) FROM fts WHERE fts MATCH 'k7q2lm'", [], |r| r.get(0)).unwrap();
        assert_eq!(found, 1);
        assert_eq!(store.files_under(&["/tmp/x".into()]).unwrap().len(), 1);
        assert_eq!(store.prune(&["/elsewhere".into()]).unwrap(), 1);
        let _ = std::fs::remove_dir_all(&folder);
    }
}
