# asset-server

A simple asset database for game/server assets. Assets are imported from disk
or memory, identified by a stable hash key, and stored in a **single database
file on disk** so that opening the database never loads all asset data into
memory at once.

The crate has no external dependencies.

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
a small in-memory index rebuilt from it.

### `Vfs`: single-file storage

`Vfs` is a virtual file system backed by the single `assets.db` file. All
stored file contents live in one **append-only log**; only a lightweight
in-memory index of `path -> (offset, length)` is kept, so stored data is read
from disk on demand, one file at a time.

File format:

```text
[ "AVFS" ][ version u32 LE ]            (header, 8 bytes)
[ path_len u32 LE ][ data_len u64 LE ]  (record header, 12 bytes)
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

`AssetDatabase` is the high-level API. It keeps only lightweight index entries
(key, path, file offset) in memory:

- `new(root)` — open (or create) the database at `root/assets.db`; existing
  records are indexed and immediately available.
- `import_from_disk(relative_path)` — copy a file from under `root` into the
  database.
- `import_from_memory(relative_path, data)` — store in-memory data; re-importing
  the same path replaces its data under the same key.
- `get(key)` / `get_by_path(relative_path)` — look up an `Asset`; the data is
  read from the database file on demand.
- `paths()`, `keys()`, `key_for_path`, `contains_key`, `contains_path`,
  `len`, `is_empty` — index queries.
- `root()`, `db_file()` — the root directory and the single database file.

### Errors

`DbError` covers:

- `InvalidPath(String)` — empty path or path traversal (`..`).
- `InvalidKey(String)` — a string that is not a valid hex key.
- `Io(std::io::Error)` — disk I/O failures (import, read, write, flush).
- `InvalidFile(String)` — the database file is missing/corrupt/not a database
  (bad header, torn record, short read).

## Usage

```rust
use asset_server::{AssetDatabase, AssetKey, DbError};
use std::str::FromStr;

fn main() -> Result<(), DbError> {
    let mut db = AssetDatabase::new("/srv/assets")?;

    // Import from disk (relative to the root) and from memory.
    let disk_key = db.import_from_disk("models/cube.obj")?;
    db.import_from_memory("note.txt", b"hello")?;

    // Look up by key or by relative path; data is read from assets.db on demand.
    let asset = db.get(disk_key).expect("present");
    let note = db.get_by_path("note.txt").expect("present");

    // Keys are stable hex strings; re-opening the database restores everything.
    let key = AssetKey::from_str(&disk_key.to_hex())?;
    let _again = AssetDatabase::new("/srv/assets")?;
    Ok(())
}
```

## Development

```sh
cargo test            # unit tests (persistence, recovery, round-trips, ...)
cargo clippy --all-targets
```

Tests use unique per-process temp directories and clean up after themselves.
