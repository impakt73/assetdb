pub mod db;
pub mod vfs;

pub use db::{
    Asset, AssetData, AssetDatabase, AssetKey, AssetMetadata, AssetRecord,
    DEFAULT_DATA_FORMAT_VERSION, DbError, IntegrityProblem, IntegrityReport,
};
pub use vfs::{Vfs, normalize_path};

pub fn add(left: u64, right: u64) -> u64 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }

    #[test]
    fn end_to_end_memory_and_disk() {
        use std::str::FromStr;
        use std::sync::atomic::{AtomicU32, Ordering};

        static COUNTER: AtomicU32 = AtomicU32::new(100);

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("asset-server-e2e-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let result: Result<(), DbError> = {
            std::fs::write(dir.join("icon.png"), b"\x89PNG").unwrap();

            let mut db = AssetDatabase::new(&dir).unwrap();
            let disk_key = db.import_from_disk("icon.png").unwrap();
            let mem_key = db.import_from_memory("note.txt", b"hello").unwrap();

            assert_eq!(db.len(), 2);
            assert_eq!(db.get(disk_key).unwrap().data(), b"\x89PNG".as_slice());
            assert_eq!(
                db.get_by_path("note.txt").unwrap().data(),
                b"hello".as_slice()
            );
            assert_eq!(db.get(mem_key).unwrap().path, "note.txt");
            // Searching by the hex key string also works.
            let parsed = AssetKey::from_str(&disk_key.to_hex()).unwrap();
            assert_eq!(parsed, disk_key);
            assert_eq!(db.get(parsed).unwrap().path, "icon.png");
            Ok(())
        };
        std::fs::remove_dir_all(&dir).unwrap();
        result.unwrap();
    }
}
