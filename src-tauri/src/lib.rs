use rusqlite::{params, Connection};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::Manager;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DriveInfo {
    name: String,
    mount_point: String,
    filesystem: Option<String>,
    total_bytes: Option<u64>,
    available_bytes: Option<u64>,
    persistent_identifier: Option<String>,
    device_identifier: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CataloguedDrive {
    persistent_identifier: String,
    name: String,
    filesystem: Option<String>,
    total_bytes: Option<u64>,
    available_bytes: Option<u64>,
    last_mount_point: Option<String>,
    last_scanned_at: Option<i64>,
    file_count: i64,
    directory_count: i64,
    catalogued_bytes: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanResult {
    persistent_identifier: String,
    file_count: i64,
    directory_count: i64,
    catalogued_bytes: i64,
    skipped_count: i64,
    scanned_at: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogueEntry {
    relative_path: String,
    name: String,
    is_directory: bool,
    size_bytes: Option<i64>,
    modified_at: Option<i64>,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn system_time_unix(value: Result<SystemTime, std::io::Error>) -> Option<i64> {
    value.ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64)
}

fn database_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Unable to locate Media Mapper data directory: {error}"))?;
    fs::create_dir_all(&directory)
        .map_err(|error| format!("Unable to create Media Mapper data directory: {error}"))?;
    Ok(directory.join("catalogue.sqlite3"))
}

fn open_database(path: &Path) -> Result<Connection, String> {
    let connection =
        Connection::open(path).map_err(|error| format!("Unable to open catalogue database: {error}"))?;

    connection
        .execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS drives (
                persistent_identifier TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                filesystem TEXT,
                total_bytes INTEGER,
                available_bytes INTEGER,
                last_mount_point TEXT,
                last_seen_at INTEGER NOT NULL,
                last_scanned_at INTEGER
            );

            CREATE TABLE IF NOT EXISTS files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                drive_id TEXT NOT NULL,
                relative_path TEXT NOT NULL,
                name TEXT NOT NULL,
                is_directory INTEGER NOT NULL,
                size_bytes INTEGER,
                modified_at INTEGER,
                UNIQUE(drive_id, relative_path),
                FOREIGN KEY(drive_id) REFERENCES drives(persistent_identifier) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_files_drive_path
                ON files(drive_id, relative_path);
            ",
        )
        .map_err(|error| format!("Unable to initialise catalogue database: {error}"))?;

    Ok(connection)
}

#[cfg(target_os = "macos")]
fn external_drives() -> Result<Vec<DriveInfo>, String> {
    use std::collections::HashMap;
    use std::process::Command;

    fn parse_info(text: &str) -> HashMap<String, String> {
        text.lines()
            .filter_map(|line| line.split_once(':'))
            .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
            .collect()
    }

    fn parse_bytes(value: Option<&String>) -> Option<u64> {
        let value = value?;
        if let Some((_, remainder)) = value.split_once('(') {
            let digits: String = remainder.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                return digits.parse().ok();
            }
        }
        value.split_whitespace().next()?.replace(',', "").parse().ok()
    }

    let volumes = Path::new("/Volumes");
    let entries =
        fs::read_dir(volumes).map_err(|error| format!("Unable to read /Volumes: {error}"))?;
    let mut drives = Vec::new();

    for entry in entries.flatten() {
        let mount_path = entry.path();
        if !mount_path.is_dir() {
            continue;
        }

        let output = match Command::new("/usr/sbin/diskutil")
            .arg("info")
            .arg(&mount_path)
            .output()
        {
            Ok(output) if output.status.success() => output,
            _ => continue,
        };

        let info = parse_info(&String::from_utf8_lossy(&output.stdout));
        if info.get("Device Location").map(String::as_str) == Some("Internal") {
            continue;
        }
        if info
            .get("Protocol")
            .is_some_and(|protocol| protocol.eq_ignore_ascii_case("Network"))
        {
            continue;
        }

        let mount_point = info
            .get("Mount Point")
            .cloned()
            .unwrap_or_else(|| mount_path.to_string_lossy().into_owned());
        let fallback_name = mount_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("External drive")
            .to_owned();

        drives.push(DriveInfo {
            name: info
                .get("Volume Name")
                .filter(|name| !name.is_empty())
                .cloned()
                .unwrap_or(fallback_name),
            mount_point,
            filesystem: info
                .get("File System Personality")
                .or_else(|| info.get("Type (Bundle)"))
                .cloned(),
            total_bytes: parse_bytes(info.get("Disk Size")),
            available_bytes: parse_bytes(info.get("Volume Free Space")),
            persistent_identifier: info
                .get("Volume UUID")
                .or_else(|| info.get("Disk / Partition UUID"))
                .cloned(),
            device_identifier: info.get("Device Identifier").cloned(),
        });
    }

    drives.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(drives)
}

#[cfg(not(target_os = "macos"))]
fn external_drives() -> Result<Vec<DriveInfo>, String> {
    Err("External drive discovery is implemented for macOS in this milestone.".to_string())
}

#[tauri::command]
fn list_external_drives() -> Result<Vec<DriveInfo>, String> {
    external_drives()
}

#[tauri::command]
fn list_catalogued_drives(app: tauri::AppHandle) -> Result<Vec<CataloguedDrive>, String> {
    let connection = open_database(&database_path(&app)?)?;
    let mut statement = connection
        .prepare(
            "
            SELECT
                d.persistent_identifier,
                d.name,
                d.filesystem,
                d.total_bytes,
                d.available_bytes,
                d.last_mount_point,
                d.last_scanned_at,
                COALESCE(SUM(CASE WHEN f.is_directory = 0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN f.is_directory = 1 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN f.is_directory = 0 THEN f.size_bytes ELSE 0 END), 0)
            FROM drives d
            LEFT JOIN files f ON f.drive_id = d.persistent_identifier
            GROUP BY d.persistent_identifier
            ORDER BY lower(d.name)
            ",
        )
        .map_err(|error| format!("Unable to query catalogue: {error}"))?;

    let rows = statement
        .query_map([], |row| {
            Ok(CataloguedDrive {
                persistent_identifier: row.get(0)?,
                name: row.get(1)?,
                filesystem: row.get(2)?,
                total_bytes: row.get::<_, Option<i64>>(3)?.map(|value| value as u64),
                available_bytes: row.get::<_, Option<i64>>(4)?.map(|value| value as u64),
                last_mount_point: row.get(5)?,
                last_scanned_at: row.get(6)?,
                file_count: row.get(7)?,
                directory_count: row.get(8)?,
                catalogued_bytes: row.get(9)?,
            })
        })
        .map_err(|error| format!("Unable to read catalogue: {error}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Unable to read catalogue rows: {error}"))
}

fn scan_directory(
    root: &Path,
    current: &Path,
    transaction: &rusqlite::Transaction<'_>,
    drive_id: &str,
    counters: &mut (i64, i64, i64, i64),
) -> Result<(), String> {
    let entries = match fs::read_dir(current) {
        Ok(entries) => entries,
        Err(_) => {
            counters.3 += 1;
            return Ok(());
        }
    };

    for entry_result in entries {
        let entry = match entry_result {
            Ok(entry) => entry,
            Err(_) => {
                counters.3 += 1;
                continue;
            }
        };

        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                counters.3 += 1;
                continue;
            }
        };

        // Never follow symlinks. This prevents a catalogue scan escaping the selected volume.
        if metadata.file_type().is_symlink() {
            counters.3 += 1;
            continue;
        }

        let relative = match path.strip_prefix(root) {
            Ok(relative) => relative.to_string_lossy().into_owned(),
            Err(_) => {
                counters.3 += 1;
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_directory = metadata.is_dir();
        let size_bytes = if metadata.is_file() {
            Some(metadata.len().min(i64::MAX as u64) as i64)
        } else {
            None
        };
        let modified_at = system_time_unix(metadata.modified());

        transaction
            .execute(
                "
                INSERT INTO files
                    (drive_id, relative_path, name, is_directory, size_bytes, modified_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ",
                params![
                    drive_id,
                    relative,
                    name,
                    if is_directory { 1 } else { 0 },
                    size_bytes,
                    modified_at
                ],
            )
            .map_err(|error| format!("Unable to write catalogue entry: {error}"))?;

        if is_directory {
            counters.1 += 1;
            scan_directory(root, &path, transaction, drive_id, counters)?;
        } else if metadata.is_file() {
            counters.0 += 1;
            counters.2 = counters.2.saturating_add(size_bytes.unwrap_or(0));
        } else {
            counters.3 += 1;
        }
    }

    Ok(())
}

#[tauri::command]
async fn scan_drive(
    app: tauri::AppHandle,
    persistent_identifier: String,
) -> Result<ScanResult, String> {
    let drive = external_drives()?
        .into_iter()
        .find(|drive| drive.persistent_identifier.as_deref() == Some(&persistent_identifier))
        .ok_or_else(|| "That drive is no longer connected.".to_string())?;

    let database = database_path(&app)?;

    tauri::async_runtime::spawn_blocking(move || {
        let drive_id = drive
            .persistent_identifier
            .clone()
            .ok_or_else(|| "This drive does not provide a stable volume identifier.".to_string())?;
        let root = PathBuf::from(&drive.mount_point);

        if !root.is_dir() {
            return Err("The drive mount point is no longer available.".to_string());
        }

        let mut connection = open_database(&database)?;
        let transaction = connection
            .transaction()
            .map_err(|error| format!("Unable to start catalogue transaction: {error}"))?;

        let scanned_at = now_unix();
        transaction
            .execute(
                "
                INSERT INTO drives (
                    persistent_identifier, name, filesystem, total_bytes,
                    available_bytes, last_mount_point, last_seen_at, last_scanned_at
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)
                ON CONFLICT(persistent_identifier) DO UPDATE SET
                    name = excluded.name,
                    filesystem = excluded.filesystem,
                    total_bytes = excluded.total_bytes,
                    available_bytes = excluded.available_bytes,
                    last_mount_point = excluded.last_mount_point,
                    last_seen_at = excluded.last_seen_at
                ",
                params![
                    drive_id,
                    drive.name,
                    drive.filesystem,
                    drive.total_bytes.map(|value| value.min(i64::MAX as u64) as i64),
                    drive.available_bytes.map(|value| value.min(i64::MAX as u64) as i64),
                    drive.mount_point,
                    scanned_at
                ],
            )
            .map_err(|error| format!("Unable to save drive record: {error}"))?;

        // A rescan replaces the previous snapshot atomically. If scanning fails,
        // the transaction rolls back and the last good catalogue remains intact.
        transaction
            .execute("DELETE FROM files WHERE drive_id = ?1", params![drive_id])
            .map_err(|error| format!("Unable to prepare drive rescan: {error}"))?;

        let mut counters = (0_i64, 0_i64, 0_i64, 0_i64);
        scan_directory(&root, &root, &transaction, &drive_id, &mut counters)?;

        transaction
            .execute(
                "UPDATE drives SET last_scanned_at = ?1 WHERE persistent_identifier = ?2",
                params![scanned_at, drive_id],
            )
            .map_err(|error| format!("Unable to finish drive scan: {error}"))?;

        transaction
            .commit()
            .map_err(|error| format!("Unable to commit catalogue scan: {error}"))?;

        Ok(ScanResult {
            persistent_identifier: drive_id,
            file_count: counters.0,
            directory_count: counters.1,
            catalogued_bytes: counters.2,
            skipped_count: counters.3,
            scanned_at,
        })
    })
    .await
    .map_err(|error| format!("Drive scan task failed: {error}"))?
}

#[tauri::command]
fn list_catalogue_entries(
    app: tauri::AppHandle,
    persistent_identifier: String,
) -> Result<Vec<CatalogueEntry>, String> {
    let connection = open_database(&database_path(&app)?)?;
    let mut statement = connection
        .prepare(
            "
            SELECT relative_path, name, is_directory, size_bytes, modified_at
            FROM files
            WHERE drive_id = ?1
            ORDER BY relative_path
            LIMIT 500
            ",
        )
        .map_err(|error| format!("Unable to query catalogue entries: {error}"))?;

    let rows = statement
        .query_map(params![persistent_identifier], |row| {
            Ok(CatalogueEntry {
                relative_path: row.get(0)?,
                name: row.get(1)?,
                is_directory: row.get::<_, i64>(2)? != 0,
                size_bytes: row.get(3)?,
                modified_at: row.get(4)?,
            })
        })
        .map_err(|error| format!("Unable to read catalogue entries: {error}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Unable to read catalogue entry rows: {error}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            list_external_drives,
            list_catalogued_drives,
            scan_drive,
            list_catalogue_entries
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
