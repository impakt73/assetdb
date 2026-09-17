use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use assetdb::{AssetDatabase, AssetKey, DbError};

const USAGE: &str = "Usage:
  assetdb create <root>
  assetdb add <root> <source> [asset-path]
  assetdb remove <root> <asset-path-or-key>
  assetdb search <root> <query>
  assetdb dump <root> <asset-path-or-key> <destination>
  assetdb compact <root>
  assetdb check <root>";

#[derive(Debug)]
enum CliError {
    Usage(String),
    Database(DbError),
    Io(std::io::Error),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => write!(f, "{message}\n\n{USAGE}"),
            Self::Database(error) => error.fmt(f),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl From<DbError> for CliError {
    fn from(error: DbError) -> Self {
        Self::Database(error)
    }
}

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), CliError> {
    let mut arguments = env::args().skip(1);
    let Some(command) = arguments.next() else {
        return Err(CliError::Usage("a command is required".to_string()));
    };
    if command == "help" || command == "--help" || command == "-h" {
        println!("{USAGE}");
        return Ok(());
    }

    match command.as_str() {
        "create" => {
            let root = required(&mut arguments, "root")?;
            reject_extra(&mut arguments)?;
            std::fs::create_dir_all(&root)?;
            let db = AssetDatabase::new(&root)?;
            println!("created {}", db.db_file().display());
        }
        "add" => {
            let root = required(&mut arguments, "root")?;
            let source = required(&mut arguments, "source")?;
            let asset_path = arguments.next().unwrap_or_else(|| source.clone());
            reject_extra(&mut arguments)?;
            let mut db = open_database(&root)?;
            let key = if asset_path == source {
                db.import_from_disk(&source)?
            } else {
                let data = std::fs::read(Path::new(&root).join(&source))?;
                db.import_from_memory(&asset_path, data)?
            };
            println!("{key}");
        }
        "remove" => {
            let root = required(&mut arguments, "root")?;
            let selector = required(&mut arguments, "asset path or key")?;
            reject_extra(&mut arguments)?;
            let mut db = open_database(&root)?;
            let removed = remove_selected(&mut db, &selector)?;
            let Some(asset) = removed else {
                return Err(CliError::Usage(format!("asset not found: {selector}")));
            };
            println!("removed {} ({})", asset.path, asset.key);
        }
        "search" => {
            let root = required(&mut arguments, "root")?;
            let query = required(&mut arguments, "query")?;
            reject_extra(&mut arguments)?;
            let db = open_database(&root)?;
            for key in db.keys() {
                let record = db.get_record(key).expect("indexed record must be readable");
                if record.path.contains(&query) || key.to_string().contains(&query) {
                    println!("{}\t{}", record.key, record.path);
                }
            }
        }
        "dump" => {
            let root = required(&mut arguments, "root")?;
            let db = open_database(&root)?;
            let first = required(&mut arguments, "asset path/key or destination")?;
            let Some(second) = arguments.next() else {
                let destination = PathBuf::from(first);
                std::fs::create_dir_all(&destination)?;
                for key in db.keys() {
                    let asset = db.get(key).expect("indexed asset must be readable");
                    let output = destination.join(&asset.path);
                    if let Some(parent) = output.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(output, asset.data())?;
                }
                reject_extra(&mut arguments)?;
                println!("dumped {} asset(s) to {}", db.len(), destination.display());
                return Ok(());
            };
            reject_extra(&mut arguments)?;
            let destination = PathBuf::from(second);
            let asset = selected_asset(&db, &first)?
                .ok_or_else(|| CliError::Usage(format!("asset not found: {first}")))?;
            if let Some(parent) = destination
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&destination, asset.data())?;
            println!("dumped {} to {}", asset.path, destination.display());
        }
        "compact" => {
            let root = required(&mut arguments, "root")?;
            reject_extra(&mut arguments)?;
            let mut db = open_database(&root)?;
            db.compact()?;
            println!("compacted {}", db.db_file().display());
        }
        "check" | "verify" => {
            let root = required(&mut arguments, "root")?;
            reject_extra(&mut arguments)?;
            let db = open_database(&root)?;
            let report = db.verify_integrity()?;
            println!("{report}");
            if !report.is_clean() {
                std::process::exit(2);
            }
        }
        _ => return Err(CliError::Usage(format!("unknown command: {command}"))),
    }
    Ok(())
}

fn required(arguments: &mut impl Iterator<Item = String>, name: &str) -> Result<String, CliError> {
    arguments
        .next()
        .ok_or_else(|| CliError::Usage(format!("missing {name}")))
}

fn reject_extra(arguments: &mut impl Iterator<Item = String>) -> Result<(), CliError> {
    if let Some(argument) = arguments.next() {
        return Err(CliError::Usage(format!("unexpected argument: {argument}")));
    }
    Ok(())
}

fn open_database(root: &str) -> Result<AssetDatabase, CliError> {
    std::fs::create_dir_all(root)?;
    Ok(AssetDatabase::new(root)?)
}

fn selected_asset(db: &AssetDatabase, selector: &str) -> Result<Option<assetdb::Asset>, CliError> {
    if let Ok(key) = AssetKey::from_str(selector) {
        Ok(db.get(key))
    } else {
        Ok(db.get_by_path(selector))
    }
}

fn remove_selected(
    db: &mut AssetDatabase,
    selector: &str,
) -> Result<Option<assetdb::Asset>, CliError> {
    if let Ok(key) = AssetKey::from_str(selector) {
        Ok(db.remove(key)?)
    } else {
        Ok(db.remove_by_path(selector)?)
    }
}
