use rusqlite::{params, Connection};
use serde::Serialize;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{Emitter, Manager};

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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanProgress {
    persistent_identifier: String,
    file_count: i64,
    directory_count: i64,
    catalogued_bytes: i64,
    skipped_count: i64,
    current_path: String,
}

fn emit_scan_progress(app: &tauri::AppHandle, drive_id: &str, counters: &(i64, i64, i64, i64), current_path: &str) {
    let _ = app.emit("scan-progress", ScanProgress {
        persistent_identifier: drive_id.to_owned(),
        file_count: counters.0,
        directory_count: counters.1,
        catalogued_bytes: counters.2,
        skipped_count: counters.3,
        current_path: current_path.to_owned(),
    });
}

static CANCELLED_SCANS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
const SCAN_CANCELLED: &str = "__MEDIA_MAPPER_SCAN_CANCELLED__";

fn cancelled_scans() -> &'static Mutex<HashSet<String>> {
    CANCELLED_SCANS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn scan_is_cancelled(drive_id: &str) -> bool {
    cancelled_scans().lock().map(|scans| scans.contains(drive_id)).unwrap_or(false)
}

fn clear_scan_cancel(drive_id: &str) {
    if let Ok(mut scans) = cancelled_scans().lock() {
        scans.remove(drive_id);
    }
}

#[tauri::command]
fn cancel_scan(persistent_identifier: String) -> Result<(), String> {
    cancelled_scans()
        .lock()
        .map_err(|_| "Unable to access scan cancellation state.".to_string())?
        .insert(persistent_identifier);
    Ok(())
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LibrarySearchResult {
    drive_id: String,
    drive_name: String,
    relative_path: String,
    name: String,
    is_directory: bool,
    size_bytes: Option<i64>,
    modified_at: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LargestFile {
    drive_id: String,
    drive_name: String,
    relative_path: String,
    name: String,
    size_bytes: i64,
    modified_at: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DuplicateFile {
    drive_id: String,
    drive_name: String,
    relative_path: String,
    name: String,
    size_bytes: i64,
    modified_at: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DuplicateGroup {
    name: String,
    size_bytes: i64,
    copies: i64,
    potential_wasted_bytes: i64,
    files: Vec<DuplicateFile>,
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
                last_scanned_at INTEGER,
                file_count INTEGER NOT NULL DEFAULT 0,
                directory_count INTEGER NOT NULL DEFAULT 0,
                catalogued_bytes INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                drive_id TEXT NOT NULL,
                relative_path TEXT NOT NULL,
                name TEXT NOT NULL,
                parent_path TEXT NOT NULL DEFAULT '',
                is_directory INTEGER NOT NULL,
                size_bytes INTEGER,
                modified_at INTEGER,
                UNIQUE(drive_id, relative_path),
                FOREIGN KEY(drive_id) REFERENCES drives(persistent_identifier) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_files_drive_path
                ON files(drive_id, relative_path);

            CREATE INDEX IF NOT EXISTS idx_files_drive_name
                ON files(drive_id, name COLLATE NOCASE);
            ",
        )
        .map_err(|error| format!("Unable to initialise catalogue database: {error}"))?;

    let existing_columns = {
        let mut statement = connection
            .prepare("PRAGMA table_info(drives)")
            .map_err(|error| format!("Unable to inspect drive schema: {error}"))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| format!("Unable to inspect drive columns: {error}"))?;
        rows.collect::<Result<HashSet<_>, _>>()
            .map_err(|error| format!("Unable to read drive columns: {error}"))?
    };

    let mut added_summary_columns = false;
    if !existing_columns.contains("file_count") {
        connection
            .execute("ALTER TABLE drives ADD COLUMN file_count INTEGER NOT NULL DEFAULT 0", [])
            .map_err(|error| format!("Unable to add drive file count: {error}"))?;
        added_summary_columns = true;
    }
    if !existing_columns.contains("directory_count") {
        connection
            .execute("ALTER TABLE drives ADD COLUMN directory_count INTEGER NOT NULL DEFAULT 0", [])
            .map_err(|error| format!("Unable to add drive directory count: {error}"))?;
        added_summary_columns = true;
    }
    if !existing_columns.contains("catalogued_bytes") {
        connection
            .execute("ALTER TABLE drives ADD COLUMN catalogued_bytes INTEGER NOT NULL DEFAULT 0", [])
            .map_err(|error| format!("Unable to add drive byte count: {error}"))?;
        added_summary_columns = true;
    }

    // Existing catalogues are preserved. This aggregation runs once when the
    // summary columns are first added; subsequent catalogue loads read the
    // cached totals directly from the drive row.
    if added_summary_columns {
        connection
            .execute_batch(
                "
                UPDATE drives
                SET file_count = (
                        SELECT COUNT(*) FROM files
                        WHERE files.drive_id = drives.persistent_identifier
                          AND files.is_directory = 0
                    ),
                    directory_count = (
                        SELECT COUNT(*) FROM files
                        WHERE files.drive_id = drives.persistent_identifier
                          AND files.is_directory = 1
                    ),
                    catalogued_bytes = (
                        SELECT COALESCE(SUM(files.size_bytes), 0) FROM files
                        WHERE files.drive_id = drives.persistent_identifier
                          AND files.is_directory = 0
                    );
                ",
            )
            .map_err(|error| format!("Unable to migrate catalogue totals: {error}"))?;
    }

    let file_columns = {
        let mut statement = connection
            .prepare("PRAGMA table_info(files)")
            .map_err(|error| format!("Unable to inspect file schema: {error}"))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| format!("Unable to inspect file columns: {error}"))?;
        rows.collect::<Result<HashSet<_>, _>>()
            .map_err(|error| format!("Unable to read file columns: {error}"))?
    };

    if !file_columns.contains("parent_path") {
        connection
            .execute("ALTER TABLE files ADD COLUMN parent_path TEXT NOT NULL DEFAULT ''", [])
            .map_err(|error| format!("Unable to add catalogue parent paths: {error}"))?;

        let existing_paths = {
            let mut statement = connection
                .prepare("SELECT id, relative_path FROM files")
                .map_err(|error| format!("Unable to read catalogue paths: {error}"))?;
            let rows = statement
                .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
                .map_err(|error| format!("Unable to read catalogue paths: {error}"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("Unable to read catalogue paths: {error}"))?
        };

        let transaction = connection
            .unchecked_transaction()
            .map_err(|error| format!("Unable to migrate catalogue parent paths: {error}"))?;
        {
            let mut update = transaction
                .prepare("UPDATE files SET parent_path = ?1 WHERE id = ?2")
                .map_err(|error| format!("Unable to prepare parent path migration: {error}"))?;

            for (id, relative_path) in existing_paths {
                let parent_path = Path::new(&relative_path)
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default();

                update
                    .execute(params![parent_path, id])
                    .map_err(|error| format!("Unable to migrate catalogue parent path: {error}"))?;
            }
        }
        transaction
            .commit()
            .map_err(|error| format!("Unable to commit parent path migration: {error}"))?;
    }

    connection
        .execute_batch(
            "
            CREATE INDEX IF NOT EXISTS idx_files_drive_parent
                ON files(drive_id, parent_path);
            ",
        )
        .map_err(|error| format!("Unable to initialise parent path index: {error}"))?;

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
                d.file_count,
                d.directory_count,
                d.catalogued_bytes
            FROM drives d
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
    insert_statement: &mut rusqlite::Statement<'_>,
    drive_id: &str,
    counters: &mut (i64, i64, i64, i64),
    app: &tauri::AppHandle,
    last_emit_at: &mut Instant,
) -> Result<(), String> {
    // Keep directory traversal on the heap rather than the call stack. This
    // avoids stack overflow on drives with unusually deep folder structures.
    let mut directories = vec![root.to_path_buf()];

    while let Some(current) = directories.pop() {
        if scan_is_cancelled(drive_id) {
            return Err(SCAN_CANCELLED.to_string());
        }

        let entries = fs::read_dir(&current).map_err(|error| {
            let relative = current
                .strip_prefix(root)
                .unwrap_or(&current)
                .to_string_lossy();
            format!(
                "Scan could not read folder '{}': {error}. The previous catalogue has been kept unchanged.",
                if relative.is_empty() { "/" } else { relative.as_ref() }
            )
        })?;

        for entry_result in entries {
            if scan_is_cancelled(drive_id) {
                return Err(SCAN_CANCELLED.to_string());
            }

            let entry = entry_result.map_err(|error| {
                let relative = current
                    .strip_prefix(root)
                    .unwrap_or(&current)
                    .to_string_lossy();
                format!(
                    "Scan could not read an entry in folder '{}': {error}. The previous catalogue has been kept unchanged.",
                    if relative.is_empty() { "/" } else { relative.as_ref() }
                )
            })?;

            let name = entry.file_name().to_string_lossy().into_owned();

            // Ignore macOS Finder metadata rather than cataloguing it as user content.
            // AppleDouble sidecars mirror real files as tiny `._*` entries; .DS_Store
            // stores Finder folder preferences. Neither belongs in Media Mapper's catalogue.
            if name.starts_with("._") || name == ".DS_Store" {
                continue;
            }

            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy();
                format!(
                    "Scan could not read metadata for '{}': {error}. The previous catalogue has been kept unchanged.",
                    relative
                )
            })?;

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
            let is_directory = metadata.is_dir();
            let size_bytes = if metadata.is_file() {
                Some(metadata.len().min(i64::MAX as u64) as i64)
            } else {
                None
            };
            let modified_at = system_time_unix(metadata.modified());
            let parent_path = Path::new(&relative)
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();

            insert_statement
                .execute(params![
                    drive_id,
                    relative,
                    name,
                    parent_path,
                    if is_directory { 1 } else { 0 },
                    size_bytes,
                    modified_at
                ])
                .map_err(|error| format!("Unable to write catalogue entry: {error}"))?;

            if is_directory {
                counters.1 += 1;
                directories.push(path);
            } else if metadata.is_file() {
                counters.0 += 1;
                counters.2 = counters.2.saturating_add(size_bytes.unwrap_or(0));
            } else {
                counters.3 += 1;
            }

            if last_emit_at.elapsed() >= Duration::from_millis(150) {
                emit_scan_progress(app, drive_id, counters, &relative);
                *last_emit_at = Instant::now();
            }
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
    let progress_app = app.clone();

    // Clear stale cancellation before the worker starts.
    clear_scan_cancel(&persistent_identifier);

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

        // A rescan replaces the previous snapshot atomically. Any filesystem read
        // failure aborts the scan, so an incomplete traversal can never replace the
        // last good catalogue. Cancellation and other failures roll back here too.
        transaction
            .execute("DELETE FROM files WHERE drive_id = ?1", params![drive_id])
            .map_err(|error| format!("Unable to prepare drive rescan: {error}"))?;

        let mut insert_statement = transaction
            .prepare_cached(
                "
                INSERT INTO files
                    (drive_id, relative_path, name, parent_path, is_directory, size_bytes, modified_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ",
            )
            .map_err(|error| format!("Unable to prepare catalogue writer: {error}"))?;

        let mut counters = (0_i64, 0_i64, 0_i64, 0_i64);
        let mut last_emit_at = Instant::now();
        emit_scan_progress(&progress_app, &drive_id, &counters, "");
        if let Err(error) = scan_directory(
            &root,
            &mut insert_statement,
            &drive_id,
            &mut counters,
            &progress_app,
            &mut last_emit_at,
        ) {
            clear_scan_cancel(&drive_id);
            if error == SCAN_CANCELLED {
                return Err("Scan cancelled.".to_string());
            }
            return Err(error);
        }
        emit_scan_progress(&progress_app, &drive_id, &counters, "");
        clear_scan_cancel(&drive_id);

        drop(insert_statement);

        transaction
            .execute(
                "
                UPDATE drives
                SET last_scanned_at = ?1,
                    file_count = ?2,
                    directory_count = ?3,
                    catalogued_bytes = ?4
                WHERE persistent_identifier = ?5
                ",
                params![scanned_at, counters.0, counters.1, counters.2, drive_id],
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
    parent_path: String,
) -> Result<Vec<CatalogueEntry>, String> {
    let connection = open_database(&database_path(&app)?)?;

    let mut statement = connection
        .prepare(
            "
            SELECT relative_path, name, is_directory, size_bytes, modified_at
            FROM files
            WHERE drive_id = ?1
              AND parent_path = ?2
            ORDER BY is_directory DESC, lower(name), name
            ",
        )
        .map_err(|error| format!("Unable to query catalogue entries: {error}"))?;

    let rows = statement
        .query_map(params![persistent_identifier, parent_path], |row| {
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

fn normalised_search_expression(column: &str) -> String {
    format!(
        "lower(replace(replace(replace({column}, '.', ' '), '_', ' '), '-', ' '))"
    )
}

fn search_tokens(query: &str) -> Vec<String> {
    query
        .split(|c: char| c.is_whitespace() || c == '.' || c == '_' || c == '-')
        .filter(|token| !token.is_empty())
        .map(|token| token.to_lowercase())
        .collect()
}

#[tauri::command]
fn search_catalogue(
    app: tauri::AppHandle,
    persistent_identifier: String,
    query: String,
) -> Result<Vec<CatalogueEntry>, String> {
    let tokens = search_tokens(query.trim());
    if tokens.is_empty() {
        return Ok(Vec::new());
    }

    let connection = open_database(&database_path(&app)?)?;

    let name_expr = normalised_search_expression("name");
    let path_expr = normalised_search_expression("relative_path");

    let conditions: Vec<String> = tokens
        .iter()
        .enumerate()
        .map(|(index, _)| {
            let parameter = index + 2;
            format!(
                "({name_expr} LIKE '%' || ?{parameter} || '%' COLLATE NOCASE \
                  OR {path_expr} LIKE '%' || ?{parameter} || '%' COLLATE NOCASE)"
            )
        })
        .collect();

    let sql = format!(
        "SELECT relative_path, name, is_directory, size_bytes, modified_at
         FROM files
         WHERE drive_id = ?1
           AND {}
         ORDER BY is_directory DESC, lower(name), relative_path
         LIMIT 200",
        conditions.join(" AND ")
    );

    let mut values: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(tokens.len() + 1);
    values.push(&persistent_identifier);
    for token in &tokens {
        values.push(token);
    }

    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| format!("Unable to search catalogue: {error}"))?;

    let rows = statement
        .query_map(values.as_slice(), |row| {
            Ok(CatalogueEntry {
                relative_path: row.get(0)?,
                name: row.get(1)?,
                is_directory: row.get::<_, i64>(2)? != 0,
                size_bytes: row.get(3)?,
                modified_at: row.get(4)?,
            })
        })
        .map_err(|error| format!("Unable to read search results: {error}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Unable to read search result rows: {error}"))
}

#[tauri::command]
fn search_all_catalogues(
    app: tauri::AppHandle,
    query: String,
) -> Result<Vec<LibrarySearchResult>, String> {
    let tokens = search_tokens(query.trim());
    if tokens.is_empty() {
        return Ok(Vec::new());
    }

    let connection = open_database(&database_path(&app)?)?;

    let name_expr = normalised_search_expression("f.name");
    let path_expr = normalised_search_expression("f.relative_path");

    let conditions: Vec<String> = tokens
        .iter()
        .enumerate()
        .map(|(index, _)| {
            let parameter = index + 1;
            format!(
                "({name_expr} LIKE '%' || ?{parameter} || '%' COLLATE NOCASE \
                  OR {path_expr} LIKE '%' || ?{parameter} || '%' COLLATE NOCASE)"
            )
        })
        .collect();

    let sql = format!(
        "SELECT f.drive_id, d.name, f.relative_path, f.name,
                f.is_directory, f.size_bytes, f.modified_at
         FROM files f
         JOIN drives d ON d.persistent_identifier = f.drive_id
         WHERE {}
         ORDER BY f.is_directory DESC, lower(f.name), lower(d.name), f.relative_path
         LIMIT 200",
        conditions.join(" AND ")
    );

    let values: Vec<&dyn rusqlite::ToSql> =
        tokens.iter().map(|token| token as &dyn rusqlite::ToSql).collect();

    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| format!("Unable to search library: {error}"))?;

    let rows = statement
        .query_map(values.as_slice(), |row| {
            Ok(LibrarySearchResult {
                drive_id: row.get(0)?,
                drive_name: row.get(1)?,
                relative_path: row.get(2)?,
                name: row.get(3)?,
                is_directory: row.get::<_, i64>(4)? != 0,
                size_bytes: row.get(5)?,
                modified_at: row.get(6)?,
            })
        })
        .map_err(|error| format!("Unable to read library search results: {error}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Unable to read library search result rows: {error}"))
}

#[tauri::command]
fn probable_duplicates(
    app: tauri::AppHandle,
) -> Result<Vec<DuplicateGroup>, String> {
    let connection = open_database(&database_path(&app)?)?;

    let mut groups_statement = connection
        .prepare(
            "SELECT MIN(f.name) AS display_name,
                    f.size_bytes,
                    COUNT(*) AS copies
             FROM files f
             WHERE f.is_directory = 0
               AND f.size_bytes IS NOT NULL
               AND f.size_bytes > 0
             GROUP BY lower(f.name), f.size_bytes
             HAVING COUNT(*) > 1
             ORDER BY (f.size_bytes * (COUNT(*) - 1)) DESC,
                      f.size_bytes DESC,
                      lower(MIN(f.name))
             LIMIT 100"
        )
        .map_err(|error| format!("Unable to query probable duplicates: {error}"))?;

    let group_rows = groups_statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|error| format!("Unable to read probable duplicate groups: {error}"))?;

    let groups = group_rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Unable to read probable duplicate group rows: {error}"))?;

    let mut file_statement = connection
        .prepare(
            "SELECT f.drive_id,
                    d.name,
                    f.relative_path,
                    f.name,
                    f.size_bytes,
                    f.modified_at
             FROM files f
             JOIN drives d ON d.persistent_identifier = f.drive_id
             WHERE f.is_directory = 0
               AND lower(f.name) = lower(?1)
               AND f.size_bytes = ?2
             ORDER BY lower(d.name), lower(f.relative_path)"
        )
        .map_err(|error| format!("Unable to prepare probable duplicate files query: {error}"))?;

    let mut results = Vec::with_capacity(groups.len());

    for (name, size_bytes, copies) in groups {
        let file_rows = file_statement
            .query_map(params![&name, size_bytes], |row| {
                Ok(DuplicateFile {
                    drive_id: row.get(0)?,
                    drive_name: row.get(1)?,
                    relative_path: row.get(2)?,
                    name: row.get(3)?,
                    size_bytes: row.get(4)?,
                    modified_at: row.get(5)?,
                })
            })
            .map_err(|error| format!("Unable to read probable duplicate files: {error}"))?;

        let files = file_rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("Unable to read probable duplicate file rows: {error}"))?;

        results.push(DuplicateGroup {
            name,
            size_bytes,
            copies,
            potential_wasted_bytes: size_bytes.saturating_mul(copies.saturating_sub(1)),
            files,
        });
    }

    Ok(results)
}

#[tauri::command]
fn largest_files(
    app: tauri::AppHandle,
) -> Result<Vec<LargestFile>, String> {
    let connection = open_database(&database_path(&app)?)?;

    let mut statement = connection
        .prepare(
            "SELECT f.drive_id,
                    d.name,
                    f.relative_path,
                    f.name,
                    f.size_bytes,
                    f.modified_at
             FROM files f
             JOIN drives d ON d.persistent_identifier = f.drive_id
             WHERE f.is_directory = 0
               AND f.size_bytes IS NOT NULL
             ORDER BY f.size_bytes DESC, lower(f.name), lower(d.name)
             LIMIT 100"
        )
        .map_err(|error| format!("Unable to query largest files: {error}"))?;

    let rows = statement
        .query_map([], |row| {
            Ok(LargestFile {
                drive_id: row.get(0)?,
                drive_name: row.get(1)?,
                relative_path: row.get(2)?,
                name: row.get(3)?,
                size_bytes: row.get(4)?,
                modified_at: row.get(5)?,
            })
        })
        .map_err(|error| format!("Unable to read largest files: {error}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Unable to read largest file rows: {error}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            list_external_drives,
            list_catalogued_drives,
            scan_drive,
            cancel_scan,
            list_catalogue_entries,
            search_catalogue,
            search_all_catalogues,
            largest_files,
            probable_duplicates
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
