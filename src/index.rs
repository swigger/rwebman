use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use walkdir::{DirEntry, WalkDir};

use crate::config::{self, Config};

#[derive(Clone, Default, Serialize)]
pub struct Progress {
    pub running: bool,
    pub visited: u64,
    pub skipped: u64,
    pub current_path: String,
    pub indexed_count: u64,
    pub last_completed: Option<u64>,
    pub elapsed_seconds: u64,
    pub error: Option<String>,
}

pub struct Index {
    pub path: PathBuf,
    pub progress: Mutex<Progress>,
    pub cancelled: AtomicBool,
}

#[derive(Default, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub path: bool,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub offset: u32,
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub size: Option<u64>,
    pub modified: Option<u64>,
}

#[derive(Serialize)]
pub struct SearchResult {
    pub entries: Vec<Entry>,
    pub has_more: bool,
    pub elapsed_ms: u128,
}

impl Index {
    pub fn open(path: PathBuf) -> Result<Arc<Self>> {
        let db = Connection::open(&path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        db.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS entries (
                id INTEGER PRIMARY KEY, name TEXT NOT NULL, path TEXT NOT NULL,
                name_fold TEXT NOT NULL, path_fold TEXT NOT NULL,
                kind TEXT NOT NULL, size INTEGER, modified INTEGER
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
                name_fold, path_fold, content='entries', content_rowid='id', tokenize='trigram'
            );
            CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
        ",
        )?;
        let count = db
            .query_row("SELECT value FROM meta WHERE key='count'", [], |r| r.get(0))
            .unwrap_or(0);
        let last = db
            .query_row(
                "SELECT value FROM meta WHERE key='last_completed'",
                [],
                |r| r.get(0),
            )
            .ok();
        Ok(Arc::new(Self {
            path,
            progress: Mutex::new(Progress {
                indexed_count: count,
                last_completed: last,
                ..Default::default()
            }),
            cancelled: AtomicBool::new(false),
        }))
    }

    pub fn start(self: &Arc<Self>, config: Config, data_dir: PathBuf) -> bool {
        {
            let mut progress = self.progress.lock().unwrap();
            if progress.running {
                return false;
            }
            progress.running = true;
            progress.visited = 0;
            progress.skipped = 0;
            progress.elapsed_seconds = 0;
            progress.current_path.clear();
            progress.error = None;
            self.cancelled.store(false, Ordering::Relaxed);
        }
        let index = self.clone();
        tokio::task::spawn_blocking(move || {
            let start = Instant::now();
            let result = index.scan(&config, &data_dir);
            let mut progress = index.progress.lock().unwrap();
            progress.running = false;
            progress.current_path.clear();
            progress.elapsed_seconds = start.elapsed().as_secs();
            match result {
                Ok((count, completed)) => {
                    progress.indexed_count = count;
                    progress.last_completed = Some(completed);
                    tracing::info!(
                        count,
                        seconds = progress.elapsed_seconds,
                        skipped = progress.skipped,
                        "Index refreshed"
                    );
                }
                Err(error) => {
                    tracing::warn!(%error, "Index refresh did not complete; previous index retained");
                    progress.error = Some(error.to_string());
                }
            }
        });
        true
    }

    fn scan(&self, config: &Config, data_dir: &Path) -> Result<(u64, u64)> {
        if config.roots.is_empty() {
            bail!("No index roots configured");
        }
        // A disconnected volume must not silently erase its previous entries.
        for root in &config.roots {
            std::fs::read_dir(root).with_context(|| {
                format!("Index root is unavailable: {}", config::display_path(root))
            })?;
        }
        let mut db = Connection::open(&self.path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO entries_fts(entries_fts) VALUES('delete-all')",
            [],
        )?;
        tx.execute("DELETE FROM entries", [])?;
        let start = Instant::now();
        let mut count = 0u64;
        let mut skipped = 0u64;
        {
            let mut insert = tx.prepare("INSERT INTO entries(id,name,path,name_fold,path_fold,kind,size,modified) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)")?;
            let mut fts =
                tx.prepare("INSERT INTO entries_fts(rowid,name_fold,path_fold) VALUES (?1,?2,?3)")?;
            for root in &config.roots {
                let walker = WalkDir::new(root)
                    .follow_links(false)
                    .into_iter()
                    .filter_entry(|e| {
                        !self.cancelled.load(Ordering::Relaxed)
                            && !excluded(e, &config.excludes, data_dir)
                    });
                for item in walker {
                    if self.cancelled.load(Ordering::Relaxed) {
                        bail!("Indexing cancelled; previous index retained");
                    }
                    let entry = match item {
                        Ok(e) => e,
                        Err(_) => {
                            skipped += 1;
                            continue;
                        }
                    };
                    let metadata = match entry.metadata() {
                        Ok(m) => m,
                        Err(_) => {
                            skipped += 1;
                            continue;
                        }
                    };
                    let kind = if entry.file_type().is_symlink() {
                        "symlink"
                    } else if metadata.is_dir() {
                        "directory"
                    } else if metadata.is_file() {
                        "file"
                    } else {
                        continue;
                    };
                    let name = entry.file_name().to_string_lossy();
                    let path = config::display_path(entry.path());
                    let name_fold = name.to_lowercase();
                    let path_fold = path.to_lowercase();
                    let size = metadata.is_file().then_some(metadata.len());
                    let modified = metadata
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|t| t.as_secs());
                    count += 1;
                    insert.execute(params![
                        count, name, path, name_fold, path_fold, kind, size, modified
                    ])?;
                    fts.execute(params![count, name_fold, path_fold])?;
                    if count.is_multiple_of(256) {
                        let mut progress = self.progress.lock().unwrap();
                        progress.visited = count;
                        progress.skipped = skipped;
                        progress.current_path = path;
                        progress.elapsed_seconds = start.elapsed().as_secs();
                    }
                }
            }
        }
        if self.cancelled.load(Ordering::Relaxed) {
            bail!("Indexing cancelled; previous index retained");
        }
        let completed = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        tx.execute(
            "INSERT OR REPLACE INTO meta(key,value) VALUES ('last_completed',?1),('count',?2)",
            params![completed, count],
        )?;
        tx.commit()?;
        // WAL auto-checkpoints normally; this also bounds storage after a large scan.
        let _ = db.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
        let mut progress = self.progress.lock().unwrap();
        progress.visited = count;
        progress.skipped = skipped;
        Ok((count, completed))
    }

    pub fn search(&self, query: &SearchQuery) -> Result<SearchResult> {
        let start = Instant::now();
        let term = query.q.trim().to_lowercase();
        if term.len() > 512 || term.contains('\0') {
            bail!("Search text is too long or contains an invalid character");
        }
        if query.offset > 100_000 {
            bail!("Please narrow your search");
        }
        if !["", "file", "directory", "symlink"].contains(&query.kind.as_str()) {
            bail!("Invalid file type filter");
        }
        let limit = query.limit.unwrap_or(100).clamp(1, 200) as usize;
        let db = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        db.busy_timeout(Duration::from_secs(3))?;
        db.progress_handler(
            10_000,
            Some(move || start.elapsed() > Duration::from_secs(5)),
        );
        let column = if query.path { "path_fold" } else { "name_fold" };
        // Trigram MATCH accelerates literal substrings >= 3 Unicode scalar values.
        // instr() preserves literal punctuation and short/CJK searches without LIKE wildcards.
        let use_fts = term.chars().count() >= 3;
        let source = if use_fts {
            "entries e JOIN entries_fts ON entries_fts.rowid=e.id"
        } else {
            "entries e"
        };
        let fts_clause = if use_fts {
            "AND entries_fts MATCH ?5"
        } else {
            ""
        };
        let sql = format!(
            "SELECT e.name,e.path,e.kind,e.size,e.modified FROM {source}
            WHERE instr(e.{column},?1)>0 AND (?2='' OR e.kind=?2) {fts_clause}
            ORDER BY e.name_fold,e.path_fold LIMIT ?3 OFFSET ?4"
        );
        let mut statement = db.prepare(&sql)?;
        let match_query = format!("{column} : \"{}\"", term.replace('"', "\"\""));
        let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Entry> {
            Ok(Entry {
                name: row.get(0)?,
                path: row.get(1)?,
                kind: row.get(2)?,
                size: row.get(3)?,
                modified: row.get(4)?,
            })
        };
        let mut entries = if use_fts {
            statement
                .query_map(
                    params![term, query.kind, limit + 1, query.offset, match_query],
                    map,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            statement
                .query_map(params![term, query.kind, limit + 1, query.offset], map)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let has_more = entries.len() > limit;
        entries.truncate(limit);
        Ok(SearchResult {
            entries,
            has_more,
            elapsed_ms: start.elapsed().as_millis(),
        })
    }
}

fn excluded(entry: &DirEntry, excludes: &[String], data_dir: &Path) -> bool {
    if entry.path().starts_with(data_dir) {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    let path = config::display_path(entry.path());
    excludes.iter().any(|exclude| {
        if exclude.contains('/') || exclude.contains('\\') {
            let excluded = exclude.trim_end_matches(['/', '\\']);
            #[cfg(windows)]
            let (path, excluded) = (path.to_lowercase(), excluded.to_lowercase());
            #[cfg(windows)]
            let excluded = excluded.as_str();
            path == excluded
                || path
                    .strip_prefix(excluded)
                    .is_some_and(|s| s.starts_with(['/', '\\']))
        } else {
            #[cfg(windows)]
            {
                name.eq_ignore_ascii_case(exclude)
            }
            #[cfg(not(windows))]
            {
                name == exclude.as_str()
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn literal_search_and_atomic_rebuild() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        let data = temp.path().join("data");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        for name in [
            "Hello.txt",
            "你好世界.txt",
            "100%_done.txt",
            "Äpfel.txt",
            "a'b.txt",
        ] {
            std::fs::write(root.join(name), "hello").unwrap();
        }
        std::fs::create_dir(root.join("node_modules")).unwrap();
        std::fs::write(root.join("node_modules/secret.txt"), "hidden").unwrap();
        let cfg = Config {
            password_hash: String::new(),
            roots: vec![root.clone()],
            excludes: vec!["node_modules".into()],
            index_interval_minutes: 60,
        };
        let index = Index::open(data.join("index.sqlite")).unwrap();
        index.scan(&cfg, &data).unwrap();
        for term in ["HELLO", "你好", "你好世", "100%_", "äpf", "a'b"] {
            assert_eq!(
                index
                    .search(&SearchQuery {
                        q: term.into(),
                        ..Default::default()
                    })
                    .unwrap()
                    .entries
                    .len(),
                1,
                "term={term}"
            );
        }
        assert!(
            index
                .search(&SearchQuery {
                    q: "secret".into(),
                    ..Default::default()
                })
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(
            index
                .search(&SearchQuery {
                    q: "\" OR x*".into(),
                    ..Default::default()
                })
                .unwrap()
                .entries
                .is_empty()
        );
        std::fs::remove_file(root.join("Hello.txt")).unwrap();
        index.cancelled.store(true, Ordering::Relaxed);
        assert!(index.scan(&cfg, &data).is_err());
        assert_eq!(
            index
                .search(&SearchQuery {
                    q: "Hello".into(),
                    ..Default::default()
                })
                .unwrap()
                .entries
                .len(),
            1
        );
        index.cancelled.store(false, Ordering::Relaxed);
        index.scan(&cfg, &data).unwrap();
        assert!(
            index
                .search(&SearchQuery {
                    q: "Hello".into(),
                    ..Default::default()
                })
                .unwrap()
                .entries
                .is_empty()
        );
        let result = index
            .search(&SearchQuery {
                limit: Some(2),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.entries.len(), 2);
        assert!(result.has_more);
    }
}
