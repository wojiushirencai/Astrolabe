//! Persistent index storage on redb.
//!
//! redb over LMDB: pure Rust, so cross-compiling to six release targets needs
//! no C toolchain, and the licence is MIT/Apache-2.0 with no attribution
//! obligation. BurntSushi picked it for ripgrep's experimental index too.
//!
//! **Write in bounded batches.** Measured on the prior art: switching from
//! per-entry synchronous writes to an unbounded async queue made indexing 5.7x
//! faster but grew steady-state RSS from 807 MB to 4049 MB, because every
//! pending value stayed buffered in memory. Batching into transactions of a
//! few thousand entries kept the full speedup at 883 MB. Never let the write
//! queue grow without a ceiling.
//!
//! Cold start must not deserialize the whole graph. Follow gopls: keep the
//! index on disk and load `O(files being worked on)`, not `O(repo)` — that one
//! architectural change bought gopls 53-83% of its memory back.

use crate::types::{
    CodeEdge, CodeFile, CodeSymbol, Confidence, EdgeKind, FileId, Language, RelPath, SymbolId,
    SymbolKind,
};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("storage: {0}")]
    Backend(String),
    #[error("schema version mismatch: found {found}, expected {expected}")]
    Version { found: u32, expected: u32 },
    #[error("database locked: {0}")]
    Locked(String),
}

pub const SCHEMA_VERSION: u32 = 1;

/// Entries buffered before a transaction is committed.
pub const WRITE_BATCH: usize = 2048;

const META: TableDefinition<&str, u32> = TableDefinition::new("meta");
const FILES: TableDefinition<u32, &[u8]> = TableDefinition::new("files");
const SYMBOLS: TableDefinition<u32, &[u8]> = TableDefinition::new("symbols");
const EDGES: TableDefinition<&str, &[u8]> = TableDefinition::new("edges");
const PARSE_CACHE: TableDefinition<&str, &[u8]> = TableDefinition::new("parse-cache");
const VERSION_KEY: &str = "schema-version";

pub struct Store {
    db: Database,
}

impl Store {
    /// Attempts before a lock-contention open gives up and the caller falls
    /// back to an unpersisted index. Six tries at 50ms spans the window in
    /// which a sibling cold start holds the file lock.
    pub const DEFAULT_RETRY_ATTEMPTS: usize = 6;
    /// Fixed wait between lock-contention retries.
    pub const DEFAULT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let db = match Database::create(path) {
            Ok(db) => db,
            Err(redb::DatabaseError::DatabaseAlreadyOpen) => {
                return Err(StoreError::Locked(
                    "Database already open. Cannot acquire lock.".to_string(),
                ));
            }
            Err(error) => return Err(backend(error)),
        };
        let write = db.begin_write().map_err(backend)?;
        {
            let mut meta = write.open_table(META).map_err(backend)?;
            let found = meta
                .get(VERSION_KEY)
                .map_err(backend)?
                .map(|value| value.value());
            match found {
                Some(found) if found != SCHEMA_VERSION => {
                    return Err(StoreError::Version {
                        found,
                        expected: SCHEMA_VERSION,
                    });
                }
                None => {
                    meta.insert(VERSION_KEY, SCHEMA_VERSION).map_err(backend)?;
                }
                _ => {}
            }
        }
        // Opening a table in a write transaction creates it when absent.
        drop(write.open_table(FILES).map_err(backend)?);
        drop(write.open_table(SYMBOLS).map_err(backend)?);
        drop(write.open_table(EDGES).map_err(backend)?);
        drop(write.open_table(PARSE_CACHE).map_err(backend)?);
        write.commit().map_err(backend)?;
        Ok(Self { db })
    }

    /// Open an existing store, or delete and recreate it when the on-disk
    /// schema does not match [`SCHEMA_VERSION`].
    ///
    /// `open` still rejects a mismatch so callers that want to inspect the
    /// error can; indexing uses this path so a bump never takes the process
    /// down. Other open failures (permissions, a corrupt file) are returned
    /// unchanged — those are not safe to repair by deleting the database.
    pub fn open_or_heal(path: &Path) -> Result<Self, StoreError> {
        match Self::open(path) {
            Ok(store) => Ok(store),
            Err(StoreError::Version { found, expected }) => {
                tracing::warn!(
                    found,
                    expected,
                    path = %path.display(),
                    "schema version mismatch; recreating store"
                );
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(backend(error)),
                }
                Self::open(path)
            }
            Err(error) => Err(error),
        }
    }

    /// Open a store, retrying while another process holds the database lock.
    ///
    /// redb allows one writer process per file: when several processes cold
    /// start the same repository, the losers see `DatabaseAlreadyOpen` and
    /// used to fall straight back to an unpersisted full reparse. That error
    /// is a race the loser can win by waiting, not a broken database, so
    /// retry up to `attempts` times (0 counts as one attempt) with
    /// `initial_delay` between tries. Any error that is not plausibly lock
    /// contention — permissions, corruption, an unhealable schema — returns
    /// immediately: retrying cannot fix those.
    pub fn open_or_heal_with_retry(
        path: &Path,
        attempts: usize,
        initial_delay: std::time::Duration,
    ) -> Result<Self, StoreError> {
        let attempts = attempts.max(1);
        for attempt in 0..attempts {
            match Self::open_or_heal(path) {
                Ok(store) => {
                    if attempt > 0 {
                        tracing::info!(
                            attempt,
                            path = %path.display(),
                            "acquired store lock after retry"
                        );
                    }
                    return Ok(store);
                }
                Err(error) => {
                    if attempt + 1 == attempts || !is_lock_contention(&error) {
                        return Err(error);
                    }
                    std::thread::sleep(initial_delay);
                }
            }
        }
        unreachable!("retry loop returns on its final attempt")
    }

    pub fn put_files(&self, files: &[CodeFile]) -> Result<(), StoreError> {
        // `chunks` is the hard bound: no transaction can retain more than
        // WRITE_BATCH newly serialized values.
        for chunk in files.chunks(WRITE_BATCH) {
            let write = self.db.begin_write().map_err(backend)?;
            {
                let mut table = write.open_table(FILES).map_err(backend)?;
                for file in chunk {
                    let value = serde_json::to_vec(&FileWire::from(file)).map_err(backend)?;
                    table.insert(file.id.0, value.as_slice()).map_err(backend)?;
                }
            }
            write.commit().map_err(backend)?;
        }
        Ok(())
    }

    pub fn put_symbols(&self, symbols: &[CodeSymbol]) -> Result<(), StoreError> {
        for chunk in symbols.chunks(WRITE_BATCH) {
            let write = self.db.begin_write().map_err(backend)?;
            {
                let mut table = write.open_table(SYMBOLS).map_err(backend)?;
                for symbol in chunk {
                    let value = serde_json::to_vec(&SymbolWire::from(symbol)).map_err(backend)?;
                    table
                        .insert(symbol.id.0, value.as_slice())
                        .map_err(backend)?;
                }
            }
            write.commit().map_err(backend)?;
        }
        Ok(())
    }

    pub fn put_edges(&self, edges: &[CodeEdge]) -> Result<(), StoreError> {
        for chunk in edges.chunks(WRITE_BATCH) {
            let write = self.db.begin_write().map_err(backend)?;
            {
                let mut table = write.open_table(EDGES).map_err(backend)?;
                for edge in chunk {
                    let key = edge_key(edge.from, edge.to, edge.kind);
                    let value = serde_json::to_vec(&EdgeWire::from(edge)).map_err(backend)?;
                    table
                        .insert(key.as_str(), value.as_slice())
                        .map_err(backend)?;
                }
            }
            write.commit().map_err(backend)?;
        }
        Ok(())
    }

    pub fn get_file(&self, id: FileId) -> Result<Option<CodeFile>, StoreError> {
        let read = self.db.begin_read().map_err(backend)?;
        let table = read.open_table(FILES).map_err(backend)?;
        let bytes = table.get(id.0).map_err(backend)?;
        bytes
            .map(|value| {
                serde_json::from_slice::<FileWire>(value.value())
                    .map(CodeFile::from)
                    .map_err(backend)
            })
            .transpose()
    }

    pub fn get_symbol(&self, id: SymbolId) -> Result<Option<CodeSymbol>, StoreError> {
        let read = self.db.begin_read().map_err(backend)?;
        let table = read.open_table(SYMBOLS).map_err(backend)?;
        let bytes = table.get(id.0).map_err(backend)?;
        bytes
            .map(|value| {
                serde_json::from_slice::<SymbolWire>(value.value())
                    .map(CodeSymbol::from)
                    .map_err(backend)
            })
            .transpose()
    }

    pub fn get_edge(
        &self,
        from: u32,
        to: u32,
        kind: EdgeKind,
    ) -> Result<Option<CodeEdge>, StoreError> {
        let read = self.db.begin_read().map_err(backend)?;
        let table = read.open_table(EDGES).map_err(backend)?;
        let key = edge_key(from, to, kind);
        let bytes = table.get(key.as_str()).map_err(backend)?;
        bytes
            .map(|value| {
                serde_json::from_slice::<EdgeWire>(value.value())
                    .map(CodeEdge::from)
                    .map_err(backend)
            })
            .transpose()
    }

    pub fn put_cached_parse(&self, sha: &str, parsed: &[u8]) -> Result<(), StoreError> {
        self.put_cached_parses([(sha, parsed)])
    }

    /// Write cached parse payloads in transactions of at most [`WRITE_BATCH`].
    ///
    /// An empty iterator is a no-op: a warm reindex must not pay for a write
    /// transaction when nothing changed.
    pub fn put_cached_parses<'a, I>(&self, entries: I) -> Result<(), StoreError>
    where
        I: IntoIterator<Item = (&'a str, &'a [u8])>,
    {
        let entries: Vec<(&str, &[u8])> = entries.into_iter().collect();
        for chunk in entries.chunks(WRITE_BATCH) {
            let write = self.db.begin_write().map_err(backend)?;
            {
                let mut table = write.open_table(PARSE_CACHE).map_err(backend)?;
                for (sha, parsed) in chunk {
                    table.insert(*sha, *parsed).map_err(backend)?;
                }
            }
            write.commit().map_err(backend)?;
        }
        Ok(())
    }

    /// Cached parse result keyed by content hash, so an unchanged file is
    /// never reparsed.
    pub fn cached_parse(&self, sha: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let read = self.db.begin_read().map_err(backend)?;
        let table = read.open_table(PARSE_CACHE).map_err(backend)?;
        Ok(table
            .get(sha)
            .map_err(backend)?
            .map(|value| value.value().to_vec()))
    }
}

/// Cache key for a parsed file: language plus content hash.
///
/// The store table is a flat string map, so the language prefix keeps two
/// files that happen to share bytes (a renamed `.js` → `.ts`, a copied
/// snippet) from handing each other the wrong query results.
pub fn parse_cache_key(language: Language, sha: &str) -> String {
    format!("{}:{sha}", language.name())
}

/// Serialize a [`crate::parse::ParsedFile`] for [`PARSE_CACHE`].
pub fn encode_parsed(parsed: &crate::parse::ParsedFile) -> Result<Vec<u8>, StoreError> {
    serde_json::to_vec(&ParsedWire::from(parsed)).map_err(backend)
}

/// Inverse of [`encode_parsed`]. A corrupt or stale payload is a cache miss
/// at the call site, not an index failure.
pub fn decode_parsed(bytes: &[u8]) -> Result<crate::parse::ParsedFile, StoreError> {
    serde_json::from_slice::<ParsedWire>(bytes)
        .map(crate::parse::ParsedFile::from)
        .map_err(backend)
}

fn backend(error: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(error.to_string())
}

/// Whether an open failure looks like another process holding the file lock.
///
/// redb maps contention to [`redb::DatabaseError::DatabaseAlreadyOpen`], which
/// [`Store::open`] turns into [`StoreError::Locked`]. For defense-in-depth,
/// we also match strings containing "already open".
fn is_lock_contention(error: &StoreError) -> bool {
    matches!(error, StoreError::Locked(_))
        || error.to_string().to_lowercase().contains("already open")
}

fn edge_key(from: u32, to: u32, kind: EdgeKind) -> String {
    format!("{from:08x}:{to:08x}:{}", edge_kind_to_u8(kind))
}

fn edge_kind_to_u8(kind: EdgeKind) -> u8 {
    match kind {
        EdgeKind::Import => 0,
        EdgeKind::Call => 1,
        EdgeKind::Inherit => 2,
    }
}

#[derive(Serialize, Deserialize)]
struct FileWire {
    id: u32,
    path: String,
    language: Option<u8>,
    loc: u32,
    sha: String,
}

impl From<&CodeFile> for FileWire {
    fn from(file: &CodeFile) -> Self {
        Self {
            id: file.id.0,
            path: file.path.as_str().to_owned(),
            language: file.language.map(language_to_u8),
            loc: file.loc,
            sha: file.sha.clone(),
        }
    }
}

impl From<FileWire> for CodeFile {
    fn from(file: FileWire) -> Self {
        Self {
            id: FileId(file.id),
            path: RelPath::new(file.path),
            language: file.language.and_then(language_from_u8),
            loc: file.loc,
            sha: file.sha,
        }
    }
}

fn language_to_u8(language: Language) -> u8 {
    match language {
        Language::Python => 0,
        Language::Go => 1,
        Language::Java => 2,
        Language::Rust => 3,
        Language::TypeScript => 4,
        Language::Tsx => 5,
        Language::JavaScript => 6,
    }
}

#[derive(Serialize, Deserialize)]
struct ParsedWire {
    symbols: Vec<SymbolWire>,
    imports: Vec<String>,
    calls: Vec<(String, u32)>,
}

impl From<&crate::parse::ParsedFile> for ParsedWire {
    fn from(parsed: &crate::parse::ParsedFile) -> Self {
        Self {
            symbols: parsed.symbols.iter().map(SymbolWire::from).collect(),
            imports: parsed.imports.clone(),
            calls: parsed.calls.clone(),
        }
    }
}

impl From<ParsedWire> for crate::parse::ParsedFile {
    fn from(parsed: ParsedWire) -> Self {
        crate::parse::ParsedFile {
            symbols: parsed.symbols.into_iter().map(CodeSymbol::from).collect(),
            imports: parsed.imports,
            calls: parsed.calls,
        }
    }
}

fn language_from_u8(language: u8) -> Option<Language> {
    Some(match language {
        0 => Language::Python,
        1 => Language::Go,
        2 => Language::Java,
        3 => Language::Rust,
        4 => Language::TypeScript,
        5 => Language::Tsx,
        6 => Language::JavaScript,
        _ => return None,
    })
}

#[derive(Serialize, Deserialize)]
struct SymbolWire {
    id: u32,
    file: u32,
    name: String,
    kind: u8,
    signature: String,
    start_line: u32,
    end_line: u32,
    exported: bool,
}

impl From<&CodeSymbol> for SymbolWire {
    fn from(symbol: &CodeSymbol) -> Self {
        Self {
            id: symbol.id.0,
            file: symbol.file.0,
            name: symbol.name.clone(),
            kind: symbol_kind_to_u8(symbol.kind),
            signature: symbol.signature.clone(),
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            exported: symbol.exported,
        }
    }
}

impl From<SymbolWire> for CodeSymbol {
    fn from(symbol: SymbolWire) -> Self {
        Self {
            id: SymbolId(symbol.id),
            file: FileId(symbol.file),
            name: symbol.name,
            kind: symbol_kind_from_u8(symbol.kind),
            signature: symbol.signature,
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            exported: symbol.exported,
        }
    }
}

fn symbol_kind_to_u8(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Function => 0,
        SymbolKind::Method => 1,
        SymbolKind::Class => 2,
        SymbolKind::Interface => 3,
        SymbolKind::Struct => 4,
        SymbolKind::Enum => 5,
        SymbolKind::Trait => 6,
        SymbolKind::Type => 7,
        SymbolKind::Const => 8,
        SymbolKind::Module => 9,
        SymbolKind::Field => 10,
        SymbolKind::Variable => 11,
    }
}

fn symbol_kind_from_u8(kind: u8) -> SymbolKind {
    match kind {
        0 => SymbolKind::Function,
        1 => SymbolKind::Method,
        2 => SymbolKind::Class,
        3 => SymbolKind::Interface,
        4 => SymbolKind::Struct,
        5 => SymbolKind::Enum,
        6 => SymbolKind::Trait,
        7 => SymbolKind::Type,
        8 => SymbolKind::Const,
        9 => SymbolKind::Module,
        10 => SymbolKind::Field,
        11 => SymbolKind::Variable,
        _ => SymbolKind::Function,
    }
}

#[derive(Serialize, Deserialize)]
struct EdgeWire {
    from: u32,
    to: u32,
    kind: u8,
    weight: u32,
    confidence: u8,
}

impl From<&CodeEdge> for EdgeWire {
    fn from(edge: &CodeEdge) -> Self {
        Self {
            from: edge.from,
            to: edge.to,
            kind: edge_kind_to_u8(edge.kind),
            weight: edge.weight,
            confidence: match edge.confidence {
                Confidence::Exact => 0,
                Confidence::Scoped => 1,
                Confidence::Syntactic => 2,
                Confidence::Unknown => 3,
            },
        }
    }
}

impl From<EdgeWire> for CodeEdge {
    fn from(edge: EdgeWire) -> Self {
        Self {
            from: edge.from,
            to: edge.to,
            kind: match edge.kind {
                0 => EdgeKind::Import,
                1 => EdgeKind::Call,
                2 => EdgeKind::Inherit,
                _ => EdgeKind::Call,
            },
            weight: edge.weight,
            confidence: match edge.confidence {
                0 => Confidence::Exact,
                1 => Confidence::Scoped,
                2 => Confidence::Syntactic,
                _ => Confidence::Unknown,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "astrolabe-store-{name}-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn db(&self) -> std::path::PathBuf {
            self.0.join("index.redb")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn file(id: u32) -> CodeFile {
        CodeFile {
            id: FileId(id),
            path: RelPath::new(format!("src/{id}.rs")),
            language: Some(Language::Rust),
            loc: id + 1,
            sha: format!("sha-{id}"),
        }
    }

    #[test]
    fn open_reopen_preserves_data() {
        let dir = TestDir::new("reopen");
        {
            let store = Store::open(&dir.db()).unwrap();
            store.put_files(&[file(7)]).unwrap();
        }
        let store = Store::open(&dir.db()).unwrap();
        let loaded = store.get_file(FileId(7)).unwrap().unwrap();
        assert_eq!(loaded.path.as_str(), "src/7.rs");
        assert_eq!(loaded.sha, "sha-7");
    }

    #[test]
    fn schema_version_mismatch_is_rejected() {
        let dir = TestDir::new("version");
        {
            let store = Store::open(&dir.db()).unwrap();
            drop(store);
            let db = Database::create(dir.db()).unwrap();
            let write = db.begin_write().unwrap();
            {
                let mut meta = write.open_table(META).unwrap();
                meta.insert(VERSION_KEY, SCHEMA_VERSION + 1).unwrap();
            }
            write.commit().unwrap();
        }
        assert!(matches!(
            Store::open(&dir.db()),
            Err(StoreError::Version {
                found,
                expected
            }) if found == SCHEMA_VERSION + 1 && expected == SCHEMA_VERSION
        ));
    }

    #[test]
    fn writes_more_than_one_batch_and_reads_every_entry() {
        let dir = TestDir::new("batch");
        let store = Store::open(&dir.db()).unwrap();
        let files: Vec<_> = (0..5000).map(file).collect();
        store.put_files(&files).unwrap();
        for id in 0..5000 {
            let loaded = store.get_file(FileId(id)).unwrap().unwrap();
            assert_eq!(loaded.id, FileId(id));
            assert_eq!(loaded.sha, format!("sha-{id}"));
        }
    }

    #[test]
    fn parse_cache_hit_and_miss() {
        let dir = TestDir::new("cache");
        let store = Store::open(&dir.db()).unwrap();
        assert_eq!(store.cached_parse("missing").unwrap(), None);
        store.put_cached_parse("abc", b"parsed tree").unwrap();
        assert_eq!(
            store.cached_parse("abc").unwrap(),
            Some(b"parsed tree".to_vec())
        );
    }

    #[test]
    fn concurrent_reads_do_not_panic() {
        let dir = TestDir::new("reads");
        let store = Arc::new(Store::open(&dir.db()).unwrap());
        store
            .put_files(&(0..64).map(file).collect::<Vec<_>>())
            .unwrap();
        let threads: Vec<_> = (0..8)
            .map(|thread| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    for offset in 0..64 {
                        let id = (thread + offset) % 64;
                        assert!(store.get_file(FileId(id)).unwrap().is_some());
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    }

    #[test]
    fn open_or_heal_rebuilds_on_schema_mismatch() {
        let dir = TestDir::new("heal");
        {
            let store = Store::open(&dir.db()).unwrap();
            store.put_cached_parse("old", b"stale").unwrap();
            drop(store);
            let db = Database::create(dir.db()).unwrap();
            let write = db.begin_write().unwrap();
            {
                let mut meta = write.open_table(META).unwrap();
                meta.insert(VERSION_KEY, SCHEMA_VERSION + 1).unwrap();
            }
            write.commit().unwrap();
        }
        let store = Store::open_or_heal(&dir.db()).unwrap();
        assert_eq!(
            store.cached_parse("old").unwrap(),
            None,
            "recreated store must not keep the previous schema's cache"
        );
        store.put_cached_parse("new", b"fresh").unwrap();
        assert_eq!(store.cached_parse("new").unwrap(), Some(b"fresh".to_vec()));
        // A healed store is a current-schema store; open must accept it.
        drop(store);
        Store::open(&dir.db()).unwrap();
    }

    #[test]
    fn put_cached_parses_writes_more_than_one_batch() {
        let dir = TestDir::new("parse-batch");
        let store = Store::open(&dir.db()).unwrap();
        let payloads: Vec<(String, Vec<u8>)> = (0..WRITE_BATCH + 128)
            .map(|i| (format!("k{i}"), format!("v{i}").into_bytes()))
            .collect();
        let refs: Vec<(&str, &[u8])> = payloads
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_slice()))
            .collect();
        store.put_cached_parses(refs).unwrap();
        assert_eq!(store.cached_parse("k0").unwrap(), Some(b"v0".to_vec()));
        assert_eq!(
            store
                .cached_parse(&format!("k{}", WRITE_BATCH + 127))
                .unwrap(),
            Some(format!("v{}", WRITE_BATCH + 127).into_bytes())
        );
    }

    #[test]
    fn parsed_file_roundtrip_through_cache() {
        let dir = TestDir::new("parsed");
        let store = Store::open(&dir.db()).unwrap();
        let parsed = crate::parse::ParsedFile {
            symbols: vec![CodeSymbol {
                id: SymbolId(0),
                file: FileId(0),
                name: "Hello".into(),
                kind: SymbolKind::Function,
                signature: "func Hello()".into(),
                start_line: 1,
                end_line: 3,
                exported: true,
            }],
            imports: vec!["fmt".into()],
            calls: vec![("Println".into(), 2)],
        };
        let bytes = encode_parsed(&parsed).unwrap();
        let key = parse_cache_key(Language::Go, "abc");
        store.put_cached_parse(&key, &bytes).unwrap();
        let loaded = decode_parsed(&store.cached_parse(&key).unwrap().unwrap()).unwrap();
        assert_eq!(loaded.imports, parsed.imports);
        assert_eq!(loaded.calls, parsed.calls);
        assert_eq!(loaded.symbols[0].name, "Hello");
        assert_eq!(loaded.symbols[0].kind, SymbolKind::Function);
        assert!(loaded.symbols[0].exported);
    }

    #[test]
    fn test_open_or_heal_with_retry_succeeds_when_lock_released() {
        let dir = TestDir::new("retry");
        let holder = Store::open(&dir.db()).unwrap();
        let path = dir.db();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            drop(holder);
        });
        // The first attempts lose the race for the file lock; the winner must
        // be acquired once the sibling handle is dropped, well inside the
        // 10 x 50ms retry budget.
        let store = Store::open_or_heal_with_retry(&path, 10, Duration::from_millis(50)).unwrap();
        handle.join().unwrap();
        store.put_files(&[file(9)]).unwrap();
        assert_eq!(store.get_file(FileId(9)).unwrap().unwrap().sha, "sha-9");
    }

    #[test]
    fn test_open_or_heal_with_retry_fails_when_lock_never_released() {
        let dir = TestDir::new("retry-fail");
        let _holder = Store::open(&dir.db()).unwrap();
        let err = match Store::open_or_heal_with_retry(&dir.db(), 2, Duration::from_millis(20)) {
            Ok(_) => panic!("expected lock failure"),
            Err(e) => e,
        };
        assert!(matches!(err, StoreError::Locked(_)));
        assert!(is_lock_contention(&err));
    }

    #[test]
    fn test_is_lock_contention_classifier() {
        assert!(is_lock_contention(&StoreError::Locked("busy".into())));
        assert!(is_lock_contention(&StoreError::Backend(
            "Database already open".into()
        )));
        assert!(!is_lock_contention(&StoreError::Backend(
            "Permission denied".into()
        )));
        assert!(!is_lock_contention(&StoreError::Version {
            found: 1,
            expected: 2
        }));
    }
}
