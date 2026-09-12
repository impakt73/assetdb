use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::FileExt as _;
#[cfg(windows)]
use std::os::windows::fs::FileExt as _;

use crate::db::{DbError, IntegrityProblem, IntegrityReport};

const MAGIC: &[u8; 4] = b"AVFS";
const VERSION: u32 = 3;
const LEGACY_VERSION: u32 = 2;
const HEADER_LEN: u64 = 8;
const HASH_LEN: u64 = 64; // lowercase hex-encoded SHA-256 content hash
const RECORD_HEADER_LEN: u64 = 16 + HASH_LEN; // path_len u32 + data_len u64 + metadata_len u32 + hex hash
const LEGACY_RECORD_HEADER_LEN: u64 = 12 + HASH_LEN;
const TOMBSTONE_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// A virtual file system backed by a single database file on disk.
///
/// All file contents live in one append-only log file; only a small
/// in-memory index of `path -> (offset, length, metadata)` records is kept, so
/// opening the file system never loads the stored data into memory.
/// Data is read from disk on demand, one file at a time.
///
/// On-disk layout:
///
/// ```text
/// [ "AVFS" ][ version u32 LE ]            (header)
/// [ path_len u32 LE ][ data_len u64 LE ][ metadata_len u32 LE ] (record header)
/// [ sha256 hash of the data (64 lowercase hex chars) ]
/// [ metadata bytes ]
/// [ path bytes (UTF-8, normalized) ]
/// [ data bytes ]
/// ... more records, appended in insertion order
/// ```
///
/// Each record stores opaque metadata and the lowercase hex-encoded SHA-256
/// hash of its data, computed when the record is written (via the `sha256`
/// crate).
/// `Vfs::verify_integrity` re-reads and re-hashes the data that is
/// currently on disk and reports every record whose data no longer matches
/// the stored hash.
///
/// Re-inserting a path appends a new record; the index keeps the latest
/// one. A truncated tail (e.g. from a crash mid-write) is discarded when
/// the file is opened again.
#[derive(Debug)]
pub struct Vfs {
    path: PathBuf,
    file: File,
    end: u64,
    index: BTreeMap<String, Record>,
}

#[derive(Debug, Clone)]
struct Record {
    /// Byte offset of the data payload within the database file.
    offset: u64,
    /// Length of the data payload.
    len: u64,
    /// Lowercase hex-encoded SHA-256 hash of the data payload, recorded
    /// when the record was written.
    hash: String,
    /// Opaque record metadata. It is indexed independently from the payload.
    metadata: Vec<u8>,
}

impl Vfs {
    /// Open the database file at `path`, creating it (empty) if missing.
    ///
    /// Existing records are indexed, but their data is not read until
    /// requested. Returns an error if the file exists but is not a valid
    /// virtual file system database.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        Self::open_inner(path.as_ref(), false)
    }

    /// Create a new, empty database file at `path`, discarding any
    /// previous file that was there.
    pub fn create(path: impl AsRef<Path>) -> Result<Self, DbError> {
        Self::open_inner(path.as_ref(), true)
    }

    fn open_inner(path: &Path, truncate: bool) -> Result<Self, DbError> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(truncate)
            .open(path)
            .map_err(DbError::Io)?;
        let mut end = file.metadata().map_err(DbError::Io)?.len();
        let mut index = BTreeMap::new();
        let mut legacy_file = false;

        if end == 0 {
            // Brand-new file: write the header.
            let mut header = Vec::with_capacity(HEADER_LEN as usize);
            header.extend_from_slice(MAGIC);
            header.extend_from_slice(&VERSION.to_le_bytes());
            file.write_all(&header).map_err(DbError::Io)?;
            end = HEADER_LEN;
        } else {
            if end < HEADER_LEN {
                return Err(DbError::InvalidFile(format!(
                    "{}: too short to be a virtual file system database",
                    path.display()
                )));
            }
            let mut header = [0u8; HEADER_LEN as usize];
            file.seek(SeekFrom::Start(0)).map_err(DbError::Io)?;
            file.read_exact(&mut header).map_err(DbError::Io)?;
            let version = u32::from_le_bytes(header[4..8].try_into().unwrap());
            if &header[0..4] != MAGIC || ![LEGACY_VERSION, VERSION].contains(&version) {
                return Err(DbError::InvalidFile(format!(
                    "{}: not a virtual file system database (bad header)",
                    path.display()
                )));
            }

            let legacy = version == LEGACY_VERSION;
            legacy_file = legacy;
            let record_header_len = if legacy {
                LEGACY_RECORD_HEADER_LEN
            } else {
                RECORD_HEADER_LEN
            };

            // Walk the log and index each record; keep the latest record
            // per path. Stop at the first incomplete or malformed record.
            let file_len = end;
            let mut pos = HEADER_LEN;
            while pos < file_len {
                if file_len - pos < record_header_len {
                    break;
                }
                let mut rec_header = vec![0u8; record_header_len as usize];
                file.seek(SeekFrom::Start(pos)).map_err(DbError::Io)?;
                file.read_exact(&mut rec_header).map_err(DbError::Io)?;
                let path_len = u32::from_le_bytes(rec_header[0..4].try_into().unwrap()) as u64;
                let data_len = u64::from_le_bytes(rec_header[4..12].try_into().unwrap());
                let metadata_len = if legacy {
                    0
                } else {
                    u32::from_le_bytes(rec_header[12..16].try_into().unwrap()) as u64
                };
                let hash_start = if legacy { 12 } else { 16 };
                let hash = match std::str::from_utf8(
                    &rec_header[hash_start..hash_start + HASH_LEN as usize],
                ) {
                    Ok(hash) => hash.to_string(),
                    Err(_) => break, // malformed hash: treat as torn tail
                };
                let metadata_offset = pos + record_header_len;
                let path_offset = match metadata_offset.checked_add(metadata_len) {
                    Some(offset) => offset,
                    None => break,
                };
                let data_offset = match path_offset.checked_add(path_len) {
                    Some(offset) => offset,
                    None => break,
                };
                let record_end = match data_offset.checked_add(data_len) {
                    Some(offset) => offset,
                    None => break,
                };
                if record_end > file_len {
                    break; // record runs past end of file: torn tail
                }
                let mut metadata = vec![0u8; metadata_len as usize];
                file.seek(SeekFrom::Start(metadata_offset))
                    .map_err(DbError::Io)?;
                file.read_exact(&mut metadata).map_err(DbError::Io)?;
                let mut path_bytes = vec![0u8; path_len as usize];
                file.seek(SeekFrom::Start(path_offset))
                    .map_err(DbError::Io)?;
                file.read_exact(&mut path_bytes).map_err(DbError::Io)?;
                let name = match std::str::from_utf8(&path_bytes) {
                    Ok(name) => name.to_string(),
                    Err(_) => break,
                };
                if hash == TOMBSTONE_HASH && data_len == 0 {
                    index.remove(&name);
                } else {
                    index.insert(
                        name,
                        Record {
                            offset: data_offset,
                            len: data_len,
                            hash,
                            metadata,
                        },
                    );
                }
                pos = data_offset + data_len;
            }

            if pos < file_len {
                // Drop the torn tail so future appends overwrite it.
                file.set_len(pos).map_err(DbError::Io)?;
                end = pos;
            }
        }

        let mut vfs = Self {
            path: path.to_path_buf(),
            file,
            end,
            index,
        };
        if legacy_file {
            // Rewrite old records once so all subsequent appends use the new
            // layout and old records receive inferred metadata at the asset
            // database layer.
            vfs.compact()?;
        }
        Ok(vfs)
    }

    /// The path of the single database file backing this file system.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Insert (or replace) a file at `relative_path`.
    ///
    /// The new content is appended to the database file immediately.
    /// Returns the previous content if a file already existed at that path.
    pub fn insert(
        &mut self,
        relative_path: &str,
        data: impl Into<Vec<u8>>,
    ) -> Result<Option<Vec<u8>>, DbError> {
        self.insert_with_metadata(relative_path, [], data)
    }

    /// Insert (or replace) a file and attach opaque metadata to its record.
    ///
    /// Metadata is stored and indexed separately from the payload. Looking up
    /// metadata never reads the payload bytes.
    pub fn insert_with_metadata(
        &mut self,
        relative_path: &str,
        metadata: impl Into<Vec<u8>>,
        data: impl Into<Vec<u8>>,
    ) -> Result<Option<Vec<u8>>, DbError> {
        let path = normalize_path(relative_path)?;
        let metadata = metadata.into();
        let data = data.into();
        let previous = self
            .index
            .get(&path)
            .map(|record| self.read_data(record))
            .transpose()?;
        self.append(&path, &metadata, &data)?;
        Ok(previous)
    }

    /// Look up a file by its (normalized) relative path.
    ///
    /// Only the requested file's data is read from the database file.
    pub fn get(&self, relative_path: &str) -> Option<Vec<u8>> {
        let path = normalize_path(relative_path).ok()?;
        let record = self.index.get(&path)?;
        self.read_data(record).ok()
    }

    /// Return a copy of a file's opaque record metadata without reading its
    /// payload.
    pub fn metadata(&self, relative_path: &str) -> Option<Vec<u8>> {
        let path = normalize_path(relative_path).ok()?;
        self.index.get(&path).map(|record| record.metadata.clone())
    }

    /// Return the recorded payload length without reading the payload.
    pub fn data_len(&self, relative_path: &str) -> Option<u64> {
        let path = normalize_path(relative_path).ok()?;
        self.index.get(&path).map(|record| record.len)
    }

    /// Whether a file exists at the (normalized) relative path.
    pub fn contains(&self, relative_path: &str) -> bool {
        match normalize_path(relative_path) {
            Ok(path) => self.index.contains_key(&path),
            Err(_) => false,
        }
    }

    /// Remove a file, returning its content if it existed.
    ///
    /// The record is dropped from the index; its bytes remain in the
    /// database file until the file is rewritten.
    pub fn remove(&mut self, relative_path: &str) -> Option<Vec<u8>> {
        self.remove_persisted(relative_path).ok().flatten()
    }

    /// Remove a file and append a tombstone to persist the removal.
    pub fn remove_persisted(&mut self, relative_path: &str) -> Result<Option<Vec<u8>>, DbError> {
        let path = normalize_path(relative_path)?;
        let Some(record) = self.index.remove(&path) else {
            return Ok(None);
        };
        let data = self.read_data(&record)?;
        if let Err(error) = self.append_tombstone(&path) {
            self.index.insert(path, record);
            return Err(error);
        }
        Ok(Some(data))
    }

    /// Rewrite the database, dropping payloads for removed or replaced files.
    pub fn compact(&mut self) -> Result<(), DbError> {
        let temporary = self.path.with_extension("db.tmp");
        let _ = std::fs::remove_file(&temporary);
        let mut replacement = Self::create(&temporary)?;
        for (current_path, current_record) in &self.index {
            replacement.insert_with_metadata(
                current_path,
                current_record.metadata.clone(),
                self.read_data(current_record)?,
            )?;
        }
        replacement.flush()?;
        drop(replacement);
        std::fs::rename(&temporary, &self.path).map_err(DbError::Io)?;
        *self = Self::open(&self.path)?;
        Ok(())
    }

    /// Iterate over the normalized relative paths of all stored files.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.index.keys().map(String::as_str)
    }

    /// Number of files stored.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Whether the virtual file system holds no files.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Flush pending writes to disk.
    pub fn flush(&self) -> Result<(), DbError> {
        self.file.sync_data().map_err(DbError::Io)
    }

    fn read_data(&self, record: &Record) -> Result<Vec<u8>, DbError> {
        let mut data = vec![0u8; record.len as usize];
        let read = self
            .file
            .read_at(&mut data, record.offset)
            .map_err(DbError::Io)?;
        if read as u64 != record.len {
            return Err(DbError::InvalidFile(format!(
                "{}: short read at offset {} ({} of {} bytes)",
                self.path.display(),
                record.offset,
                read,
                record.len
            )));
        }
        Ok(data)
    }

    fn append(&mut self, path: &str, metadata: &[u8], data: &[u8]) -> Result<(), DbError> {
        let hash = sha256::digest(data);
        let path_len = path.len() as u32;
        let metadata_len = u32::try_from(metadata.len()).map_err(|_| {
            DbError::InvalidFile(format!(
                "record metadata is too large: {} bytes",
                metadata.len()
            ))
        })?;
        let data_offset = self.end + RECORD_HEADER_LEN + metadata.len() as u64 + path.len() as u64;
        let mut record = Vec::with_capacity(
            RECORD_HEADER_LEN as usize + metadata.len() + path.len() + data.len(),
        );
        record.extend_from_slice(&path_len.to_le_bytes());
        record.extend_from_slice(&(data.len() as u64).to_le_bytes());
        record.extend_from_slice(&metadata_len.to_le_bytes());
        record.extend_from_slice(hash.as_bytes());
        record.extend_from_slice(metadata);
        record.extend_from_slice(path.as_bytes());
        record.extend_from_slice(data);
        let written = self.file.write_at(&record, self.end).map_err(DbError::Io)?;
        if written as u64 != record.len() as u64 {
            return Err(DbError::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                format!(
                    "{}: short write at offset {} ({} of {} bytes)",
                    self.path.display(),
                    self.end,
                    written,
                    record.len()
                ),
            )));
        }
        self.end += record.len() as u64;
        self.index.insert(
            path.to_string(),
            Record {
                offset: data_offset,
                len: data.len() as u64,
                hash,
                metadata: metadata.to_vec(),
            },
        );
        Ok(())
    }

    fn append_tombstone(&mut self, path: &str) -> Result<(), DbError> {
        let path_len = path.len() as u32;
        let mut record = Vec::with_capacity(RECORD_HEADER_LEN as usize + path.len());
        record.extend_from_slice(&path_len.to_le_bytes());
        record.extend_from_slice(&0u64.to_le_bytes());
        record.extend_from_slice(&0u32.to_le_bytes());
        record.extend_from_slice(TOMBSTONE_HASH.as_bytes());
        record.extend_from_slice(path.as_bytes());
        let written = self.file.write_at(&record, self.end).map_err(DbError::Io)?;
        if written != record.len() {
            return Err(DbError::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                format!(
                    "{}: short write at offset {} ({} of {} bytes)",
                    self.path.display(),
                    self.end,
                    written,
                    record.len()
                ),
            )));
        }
        self.end += record.len() as u64;
        Ok(())
    }

    /// Check every stored file against the hash recorded when it was
    /// written.
    ///
    /// The data of every file is re-read from the database file, hashed,
    /// and compared against the hash stored in the file's record header.
    /// Returns a report describing any detected integrity problems: for
    /// each affected asset the path, the expected and actual sizes, and
    /// both hash values. I/O failures surface as an error, since the
    /// check could not be completed.
    pub fn verify_integrity(&self) -> Result<IntegrityReport, DbError> {
        let mut checked = 0usize;
        let mut problems = Vec::new();
        for (path, record) in &self.index {
            checked += 1;
            let (actual_size, actual_hash) = self.hash_record_data(record)?;
            if actual_size != record.len || actual_hash != record.hash {
                let detail = if actual_size != record.len {
                    format!(
                        "only {actual_size} of {} bytes found at offset {} (expected {})",
                        record.len, record.offset, record.len
                    )
                } else {
                    format!(
                        "hash mismatch: stored {}, current {}",
                        record.hash, actual_hash
                    )
                };
                problems.push(IntegrityProblem {
                    path: path.clone(),
                    expected_size: record.len,
                    actual_size,
                    stored_hash: record.hash.clone(),
                    actual_hash,
                    detail,
                });
            }
        }
        Ok(IntegrityReport { checked, problems })
    }

    /// Read and hash the bytes a record's data currently occupies in the
    /// database file.
    ///
    /// Returns the number of bytes actually read (less than the recorded
    /// length if the file was truncated out from under the open handle)
    /// and the lowercase hex-encoded SHA-256 hash of exactly those bytes.
    fn hash_record_data(&self, record: &Record) -> Result<(u64, String), DbError> {
        let mut data = vec![0u8; record.len as usize];
        let read = self
            .file
            .read_at(&mut data, record.offset)
            .map_err(DbError::Io)?;
        data.truncate(read as usize);
        Ok((read as u64, sha256::digest(&data)))
    }
}

/// Normalize an asset path into its canonical form:
///
/// - Leading `./` and `/` segments are stripped.
/// - Backslashes are treated as separators.
/// - Empty segments (from `//`) are dropped.
/// - `..` segments are rejected, as is a path that ends up empty.
pub fn normalize_path(path: &str) -> Result<String, DbError> {
    let mut out = Vec::new();
    for component in path.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => return Err(DbError::InvalidPath(path.to_string())),
            other => out.push(other),
        }
    }
    if out.is_empty() {
        return Err(DbError::InvalidPath(path.to_string()));
    }
    Ok(out.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Unique temp directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("asset-server-vfs-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn db_file(&self) -> PathBuf {
            self.0.join("vfs.db")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn normalizes_paths() {
        assert_eq!(
            normalize_path("textures/sky.png").unwrap(),
            "textures/sky.png"
        );
        assert_eq!(
            normalize_path("/textures/sky.png").unwrap(),
            "textures/sky.png"
        );
        assert_eq!(
            normalize_path("./textures/sky.png").unwrap(),
            "textures/sky.png"
        );
        assert_eq!(
            normalize_path("textures//sky.png").unwrap(),
            "textures/sky.png"
        );
        assert_eq!(
            normalize_path("textures\\sky.png").unwrap(),
            "textures/sky.png"
        );
        assert_eq!(normalize_path(".//./a/b").unwrap(), "a/b");
    }

    #[test]
    fn rejects_unsafe_paths() {
        assert!(matches!(normalize_path(""), Err(DbError::InvalidPath(_))));
        assert!(matches!(normalize_path("."), Err(DbError::InvalidPath(_))));
        assert!(matches!(
            normalize_path("../x"),
            Err(DbError::InvalidPath(_))
        ));
        assert!(matches!(
            normalize_path("a/../../x"),
            Err(DbError::InvalidPath(_))
        ));
    }

    #[test]
    fn stores_reads_and_removes_files() {
        let tmp = TempDir::new();
        let mut vfs = Vfs::open(tmp.db_file()).unwrap();
        assert!(vfs.is_empty());
        assert_eq!(vfs.len(), 0);

        vfs.insert("a/b.txt", b"hello").unwrap();
        assert_eq!(vfs.len(), 1);
        assert_eq!(vfs.get("a/b.txt"), Some(b"hello".to_vec()));
        // Lookup normalizes the requested path too.
        assert_eq!(vfs.get("./a/b.txt"), Some(b"hello".to_vec()));
        assert!(vfs.contains("a/b.txt"));
        assert!(!vfs.contains("missing.txt"));
        assert!(vfs.get("missing.txt").is_none());

        // Re-inserting the same path replaces the content.
        let previous = vfs.insert("a/b.txt", b"world").unwrap();
        assert_eq!(previous, Some(b"hello".to_vec()));
        assert_eq!(vfs.len(), 1);
        assert_eq!(vfs.get("a/b.txt"), Some(b"world".to_vec()));

        assert_eq!(vfs.remove("a/b.txt"), Some(b"world".to_vec()));
        assert!(vfs.is_empty());
        assert_eq!(vfs.remove("a/b.txt"), None);
    }

    #[test]
    fn lists_paths() {
        let tmp = TempDir::new();
        let mut vfs = Vfs::open(tmp.db_file()).unwrap();
        vfs.insert("b/b.bin", b"2").unwrap();
        vfs.insert("a/a.bin", b"1").unwrap();
        let paths: Vec<&str> = vfs.paths().collect();
        assert_eq!(paths, vec!["a/a.bin", "b/b.bin"]);
    }

    #[test]
    fn data_persists_across_reopen() {
        let tmp = TempDir::new();
        let file = tmp.db_file();
        {
            let mut vfs = Vfs::open(&file).unwrap();
            vfs.insert("a.bin", b"alpha").unwrap();
            vfs.insert("b/c.bin", b"beta-beta").unwrap();
        }

        // Reopening rebuilds the index from the file on disk.
        let vfs = Vfs::open(&file).unwrap();
        assert_eq!(vfs.len(), 2);
        assert!(vfs.contains("a.bin"));
        assert!(vfs.contains("b/c.bin"));
        assert_eq!(vfs.get("a.bin"), Some(b"alpha".to_vec()));
        assert_eq!(vfs.get("b/c.bin"), Some(b"beta-beta".to_vec()));
        let paths: Vec<&str> = vfs.paths().collect();
        assert_eq!(paths, vec!["a.bin", "b/c.bin"]);
    }

    #[test]
    fn record_metadata_persists_without_loading_payload() {
        let tmp = TempDir::new();
        let file = tmp.db_file();
        {
            let mut vfs = Vfs::open(&file).unwrap();
            vfs.insert_with_metadata("asset.bin", b"record metadata", b"payload")
                .unwrap();
            assert_eq!(vfs.metadata("asset.bin"), Some(b"record metadata".to_vec()));
        }

        let vfs = Vfs::open(&file).unwrap();
        assert_eq!(vfs.metadata("asset.bin"), Some(b"record metadata".to_vec()));
        assert_eq!(vfs.data_len("asset.bin"), Some(7));
    }

    #[test]
    fn migrates_legacy_version_two_files() {
        let tmp = TempDir::new();
        let file = tmp.db_file();
        let path = b"legacy.bin";
        let data = b"legacy payload";
        let hash = sha256::digest(data);
        let mut raw = Vec::new();
        raw.extend_from_slice(MAGIC);
        raw.extend_from_slice(&LEGACY_VERSION.to_le_bytes());
        raw.extend_from_slice(&(path.len() as u32).to_le_bytes());
        raw.extend_from_slice(&(data.len() as u64).to_le_bytes());
        raw.extend_from_slice(hash.as_bytes());
        raw.extend_from_slice(path);
        raw.extend_from_slice(data);
        std::fs::write(&file, raw).unwrap();

        let vfs = Vfs::open(&file).unwrap();
        assert_eq!(vfs.get("legacy.bin"), Some(data.to_vec()));
        assert_eq!(vfs.metadata("legacy.bin"), Some(Vec::new()));
        let header = std::fs::read(&file).unwrap();
        assert_eq!(&header[4..8], &VERSION.to_le_bytes());
    }

    #[test]
    fn reimport_after_reopen_keeps_latest_version() {
        let tmp = TempDir::new();
        let file = tmp.db_file();
        {
            let mut vfs = Vfs::open(&file).unwrap();
            vfs.insert("x.bin", b"v1").unwrap();
        }
        let mut vfs = Vfs::open(&file).unwrap();
        assert_eq!(vfs.get("x.bin"), Some(b"v1".to_vec()));
        let previous = vfs.insert("x.bin", b"v2").unwrap();
        assert_eq!(previous, Some(b"v1".to_vec()));
        assert_eq!(vfs.len(), 1);
        assert_eq!(vfs.get("x.bin"), Some(b"v2".to_vec()));
    }

    #[test]
    fn create_discards_previous_file() {
        let tmp = TempDir::new();
        let file = tmp.db_file();
        {
            let mut vfs = Vfs::open(&file).unwrap();
            vfs.insert("a.bin", b"old").unwrap();
        }
        let mut vfs = Vfs::create(&file).unwrap();
        assert!(vfs.is_empty());
        vfs.insert("a.bin", b"new").unwrap();
        assert_eq!(vfs.get("a.bin"), Some(b"new".to_vec()));
    }

    #[test]
    fn rejects_files_that_are_not_databases() {
        let tmp = TempDir::new();
        let file = tmp.db_file();

        std::fs::write(&file, b"not a vfs database").unwrap();
        assert!(matches!(Vfs::open(&file), Err(DbError::InvalidFile(_))));

        // A file shorter than the header is invalid as well.
        std::fs::write(&file, b"AV").unwrap();
        assert!(matches!(Vfs::open(&file), Err(DbError::InvalidFile(_))));
    }

    #[test]
    fn recovers_from_torn_tail() {
        let tmp = TempDir::new();
        let file = tmp.db_file();
        {
            let mut vfs = Vfs::open(&file).unwrap();
            vfs.insert("ok.bin", b"good-data").unwrap();
        }
        // Simulate a crash mid-write: a partial record after the last good one.
        let mut f = OpenOptions::new().append(true).open(&file).unwrap();
        f.write_all(b"\x02\x00\x00\x00").unwrap();
        drop(f);

        let vfs = Vfs::open(&file).unwrap();
        assert_eq!(vfs.len(), 1);
        assert_eq!(vfs.get("ok.bin"), Some(b"good-data".to_vec()));
        // Appending after recovery still works.
        drop(vfs);
        let mut vfs = Vfs::open(&file).unwrap();
        vfs.insert("more.bin", b"more").unwrap();
        assert_eq!(vfs.len(), 2);
    }

    #[test]
    fn external_truncation_surfaces_as_invalid_file() {
        let tmp = TempDir::new();
        let file = tmp.db_file();
        let mut vfs = Vfs::open(&file).unwrap();
        vfs.insert("a.bin", b"0123456789abcdef").unwrap();

        // Shrink the file out from under the open handle, as another
        // process writing to it could.
        let f = OpenOptions::new().write(true).open(&file).unwrap();
        f.set_len(HEADER_LEN + RECORD_HEADER_LEN + 5).unwrap();
        drop(f);

        // Reading the truncated record is an error, not zero-padded garbage.
        assert!(vfs.get("a.bin").is_none());
        assert!(matches!(
            vfs.insert("a.bin", b"new"),
            Err(DbError::InvalidFile(_))
        ));

        // A fresh open discards the torn record.
        drop(vfs);
        let vfs = Vfs::open(&file).unwrap();
        assert!(vfs.is_empty());
    }

    #[test]
    fn missing_parent_directory_is_an_io_error() {
        let tmp = TempDir::new();
        let missing = tmp.0.join("no/such/dir/vfs.db");
        assert!(matches!(Vfs::open(missing), Err(DbError::Io(_))));
    }
}
