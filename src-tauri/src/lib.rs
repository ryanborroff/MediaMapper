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

fn emit_scan_progress(
    app: &tauri::AppHandle,
    drive_id: &str,
    counters: &(i64, i64, i64, i64),
    current_path: &str,
) {
    let _ = app.emit(
        "scan-progress",
        ScanProgress {
            persistent_identifier: drive_id.to_owned(),
            file_count: counters.0,
            directory_count: counters.1,
            catalogued_bytes: counters.2,
            skipped_count: counters.3,
            current_path: current_path.to_owned(),
        },
    );
}

static CANCELLED_SCANS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
const SCAN_CANCELLED: &str = "__MEDIA_MAPPER_SCAN_CANCELLED__";

fn cancelled_scans() -> &'static Mutex<HashSet<String>> {
    CANCELLED_SCANS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn scan_is_cancelled(drive_id: &str) -> bool {
    cancelled_scans()
        .lock()
        .map(|scans| scans.contains(drive_id))
        .unwrap_or(false)
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Location {
    id: String,
    kind: String,
    display_name: String,
    user_label: Option<String>,
    drive_id: Option<String>,
    local_path: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlannedMove {
    id: i64,
    source_drive_id: String,
    source_drive_name: String,
    source_relative_path: String,
    source_name: String,
    source_size_bytes: Option<i64>,
    source_is_directory: bool,
    destination_location_id: String,
    destination_location_name: String,
    destination_relative_path: String,
    created_at: i64,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PlannedFolderEntry {
    move_id: i64,
    source_drive_id: String,
    source_drive_name: String,
    source_relative_path: String,
    name: String,
    is_directory: bool,
    size_bytes: Option<i64>,
    destination_relative_path: String,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn system_time_unix(value: Result<SystemTime, std::io::Error>) -> Option<i64> {
    value
        .ok()?
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

// How long a connection waits for another connection's write lock before
// reporting `database is locked`. This absorbs brief contention between
// commands. It does not make writes succeed during a long scan, which holds
// its write transaction for the whole traversal. Commands still run on the
// main thread, so a longer wait would freeze the window for that long.
const DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(2);

// Opens a connection and makes sure the schema is current.
//
// Every command calls this, including read-only ones, so it must not write
// during normal use. The schema statements below only write when a table,
// index or column is missing; once the schema exists they need no write lock.
// Routine location synchronisation lives in `sync_drive_locations`.
fn open_database(path: &Path) -> Result<Connection, String> {
    let mut connection = Connection::open(path)
        .map_err(|error| format!("Unable to open catalogue database: {error}"))?;

    connection
        .busy_timeout(DATABASE_BUSY_TIMEOUT)
        .map_err(|error| format!("Unable to configure catalogue database: {error}"))?;

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
            .execute(
                "ALTER TABLE drives ADD COLUMN file_count INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|error| format!("Unable to add drive file count: {error}"))?;
        added_summary_columns = true;
    }
    if !existing_columns.contains("directory_count") {
        connection
            .execute(
                "ALTER TABLE drives ADD COLUMN directory_count INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|error| format!("Unable to add drive directory count: {error}"))?;
        added_summary_columns = true;
    }
    if !existing_columns.contains("catalogued_bytes") {
        connection
            .execute(
                "ALTER TABLE drives ADD COLUMN catalogued_bytes INTEGER NOT NULL DEFAULT 0",
                [],
            )
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
            .execute(
                "ALTER TABLE files ADD COLUMN parent_path TEXT NOT NULL DEFAULT ''",
                [],
            )
            .map_err(|error| format!("Unable to add catalogue parent paths: {error}"))?;

        let existing_paths = {
            let mut statement = connection
                .prepare("SELECT id, relative_path FROM files")
                .map_err(|error| format!("Unable to read catalogue paths: {error}"))?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
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

            -- A location is somewhere Media Mapper can plan files to live.
            --
            -- external_drive locations point back to the existing catalogue
            -- drive identity. local_folder locations will later represent
            -- folders explicitly chosen by the user on this computer.
            CREATE TABLE IF NOT EXISTS locations (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL CHECK(kind IN ('external_drive', 'local_folder')),
                display_name TEXT NOT NULL,
                user_label TEXT,
                drive_id TEXT,
                local_path TEXT,
                created_at INTEGER NOT NULL,
                FOREIGN KEY(drive_id) REFERENCES drives(persistent_identifier) ON DELETE CASCADE,
                CHECK(
                    (kind = 'external_drive' AND drive_id IS NOT NULL AND local_path IS NULL)
                    OR
                    (kind = 'local_folder' AND drive_id IS NULL AND local_path IS NOT NULL)
                )
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_locations_drive
                ON locations(drive_id)
                WHERE drive_id IS NOT NULL;

            CREATE UNIQUE INDEX IF NOT EXISTS idx_locations_local_path
                ON locations(local_path)
                WHERE local_path IS NOT NULL;
            ",
        )
        .map_err(|error| format!("Unable to initialise location schema: {error}"))?;

    let location_columns = {
        let mut statement = connection
            .prepare("PRAGMA table_info(locations)")
            .map_err(|error| format!("Unable to inspect location schema: {error}"))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| format!("Unable to inspect location columns: {error}"))?;
        rows.collect::<Result<HashSet<_>, _>>()
            .map_err(|error| format!("Unable to read location columns: {error}"))?
    };

    if !location_columns.contains("user_label") {
        connection
            .execute("ALTER TABLE locations ADD COLUMN user_label TEXT", [])
            .map_err(|error| format!("Unable to add drive labels: {error}"))?;
    }

    // Planned destinations use Media Mapper locations rather than assuming
    // every destination is an external drive.
    let planned_move_table_exists: bool = connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1
                FROM sqlite_master
                WHERE type = 'table' AND name = 'planned_moves'
            )",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("Unable to inspect planned move table: {error}"))?;

    if !planned_move_table_exists {
        connection
            .execute_batch(
                "CREATE TABLE planned_moves (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    source_drive_id TEXT NOT NULL,
                    source_relative_path TEXT NOT NULL,
                    destination_location_id TEXT NOT NULL,
                    destination_relative_path TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    UNIQUE(source_drive_id, source_relative_path),
                    FOREIGN KEY(source_drive_id)
                        REFERENCES drives(persistent_identifier)
                        ON DELETE CASCADE,
                    FOREIGN KEY(destination_location_id)
                        REFERENCES locations(id)
                        ON DELETE CASCADE
                );

                CREATE INDEX idx_planned_moves_destination
                    ON planned_moves(
                        destination_location_id,
                        destination_relative_path
                    );",
            )
            .map_err(|error| format!("Unable to create planned move schema: {error}"))?;
    } else {
        let planned_move_columns = {
            let mut statement = connection
                .prepare("PRAGMA table_info(planned_moves)")
                .map_err(|error| format!("Unable to inspect planned move schema: {error}"))?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(1))
                .map_err(|error| format!("Unable to inspect planned move columns: {error}"))?;
            rows.collect::<Result<HashSet<_>, _>>()
                .map_err(|error| format!("Unable to read planned move columns: {error}"))?
        };

        if planned_move_columns.contains("destination_drive_id") {
            let transaction = connection
                .transaction()
                .map_err(|error| format!("Unable to start planned move migration: {error}"))?;

            // The migrated rows reference `drive:` locations through a foreign
            // key, so those locations must exist before the rows are copied.
            sync_drive_locations(&transaction)?;

            transaction
                .execute_batch(
                    "CREATE TABLE planned_moves_new (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        source_drive_id TEXT NOT NULL,
                        source_relative_path TEXT NOT NULL,
                        destination_location_id TEXT NOT NULL,
                        destination_relative_path TEXT NOT NULL,
                        created_at INTEGER NOT NULL,
                        UNIQUE(source_drive_id, source_relative_path),
                        FOREIGN KEY(source_drive_id)
                            REFERENCES drives(persistent_identifier)
                            ON DELETE CASCADE,
                        FOREIGN KEY(destination_location_id)
                            REFERENCES locations(id)
                            ON DELETE CASCADE
                    );

                    INSERT INTO planned_moves_new (
                        id,
                        source_drive_id,
                        source_relative_path,
                        destination_location_id,
                        destination_relative_path,
                        created_at
                    )
                    SELECT
                        id,
                        source_drive_id,
                        source_relative_path,
                        COALESCE(
                            NULLIF(destination_location_id, ''),
                            'drive:' || destination_drive_id
                        ),
                        destination_relative_path,
                        created_at
                    FROM planned_moves;

                    DROP TABLE planned_moves;

                    ALTER TABLE planned_moves_new RENAME TO planned_moves;

                    CREATE INDEX idx_planned_moves_destination
                        ON planned_moves(
                            destination_location_id,
                            destination_relative_path
                        );",
                )
                .map_err(|error| {
                    format!("Unable to migrate planned moves to locations: {error}")
                })?;

            transaction
                .commit()
                .map_err(|error| format!("Unable to commit planned move migration: {error}"))?;
        }
    }

    Ok(connection)
}

// Every catalogued external drive is also a Media Mapper location.
//
// The `drive:` prefix keeps the location namespace separate from future
// local-folder identifiers while preserving the drive UUID as its stable
// underlying identity. Existing locations keep their id and user label; only
// the display name follows the drive's current volume name.
//
// This writes, so it runs only where changes are expected: at startup, inside
// the scan transaction, and before the legacy planned-move migration.
fn sync_drive_locations(connection: &Connection) -> Result<(), String> {
    connection
        .execute(
            "INSERT OR IGNORE INTO locations (
                id,
                kind,
                display_name,
                drive_id,
                local_path,
                created_at
            )
            SELECT
                'drive:' || persistent_identifier,
                'external_drive',
                name,
                persistent_identifier,
                NULL,
                ?1
            FROM drives",
            params![now_unix()],
        )
        .map_err(|error| format!("Unable to create drive locations: {error}"))?;

    connection
        .execute(
            "UPDATE locations
             SET display_name = (
                 SELECT drives.name
                 FROM drives
                 WHERE drives.persistent_identifier = locations.drive_id
             )
             WHERE kind = 'external_drive'
               AND drive_id IS NOT NULL",
            [],
        )
        .map_err(|error| format!("Unable to update drive locations: {error}"))?;

    Ok(())
}

// Runs once at startup: brings the schema up to date and backfills drive
// locations for catalogues created before locations existed.
fn initialise_database(app: &tauri::AppHandle) -> Result<(), String> {
    let connection = open_database(&database_path(app)?)?;
    sync_drive_locations(&connection)
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
            let digits: String = remainder
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if !digits.is_empty() {
                return digits.parse().ok();
            }
        }
        value
            .split_whitespace()
            .next()?
            .replace(',', "")
            .parse()
            .ok()
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

        // Create or rename this drive's location in the same transaction, so a
        // cancelled or failed scan leaves locations exactly as they were.
        sync_drive_locations(&transaction)?;

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
    format!("lower(replace(replace(replace({column}, '.', ' '), '_', ' '), '-', ' '))")
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

    let values: Vec<&dyn rusqlite::ToSql> = tokens
        .iter()
        .map(|token| token as &dyn rusqlite::ToSql)
        .collect();

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
fn probable_duplicates(app: tauri::AppHandle) -> Result<Vec<DuplicateGroup>, String> {
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
             LIMIT 100",
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
             ORDER BY lower(d.name), lower(f.relative_path)",
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
fn largest_files(app: tauri::AppHandle) -> Result<Vec<LargestFile>, String> {
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
             LIMIT 100",
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

fn validate_catalogue_relative_path(path: &str) -> Result<(), String> {
    let candidate = Path::new(path);
    if path.trim().is_empty()
        || candidate.is_absolute()
        || candidate.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(
            "Planned paths must be non-empty relative paths without '..' components.".to_string(),
        );
    }
    Ok(())
}

#[tauri::command]
fn list_locations(app: tauri::AppHandle) -> Result<Vec<Location>, String> {
    let connection = open_database(&database_path(&app)?)?;
    let mut statement = connection
        .prepare(
            "SELECT id, kind, display_name, user_label, drive_id, local_path
             FROM locations
             ORDER BY CASE kind WHEN 'external_drive' THEN 0 ELSE 1 END,
                      lower(COALESCE(NULLIF(user_label, ''), display_name))",
        )
        .map_err(|error| format!("Unable to query locations: {error}"))?;

    let rows = statement
        .query_map([], |row| {
            Ok(Location {
                id: row.get(0)?,
                kind: row.get(1)?,
                display_name: row.get(2)?,
                user_label: row.get(3)?,
                drive_id: row.get(4)?,
                local_path: row.get(5)?,
            })
        })
        .map_err(|error| format!("Unable to read locations: {error}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Unable to read location rows: {error}"))
}

#[tauri::command]
fn set_drive_label(
    app: tauri::AppHandle,
    persistent_identifier: String,
    label: String,
) -> Result<(), String> {
    let trimmed = label.trim();

    if trimmed.chars().count() > 40 {
        return Err("Drive labels can be up to 40 characters.".to_string());
    }

    let connection = open_database(&database_path(&app)?)?;
    let changed = connection
        .execute(
            "UPDATE locations
             SET user_label = CASE WHEN ?1 = '' THEN NULL ELSE ?1 END
             WHERE kind = 'external_drive'
               AND drive_id = ?2",
            params![trimmed, persistent_identifier],
        )
        .map_err(|error| format!("Unable to save drive label: {error}"))?;

    if changed == 0 {
        return Err("That drive is not in the Media Mapper catalogue.".to_string());
    }

    Ok(())
}

#[tauri::command]
fn add_local_folder_location(app: tauri::AppHandle, path: String) -> Result<Location, String> {
    let candidate = PathBuf::from(path.trim());

    if !candidate.is_absolute() {
        return Err("The selected folder must have an absolute path.".to_string());
    }

    let metadata = fs::metadata(&candidate)
        .map_err(|error| format!("Unable to inspect selected folder: {error}"))?;

    if !metadata.is_dir() {
        return Err("The selected location is not a folder.".to_string());
    }

    let canonical = fs::canonicalize(&candidate)
        .map_err(|error| format!("Unable to resolve selected folder: {error}"))?;
    let local_path = canonical.to_string_lossy().into_owned();

    let display_name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("This Mac")
        .to_owned();

    // The path itself is the stable identity material. It is stored separately
    // from the display name so renaming UI labels later will not affect plans.
    let id = format!("local:{local_path}");

    let connection = open_database(&database_path(&app)?)?;
    connection
        .execute(
            "INSERT OR IGNORE INTO locations
                (id, kind, display_name, drive_id, local_path, created_at)
             VALUES (?1, 'local_folder', ?2, NULL, ?3, ?4)",
            params![id, display_name, local_path, now_unix()],
        )
        .map_err(|error| format!("Unable to save local folder location: {error}"))?;

    connection
        .query_row(
            "SELECT id, kind, display_name, user_label, drive_id, local_path
             FROM locations
             WHERE local_path = ?1",
            params![local_path],
            |row| {
                Ok(Location {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    display_name: row.get(2)?,
                    user_label: row.get(3)?,
                    drive_id: row.get(4)?,
                    local_path: row.get(5)?,
                })
            },
        )
        .map_err(|error| format!("Unable to read local folder location: {error}"))
}

#[tauri::command]
fn create_planned_move(
    app: tauri::AppHandle,
    source_drive_id: String,
    source_relative_path: String,
    destination_location_id: String,
    destination_relative_path: String,
) -> Result<i64, String> {
    validate_catalogue_relative_path(&source_relative_path)?;
    validate_catalogue_relative_path(&destination_relative_path)?;

    let connection = open_database(&database_path(&app)?)?;

    let (destination_kind, destination_drive_id): (String, Option<String>) = connection
        .query_row(
            "SELECT kind, drive_id FROM locations WHERE id = ?1",
            params![destination_location_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| "The destination location is not available in Media Mapper.".to_string())?;

    if destination_drive_id.as_deref() == Some(source_drive_id.as_str())
        && source_relative_path == destination_relative_path
    {
        return Err(
            "The planned destination is the same as the current catalogue location.".to_string(),
        );
    }
    let source_is_directory = match connection.query_row(
        "SELECT is_directory
         FROM files
         WHERE drive_id = ?1 AND relative_path = ?2",
        params![source_drive_id, source_relative_path],
        |row| row.get::<_, i64>(0),
    ) {
        Ok(value) => value != 0,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err("The source item is not present in the catalogue.".to_string());
        }
        Err(error) => {
            return Err(format!("Unable to validate planned move source: {error}"));
        }
    };

    if source_is_directory
        && destination_drive_id.as_deref() == Some(source_drive_id.as_str())
        && destination_relative_path.starts_with(&(source_relative_path.clone() + "/"))
    {
        return Err("A folder cannot be planned inside itself.".to_string());
    }

    // Planning must not silently target a catalogue location that is already
    // occupied. The catalogue is a snapshot, so this is an early safety check;
    // execution will re-check the live destination before any future copy.
    if destination_kind == "external_drive" {
        let destination_drive_id = destination_drive_id
            .as_deref()
            .ok_or_else(|| "External drive location has no drive identity.".to_string())?;

        let destination_occupied: bool = connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1
                    FROM files
                    WHERE drive_id = ?1 AND relative_path = ?2
                )",
                params![destination_drive_id, destination_relative_path],
                |row| row.get(0),
            )
            .map_err(|error| format!("Unable to check planned destination: {error}"))?;

        if destination_occupied {
            return Err(
                "That destination already exists in the destination drive catalogue.".to_string(),
            );
        }
    }

    // Two source files must never silently claim the same planned destination.
    // Exclude this source so an existing plan can still be edited/replaced.
    let destination_already_planned: bool = connection
        .query_row(
            "SELECT EXISTS(
            SELECT 1
            FROM planned_moves
            WHERE destination_location_id = ?1
              AND destination_relative_path = ?2
              AND NOT (
                  source_drive_id = ?3
                  AND source_relative_path = ?4
              )
        )",
            params![
                destination_location_id,
                destination_relative_path,
                source_drive_id,
                source_relative_path
            ],
            |row| row.get(0),
        )
        .map_err(|error| format!("Unable to check planned destination conflicts: {error}"))?;

    if destination_already_planned {
        return Err("Another planned move already uses that destination.".to_string());
    }

    connection
        .execute(
            "INSERT INTO planned_moves (
                source_drive_id,
                source_relative_path,
                destination_location_id,
                destination_relative_path,
                created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(source_drive_id, source_relative_path) DO UPDATE SET
                destination_location_id = excluded.destination_location_id,
                destination_relative_path = excluded.destination_relative_path,
                created_at = excluded.created_at",
            params![
                source_drive_id,
                source_relative_path,
                destination_location_id,
                destination_relative_path,
                now_unix()
            ],
        )
        .map_err(|error| format!("Unable to save planned move: {error}"))?;

    connection
        .query_row(
            "SELECT id FROM planned_moves WHERE source_drive_id = ?1 AND source_relative_path = ?2",
            params![source_drive_id, source_relative_path],
            |row| row.get(0),
        )
        .map_err(|error| format!("Unable to read planned move id: {error}"))
}

#[tauri::command]
fn list_planned_moves(app: tauri::AppHandle) -> Result<Vec<PlannedMove>, String> {
    let connection = open_database(&database_path(&app)?)?;
    let mut statement = connection
        .prepare(
            "SELECT p.id,
                p.source_drive_id,
                sd.name,
                p.source_relative_path,
                sf.name,
                sf.size_bytes,
                COALESCE(sf.is_directory, 0),
                dl.id,
                COALESCE(NULLIF(dl.user_label, ''), dl.display_name),
                p.destination_relative_path,
                p.created_at
         FROM planned_moves p
         JOIN drives sd ON sd.persistent_identifier = p.source_drive_id
         JOIN locations dl ON dl.id = p.destination_location_id
         LEFT JOIN files sf
           ON sf.drive_id = p.source_drive_id
          AND sf.relative_path = p.source_relative_path
         ORDER BY p.created_at DESC",
        )
        .map_err(|error| format!("Unable to query planned moves: {error}"))?;

    let rows = statement
        .query_map([], |row| {
            let source_relative_path: String = row.get(3)?;
            let fallback_name = Path::new(&source_relative_path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&source_relative_path)
                .to_owned();
            Ok(PlannedMove {
                id: row.get(0)?,
                source_drive_id: row.get(1)?,
                source_drive_name: row.get(2)?,
                source_relative_path,
                source_name: row.get::<_, Option<String>>(4)?.unwrap_or(fallback_name),
                source_size_bytes: row.get(5)?,
                source_is_directory: row.get::<_, i64>(6)? != 0,
                destination_location_id: row.get(7)?,
                destination_location_name: row.get(8)?,
                destination_relative_path: row.get(9)?,
                created_at: row.get(10)?,
            })
        })
        .map_err(|error| format!("Unable to read planned moves: {error}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Unable to read planned move rows: {error}"))
}

#[tauri::command]
fn list_planned_folder_entries(
    app: tauri::AppHandle,
    destination_location_id: String,
    parent_path: String,
) -> Result<Vec<PlannedFolderEntry>, String> {
    if !parent_path.is_empty() {
        validate_catalogue_relative_path(&parent_path)?;
    }

    let connection = open_database(&database_path(&app)?)?;

    let roots = {
        let mut statement = connection
            .prepare(
                "SELECT
                    p.id,
                    p.source_drive_id,
                    sd.name,
                    p.source_relative_path,
                    sf.name,
                    COALESCE(sf.is_directory, 0),
                    sf.size_bytes,
                    p.destination_relative_path
                 FROM planned_moves p
                 JOIN drives sd
                   ON sd.persistent_identifier = p.source_drive_id
                 LEFT JOIN files sf
                   ON sf.drive_id = p.source_drive_id
                  AND sf.relative_path = p.source_relative_path
                 WHERE p.destination_location_id = ?1
                 ORDER BY lower(p.destination_relative_path)",
            )
            .map_err(|error| format!("Unable to query planned folder: {error}"))?;

        let rows = statement
            .query_map(params![destination_location_id], |row| {
                let source_relative_path: String = row.get(3)?;
                let stored_name: Option<String> = row.get(4)?;

                let name = stored_name.unwrap_or_else(|| {
                    Path::new(&source_relative_path)
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or(&source_relative_path)
                        .to_owned()
                });

                Ok(PlannedFolderEntry {
                    move_id: row.get(0)?,
                    source_drive_id: row.get(1)?,
                    source_drive_name: row.get(2)?,
                    source_relative_path,
                    name,
                    is_directory: row.get::<_, i64>(5)? != 0,
                    size_bytes: row.get(6)?,
                    destination_relative_path: row.get(7)?,
                })
            })
            .map_err(|error| format!("Unable to read planned folder: {error}"))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("Unable to read planned folder rows: {error}"))?
    };

    let mut entries = Vec::new();

    for root in roots {
        let destination_parent = Path::new(&root.destination_relative_path)
            .parent()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .replace('\\', "/");

        // Show the planned item itself in its future parent folder.
        if destination_parent == parent_path {
            entries.push(root.clone());
        }

        if !root.is_directory {
            continue;
        }

        // If the user is browsing inside a planned directory at its future
        // location, map that virtual path back to the source catalogue.
        let source_parent = if parent_path == root.destination_relative_path {
            Some(root.source_relative_path.clone())
        } else {
            let prefix = format!("{}/", root.destination_relative_path);

            parent_path.strip_prefix(&prefix).map(|suffix| {
                if suffix.is_empty() {
                    root.source_relative_path.clone()
                } else {
                    format!("{}/{}", root.source_relative_path, suffix)
                }
            })
        };

        let Some(source_parent) = source_parent else {
            continue;
        };

        let children = {
            let mut statement = connection
                .prepare(
                    "SELECT relative_path, name, is_directory, size_bytes
                     FROM files
                     WHERE drive_id = ?1 AND parent_path = ?2
                     ORDER BY lower(name)",
                )
                .map_err(|error| format!("Unable to query planned directory contents: {error}"))?;

            let rows = statement
                .query_map(params![root.source_drive_id, source_parent], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)? != 0,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                })
                .map_err(|error| format!("Unable to read planned directory contents: {error}"))?;

            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("Unable to read planned directory rows: {error}"))?
        };

        for (source_relative_path, name, is_directory, size_bytes) in children {
            let destination_relative_path = if parent_path.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", parent_path, name)
            };

            entries.push(PlannedFolderEntry {
                move_id: root.move_id,
                source_drive_id: root.source_drive_id.clone(),
                source_drive_name: root.source_drive_name.clone(),
                source_relative_path,
                name,
                is_directory,
                size_bytes,
                destination_relative_path,
            });
        }
    }

    entries.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()));

    Ok(entries)
}

#[tauri::command]
fn remove_planned_move(app: tauri::AppHandle, id: i64) -> Result<(), String> {
    let connection = open_database(&database_path(&app)?)?;
    let changed = connection
        .execute("DELETE FROM planned_moves WHERE id = ?1", params![id])
        .map_err(|error| format!("Unable to remove planned move: {error}"))?;
    if changed == 0 {
        return Err("That planned move no longer exists.".to_string());
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // A failure here must not stop the app from opening. Commands
            // open the database themselves and will report the same error.
            if let Err(error) = initialise_database(app.handle()) {
                eprintln!("Media Mapper database initialisation failed: {error}");
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_external_drives,
            list_catalogued_drives,
            scan_drive,
            cancel_scan,
            list_catalogue_entries,
            search_catalogue,
            search_all_catalogues,
            largest_files,
            probable_duplicates,
            list_locations,
            set_drive_label,
            add_local_folder_location,
            create_planned_move,
            list_planned_moves,
            list_planned_folder_entries,
            remove_planned_move
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDatabase(PathBuf);

    impl TestDatabase {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "media-mapper-{name}-{}-{}.sqlite3",
                std::process::id(),
                now_unix()
            ));
            let database = TestDatabase(path);
            database.remove_files();
            database
        }

        fn remove_files(&self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut file = self.0.clone().into_os_string();
                file.push(suffix);
                let _ = fs::remove_file(file);
            }
        }
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            self.remove_files();
        }
    }

    fn insert_drive(connection: &Connection, id: &str, name: &str) {
        connection
            .execute(
                "INSERT INTO drives (persistent_identifier, name, last_seen_at)
                 VALUES (?1, ?2, 0)
                 ON CONFLICT(persistent_identifier) DO UPDATE SET name = excluded.name",
                params![id, name],
            )
            .unwrap();
    }

    fn location_count(connection: &Connection) -> i64 {
        connection
            .query_row("SELECT COUNT(*) FROM locations", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn read_connection_opens_while_scan_holds_write_transaction() {
        let database = TestDatabase::new("read-during-scan");
        {
            let setup = open_database(&database.0).unwrap();
            insert_drive(&setup, "UUID-1", "Backup");
            sync_drive_locations(&setup).unwrap();
        }

        // Simulate a scan: one long write transaction replacing the catalogue.
        let mut writer = open_database(&database.0).unwrap();
        let scan = writer.transaction().unwrap();
        scan.execute("DELETE FROM files WHERE drive_id = 'UUID-1'", [])
            .unwrap();
        scan.execute(
            "INSERT INTO files (drive_id, relative_path, name, parent_path, is_directory)
             VALUES ('UUID-1', 'film.mp4', 'film.mp4', '', 0)",
            [],
        )
        .unwrap();

        // Opening a connection must not wait on the scan's write lock.
        let started = Instant::now();
        let reader = open_database(&database.0).expect("opening must not write");
        assert!(started.elapsed() < DATABASE_BUSY_TIMEOUT);

        assert_eq!(location_count(&reader), 1);
        let visible_files: i64 = reader
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(visible_files, 0, "uncommitted scan rows must stay invisible");

        // Location sync is a write, which is why it no longer runs on open.
        assert!(sync_drive_locations(&reader).is_err());

        scan.rollback().unwrap();
    }

    #[test]
    fn scan_location_sync_follows_the_scan_transaction() {
        let database = TestDatabase::new("scan-location-sync");
        let mut connection = open_database(&database.0).unwrap();

        // A cancelled first scan leaves no drive and no location behind.
        let cancelled = connection.transaction().unwrap();
        insert_drive(&cancelled, "UUID-1", "Backup");
        sync_drive_locations(&cancelled).unwrap();
        cancelled.rollback().unwrap();
        assert_eq!(location_count(&connection), 0);

        // A committed scan creates the location.
        let first = connection.transaction().unwrap();
        insert_drive(&first, "UUID-1", "Backup");
        sync_drive_locations(&first).unwrap();
        first.commit().unwrap();

        connection
            .execute(
                "UPDATE locations SET user_label = 'Mars' WHERE drive_id = 'UUID-1'",
                [],
            )
            .unwrap();

        // A rescan after a volume rename updates the display name only.
        let rescan = connection.transaction().unwrap();
        insert_drive(&rescan, "UUID-1", "Backup 2");
        sync_drive_locations(&rescan).unwrap();
        rescan.commit().unwrap();

        let (id, display_name, user_label): (String, String, Option<String>) = connection
            .query_row(
                "SELECT id, display_name, user_label FROM locations WHERE drive_id = 'UUID-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(id, "drive:UUID-1");
        assert_eq!(display_name, "Backup 2");
        assert_eq!(user_label.as_deref(), Some("Mars"));
        assert_eq!(location_count(&connection), 1);
    }
}
