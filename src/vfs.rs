use std::collections::BTreeMap;

use crate::db::DbError;

/// A minimal in-memory virtual file system.
///
/// Files are stored as raw bytes, keyed by their normalized path relative to
/// a virtual root directory. It acts as the backing store for the
/// [`AssetDatabase`](crate::AssetDatabase).
#[derive(Debug, Clone)]
pub struct Vfs {
    root: String,
    files: BTreeMap<String, Vec<u8>>,
}

impl Vfs {
    /// Create a new empty virtual file system rooted at `root`.
    pub fn new(root: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            files: BTreeMap::new(),
        }
    }

    /// The virtual root directory label.
    pub fn root(&self) -> &str {
        &self.root
    }

    /// Insert (or replace) a file at `relative_path`.
    ///
    /// Returns the previous content if a file already existed at that path.
    pub fn insert(
        &mut self,
        relative_path: &str,
        data: impl Into<Vec<u8>>,
    ) -> Result<Option<Vec<u8>>, DbError> {
        let path = normalize_path(relative_path)?;
        Ok(self.files.insert(path, data.into()))
    }

    /// Look up a file by its (normalized) relative path.
    pub fn get(&self, relative_path: &str) -> Option<&[u8]> {
        self.files
            .get(&normalize_path(relative_path).ok()?)
            .map(Vec::as_slice)
    }

    /// Whether a file exists at the (normalized) relative path.
    pub fn contains(&self, relative_path: &str) -> bool {
        match normalize_path(relative_path) {
            Ok(path) => self.files.contains_key(&path),
            Err(_) => false,
        }
    }

    /// Remove a file, returning its content if it existed.
    pub fn remove(&mut self, relative_path: &str) -> Option<Vec<u8>> {
        self.files.remove(&normalize_path(relative_path).ok()?)
    }

    /// Iterate over the normalized relative paths of all stored files.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// Number of files stored.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the virtual file system holds no files.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

impl Default for Vfs {
    fn default() -> Self {
        Self::new(String::new())
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
        let mut vfs = Vfs::new("assets");
        assert!(vfs.is_empty());
        assert_eq!(vfs.len(), 0);

        vfs.insert("a/b.txt", b"hello").unwrap();
        assert_eq!(vfs.len(), 1);
        assert_eq!(vfs.get("a/b.txt"), Some(b"hello".as_slice()));
        // Lookup normalizes the requested path too.
        assert_eq!(vfs.get("./a/b.txt"), Some(b"hello".as_slice()));
        assert!(vfs.contains("a/b.txt"));
        assert!(!vfs.contains("missing.txt"));
        assert!(vfs.get("missing.txt").is_none());

        // Re-inserting the same path replaces the content.
        let previous = vfs.insert("a/b.txt", b"world").unwrap();
        assert_eq!(previous, Some(b"hello".to_vec()));
        assert_eq!(vfs.len(), 1);
        assert_eq!(vfs.get("a/b.txt"), Some(b"world".as_slice()));

        assert_eq!(vfs.remove("a/b.txt"), Some(b"world".to_vec()));
        assert!(vfs.is_empty());
        assert_eq!(vfs.remove("a/b.txt"), None);
    }

    #[test]
    fn lists_paths() {
        let mut vfs = Vfs::new("assets");
        vfs.insert("b/b.bin", b"2").unwrap();
        vfs.insert("a/a.bin", b"1").unwrap();
        let paths: Vec<&str> = vfs.paths().collect();
        assert_eq!(paths, vec!["a/a.bin", "b/b.bin"]);
    }
}
