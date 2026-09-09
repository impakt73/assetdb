use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::vfs::{normalize_path, Vfs};

/// FNV-1a 64-bit hash, used to turn a relative asset path into a stable key.
///
/// The hash is deterministic across processes and platforms (unlike
/// `std::collections::hash_map::DefaultHasher`, which is randomized), so
/// database keys are stable record identifiers.
pub fn fnv1a64(data: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// A stable identifier for an asset: the FNV-1a 64-bit hash of the asset's
/// normalized path relative to the root asset directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AssetKey(u64);

impl AssetKey {
    /// Compute the key for a relative asset path.
    ///
    /// The path is normalized before hashing, so `"/a/b.png"`, `./a/b.png`
    /// and `a/b.png` all produce the same key.
    pub fn from_path(relative_path: &str) -> Self {
        match normalize_path(relative_path) {
            Ok(path) => Self(fnv1a64(path.as_bytes())),
            Err(_) => Self(fnv1a64(relative_path.as_bytes())),
        }
    }

    /// The raw 64-bit value of the key.
    pub fn as_u64(self) -> u64 {
        self.0
    }

    /// The key as a 16-character, zero-padded, lowercase hex string.
    pub fn to_hex(self) -> String {
        format!("{:016x}", self.0)
    }
}

impl fmt::Display for AssetKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl FromStr for AssetKey {
    type Err = DbError;

    /// Parse a key from its hex string representation (e.g. `"af63dc4c8601ec8c"`).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(DbError::InvalidKey(s.to_string()));
        }
        let value = u64::from_str_radix(s, 16).map_err(|_| DbError::InvalidKey(s.to_string()))?;
        Ok(Self(value))
    }
}

/// Errors that can occur when working with the asset database.
#[derive(Debug)]
pub enum DbError {
    /// The relative path was empty or tried to escape the root directory.
    InvalidPath(String),
    /// A string could not be parsed as an asset key.
    InvalidKey(String),
    /// A disk import failed to read the file.
    Io(std::io::Error),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath(path) => write!(f, "invalid asset path: {path:?}"),
            Self::InvalidKey(key) => write!(f, "invalid asset key: {key:?}"),
            Self::Io(err) => write!(f, "io error while importing asset: {err}"),
        }
    }
}

impl std::error::Error for DbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for DbError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// A single asset record: its key, normalized relative path, and raw data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub key: AssetKey,
    pub path: String,
    data: Vec<u8>,
}

impl Asset {
    /// The raw data of the asset.
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

/// A simple asset database.
///
/// Assets live in an in-memory virtual file system. Each asset's database
/// key is the FNV-1a 64-bit hash of its path relative to the root asset
/// directory, which keeps assets unique and lets them be looked up either
/// by key or by relative path.
#[derive(Debug, Clone)]
pub struct AssetDatabase {
    root: PathBuf,
    vfs: Vfs,
    keys: BTreeMap<AssetKey, String>,
}

impl AssetDatabase {
    /// Create a new, empty database rooted at `root`.
    ///
    /// The root asset directory is where `import_from_disk` resolves
    /// relative paths against.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let vfs = Vfs::new(root.display().to_string());
        Self {
            root,
            vfs,
            keys: BTreeMap::new(),
        }
    }

    /// The root asset directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The virtual file system backing this database.
    pub fn vfs(&self) -> &Vfs {
        &self.vfs
    }

    /// Import an asset that exists on disk under the root asset directory.
    ///
    /// `relative_path` is the path of the file relative to the root, for
    /// example `"models/cube.obj"`. Returns the key of the imported asset.
    pub fn import_from_disk(&mut self, relative_path: &str) -> Result<AssetKey, DbError> {
        let path = normalize_path(relative_path)?;
        let data = std::fs::read(self.root.join(&path))?;
        Ok(self.store(path, data))
    }

    /// Import an asset from in-memory data at `relative_path`.
    ///
    /// Re-importing the same relative path keeps a single record and
    /// replaces its data. Returns the key of the asset.
    pub fn import_from_memory(
        &mut self,
        relative_path: &str,
        data: impl Into<Vec<u8>>,
    ) -> Result<AssetKey, DbError> {
        let path = normalize_path(relative_path)?;
        Ok(self.store(path, data.into()))
    }

    fn store(&mut self, path: String, data: Vec<u8>) -> AssetKey {
        let key = AssetKey(fnv1a64(path.as_bytes()));
        // `path` is already normalized, so this cannot fail.
        self.vfs
            .insert(&path, data)
            .expect("normalized path is always a valid vfs path");
        self.keys.insert(key, path);
        key
    }

    /// Look up an asset record by key.
    pub fn get(&self, key: AssetKey) -> Option<Asset> {
        let path = self.keys.get(&key)?;
        let data = self.vfs.get(path)?.to_vec();
        Some(Asset {
            key,
            path: path.clone(),
            data,
        })
    }

    /// Look up an asset record by (normalized) relative path.
    pub fn get_by_path(&self, relative_path: &str) -> Option<Asset> {
        let path = normalize_path(relative_path).ok()?;
        let key = AssetKey(fnv1a64(path.as_bytes()));
        let data = self.vfs.get(&path)?.to_vec();
        Some(Asset { key, path, data })
    }

    /// The key for `relative_path`, if an asset with that path is stored.
    pub fn key_for_path(&self, relative_path: &str) -> Option<AssetKey> {
        let key = AssetKey::from_path(relative_path);
        self.keys.contains_key(&key).then_some(key)
    }

    pub fn contains_key(&self, key: AssetKey) -> bool {
        self.keys.contains_key(&key)
    }

    pub fn contains_path(&self, relative_path: &str) -> bool {
        self.vfs.contains(relative_path)
    }

    /// Iterate over the keys of all stored assets (sorted).
    pub fn keys(&self) -> impl Iterator<Item = AssetKey> {
        self.keys.keys().copied()
    }

    /// Iterate over the normalized relative paths of all stored assets.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.vfs.paths()
    }

    /// Number of assets stored.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the database holds no assets.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
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
                .join(format!("asset-server-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn key_is_fnv1a_hash_of_relative_path() {
        // Published FNV-1a 64-bit test vectors.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);

        assert_eq!(AssetKey::from_path("a").to_hex(), "af63dc4c8601ec8c");
        // Equivalent normalized paths produce the same key.
        assert_eq!(
            AssetKey::from_path("/textures/sky.png"),
            AssetKey::from_path("textures/sky.png")
        );
        assert_ne!(
            AssetKey::from_path("textures/sky.png"),
            AssetKey::from_path("textures/sun.png")
        );
    }

    #[test]
    fn key_round_trips_through_hex_string() {
        let key = AssetKey::from_path("textures/sky.png");
        let hex = key.to_string();
        assert_eq!(hex.len(), 16);
        assert_eq!(AssetKey::from_str(&hex).unwrap(), key);
        assert!(AssetKey::from_str("nothex").is_err());
        assert!(AssetKey::from_str("").is_err());
    }

    #[test]
    fn import_from_memory_and_lookup_by_key_and_path() {
        let mut db = AssetDatabase::new("/root/assets");
        let data: &[u8] = b"PNG-bytes-here";
        let key = db.import_from_memory("textures/sky.png", data).unwrap();

        assert_eq!(key, AssetKey::from_path("textures/sky.png"));
        assert_eq!(db.len(), 1);
        assert!(!db.is_empty());

        // Look up by key and extract the data.
        let asset = db.get(key).expect("asset found by key");
        assert_eq!(asset.key, key);
        assert_eq!(asset.path, "textures/sky.png");
        assert_eq!(asset.data(), data);

        // Look up by relative path.
        let asset = db
            .get_by_path("textures/sky.png")
            .expect("asset found by path");
        assert_eq!(asset.key, key);
        assert_eq!(asset.data(), data);
        // Path lookup normalizes the requested path.
        assert_eq!(
            db.get_by_path("./textures/sky.png")
                .expect("normalized path lookup"),
            asset
        );

        assert!(db.contains_key(key));
        assert!(db.contains_path("textures/sky.png"));
        assert_eq!(db.key_for_path("textures/sky.png"), Some(key));
        assert!(db.get(AssetKey::from_path("other.png")).is_none());
        assert!(db.get_by_path("other.png").is_none());
        assert!(!db.contains_path("other.png"));
    }

    #[test]
    fn reimporting_same_path_keeps_one_unique_record() {
        let mut db = AssetDatabase::new("/root/assets");
        let key1 = db.import_from_memory("a.txt", b"v1").unwrap();
        let key2 = db.import_from_memory("a.txt", b"v2").unwrap();

        assert_eq!(key1, key2);
        assert_eq!(db.len(), 1);
        assert_eq!(db.get(key1).expect("record exists").data(), b"v2".as_slice());
    }

    #[test]
    fn import_from_disk() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.0.join("models")).unwrap();
        std::fs::write(tmp.0.join("models/cube.obj"), b"v 0 0 0").unwrap();
        std::fs::write(tmp.0.join("shaders.glsl"), b"void main(){}").unwrap();

        let mut db = AssetDatabase::new(&tmp.0);
        let key = db.import_from_disk("models/cube.obj").unwrap();
        assert_eq!(key, AssetKey::from_path("models/cube.obj"));

        let asset = db.get(key).expect("asset imported from disk");
        assert_eq!(asset.path, "models/cube.obj");
        assert_eq!(asset.data(), b"v 0 0 0".as_slice());

        let key2 = db.import_from_disk("shaders.glsl").unwrap();
        assert_ne!(key, key2);
        assert_eq!(db.len(), 2);
        assert_eq!(
            db.get_by_path("shaders.glsl")
                .expect("found by path")
                .data(),
            b"void main(){}".as_slice()
        );
        assert_eq!(db.paths().count(), 2);
        assert_eq!(db.keys().count(), 2);
    }

    #[test]
    fn disk_import_errors() {
        let tmp = TempDir::new();
        let mut db = AssetDatabase::new(&tmp.0);
        assert!(matches!(
            db.import_from_disk("missing.bin"),
            Err(DbError::Io(_))
        ));
        // Paths that escape the root are rejected before any disk access.
        assert!(matches!(
            db.import_from_disk("../escape.bin"),
            Err(DbError::InvalidPath(_))
        ));
        assert!(matches!(
            db.import_from_memory("", b"x"),
            Err(DbError::InvalidPath(_))
        ));
        assert!(db.is_empty());
    }

    #[test]
    fn lists_all_assets() {
        let mut db = AssetDatabase::new("/root/assets");
        db.import_from_memory("b/b.bin", b"2").unwrap();
        db.import_from_memory("a/a.bin", b"1").unwrap();

        let paths: Vec<&str> = db.paths().collect();
        assert_eq!(paths, vec!["a/a.bin", "b/b.bin"]);
        assert_eq!(db.keys().count(), 2);
    }
}
