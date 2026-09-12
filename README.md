# asset-server

A simple asset database for game/server assets. Asset records are imported from
disk or memory, identified by a stable hash key, and stored with type and data
format metadata in a **single database file on disk**. Opening the database
indexes records without loading or decoding the binary asset data.

The crate's only external dependency is the `sha256` crate, used to hash
stored asset data.

## Design

### On-disk layout

A database is rooted at an asset directory (`root`):

```
<root>/
    assets.db            # single database file holding all asset data
    models/cube.obj      # source assets imported via import_from_disk (optional)
    ...
```

`AssetDatabase::new(root)` opens `<root>/assets.db`, creating it if missing.
Everything the database knows about its assets is either in that one file or in
a small in-memory record index rebuilt from it.

### `Vfs`: single-file storage

`Vfs` is a virtual file system backed by the single `assets.db` file. All
stored file contents live in one **append-only log**; only a lightweight
in-memory index of `path -> (offset, length, record metadata, content hash)` is
kept, so stored data is read from disk on demand, one file at a time.

File format (version 3):

```text
[ "AVFS" ][ version u32 LE ]            (header, 8 bytes)
[ path_len u32 LE ][ data_len u64 LE ][ metadata_len u32 LE ] (record header, 16 bytes)
[ sha256 hash of the data (64 lowercase hex chars) ]
[ record metadata bytes ]
[ path bytes (UTF-8, normalized) ]
[ data bytes ]
... more records, appended in insertion order
```

- **Insert/replace**: re-inserting a path appends a new record; the index
  keeps the latest one. Old payloads remain in the file until it is rewritten
  (e.g. via `Vfs::create`).
- **Open**: `Vfs::open` walks the log and rebuilds the index without reading
  any data payloads. `Vfs::create` discards a previous file and starts empty.
- **Crash recovery**: a truncated tail (e.g. from a crash mid-write) is
  detected and truncated away when the file is opened again. A file with a bad
  magic/version, or shorter than the header, is rejected with
  `DbError::InvalidFile`.
- **Corruption guard**: reads and writes verify the number of bytes actually
  transferred, so a file modified underneath an open handle (short read/write)
  surfaces as an error instead of silently returning zero-padded data.
- **Content hashes**: every record stores the lowercase hex-encoded SHA-256
  hash of its data, computed when the record is written (via the `sha256`
  crate). `Vfs::verify_integrity` re-reads and re-hashes each stored file and
  reports every mismatch.
- **Record metadata**: `Vfs` stores opaque metadata separately from the data
  payload. `Vfs::metadata` can retrieve it without reading the payload.

### `AssetKey`

Each asset's database key is the **FNV-1a 64-bit hash of its normalized
relative path**. FNV-1a is deterministic across processes and platforms
(unlike Rust's randomized `DefaultHasher`), so keys are stable identifiers.
Keys round-trip through their 16-character lowercase hex string form
(`"af63dc4c8601ec8c"`).

### Path normalization

Asset paths are normalized before storage, hashing, and lookup:

- leading `./` and `/` segments are stripped,
- backslashes are treated as separators,
- empty segments (from `//`) are dropped,
- `..` segments are rejected, as is a path that ends up empty.

So `"/textures/sky.png"`, `"./textures/sky.png"`, and `"textures\sky.png"`
all refer to the same asset.

### `AssetDatabase`

`AssetDatabase` is the high-level API. It keeps lightweight `AssetRecord`
entries in memory. An `AssetRecord` contains the key, normalized path,
application-defined asset type, and data format version; it never contains
binary data:

- `new(root)` — open (or create) the database at `root/assets.db`; existing
  records are indexed and immediately available.
- `import_from_disk(relative_path)` — copy a file from under `root` into the
  database, deriving the type from the file extension and using data format
  version 1.
- `import_from_memory(relative_path, data)` — store in-memory data; re-importing
  the same path replaces its data under the same key.
- `import_from_disk_with_metadata` / `import_from_memory_with_metadata` — import
  data with explicit `AssetMetadata` instead of inferred defaults.
- `get_record(key)` / `get_record_by_path(relative_path)` — look up an
  `AssetRecord` without reading the binary data.
- `get_data(key)` — read the binary data separately as `AssetData`.
- `get(key)` / `get_by_path(relative_path)` — compatibility convenience that
  combines a record with loaded data.
- `records()`, `paths()`, `keys()`, `key_for_path`, `contains_key`,
  `contains_path`, `len`, `is_empty` — index queries.
- `verify_integrity()` — check every stored asset against the content hash
  recorded when it was added or last updated; returns an `IntegrityReport`.
- `root()`, `db_file()` — the root directory and the single database file.

### File integrity

Because every record stores the SHA-256 hash of its data, the database can
detect corruption at any time without trusting anything:

```rust
let report = db.verify_integrity()?;
if report.is_clean() {
    println!("{report}"); // "integrity check passed: 3 asset(s) verified"
} else {
    for problem in &report.problems {
        eprintln!("corrupt asset: {problem}");
    }
}
```

`IntegrityProblem` carries everything needed to identify and report the
affected asset:

- `path` — the asset's relative path,
- `expected_size` / `actual_size` — recorded size vs. bytes actually found,
- `stored_hash` / `actual_hash` — both SHA-256 hashes as hex strings,
- `detail` — a human-readable explanation of the mismatch.

The check re-reads the data of every asset from the database file, so it
catches both byte-level corruption (hash mismatch, sizes unchanged) and
truncation (fewer bytes on disk than recorded). I/O failures surface as
`DbError`, since the check could not be completed.

### Errors

`DbError` covers:

- `InvalidPath(String)` — empty path or path traversal (`..`).
- `InvalidKey(String)` — a string that is not a valid hex key.
- `Io(std::io::Error)` — disk I/O failures (import, read, write, flush).
- `InvalidFile(String)` — the database file is missing/corrupt/not a database
  (bad header, torn record, short read).
- `InvalidMetadata(String)` — an asset type is empty or otherwise invalid.

## Usage

```rust
use asset_server::{AssetDatabase, AssetKey, DbError};
use std::str::FromStr;

fn main() -> Result<(), DbError> {
    let mut db = AssetDatabase::new("/srv/assets")?;

    // Import from disk (relative to the root) and from memory.
    let disk_key = db.import_from_disk("models/cube.obj")?;
    db.import_from_memory("note.txt", b"hello")?;

    // Look up metadata without reading or decoding the binary payload.
    let record = db.get_record(disk_key).expect("present");
    assert_eq!(record.asset_type, "obj");
    assert_eq!(record.data_format_version, 1);

    // Read data separately only when it is needed.
    let data = db.get_data(disk_key).expect("present");
    assert!(!data.as_bytes().is_empty());

    // Keys are stable hex strings; re-opening the database restores everything.
    let key = AssetKey::from_str(&disk_key.to_hex())?;
    let _again = AssetDatabase::new("/srv/assets")?;

    // Check stored data against the hashes recorded at import time.
    let report = db.verify_integrity()?;
    assert!(report.is_clean());
    Ok(())
}
```

## Development

```sh
cargo test            # unit tests (persistence, recovery, round-trips, ...)
cargo clippy --all-targets
```

Tests use unique per-process temp directories and clean up after themselves.

## CLI

The crate also builds an `asset-server` binary. Database roots are directories;
the database file is created as `<root>/assets.db`.

```sh
asset-server create ./assets
asset-server add ./assets models/cube.obj
asset-server add ./assets source.bin stored/name.bin
asset-server search ./assets cube
asset-server dump ./assets models/cube.obj ./out/cube.obj
asset-server dump ./assets ./out/all-assets
asset-server remove ./assets models/cube.obj
asset-server compact ./assets
asset-server check ./assets
```

`add` imports a source file relative to the database root. Its optional third
argument is the normalized path stored in the database. The CLI derives the
asset type from that stored path and uses data format version 1. `search`
matches asset paths and hexadecimal keys using metadata only. `remove` and
`dump` accept either a stored path or an asset key. Omitting the selector from
`dump` exports every asset below the destination directory. `check` exits with
status 2 when corruption is found and status 1 when the database cannot be
opened or checked.

`remove` records a deletion without rewriting the database. Use `compact` as a
separate maintenance operation to reclaim space from removed and replaced
assets.
