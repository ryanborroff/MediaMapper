use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufReader, BufWriter, Read, Write},
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
    // Folders the last scan could not fully read. Their catalogued contents
    // are incomplete.
    unreadable_folder_count: i64,
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
    unreadable_folder_count: i64,
    // A few of the unreadable folders, for the scan summary. "" is the top
    // folder of the drive.
    unreadable_examples: Vec<String>,
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
    // A folder the last scan could not fully read.
    unreadable: bool,
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
    // A folder that exists only in the plan: some planned destination sits
    // inside it, but it is not in the catalogue. It has no single source move.
    is_new_folder: bool,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct PlanPreflightDestination {
    location_id: String,
    display_name: String,
    kind: String,
    move_count: i64,
    known_bytes: i64,
    unknown_size_count: i64,
    available_bytes: Option<i64>,
    projected_available_bytes: Option<i64>,
    capacity_sufficient: Option<bool>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct PlanPreflightIssue {
    code: String,
    message: String,
    move_id: Option<i64>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct PlanPreflight {
    move_count: i64,
    known_bytes: i64,
    unknown_size_count: i64,
    destinations: Vec<PlanPreflightDestination>,
    issues: Vec<PlanPreflightIssue>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct PlanLiveValidation {
    ready: bool,
    issues: Vec<PlanPreflightIssue>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct TransferRecord {
    id: i64,
    planned_move_id: Option<i64>,
    source_drive_id: String,
    source_relative_path: String,
    destination_location_id: String,
    destination_relative_path: String,
    total_bytes: Option<i64>,
    copied_bytes: i64,
    status: String,
    error_message: Option<String>,
    created_at: i64,
    started_at: Option<i64>,
    completed_at: Option<i64>,
}

fn read_transfer(row: &rusqlite::Row<'_>) -> rusqlite::Result<TransferRecord> {
    Ok(TransferRecord {
        id: row.get(0)?,
        planned_move_id: row.get(1)?,
        source_drive_id: row.get(2)?,
        source_relative_path: row.get(3)?,
        destination_location_id: row.get(4)?,
        destination_relative_path: row.get(5)?,
        total_bytes: row.get(6)?,
        copied_bytes: row.get(7)?,
        status: row.get(8)?,
        error_message: row.get(9)?,
        created_at: row.get(10)?,
        started_at: row.get(11)?,
        completed_at: row.get(12)?,
    })
}

fn create_transfer_record(
    connection: &Connection,
    planned_move_id: i64,
) -> Result<TransferRecord, String> {
    let planned: Option<(String, String, String, String, Option<i64>)> = connection
        .query_row(
            "SELECT
                p.source_drive_id,
                p.source_relative_path,
                p.destination_location_id,
                p.destination_relative_path,
                f.size_bytes
             FROM planned_moves p
             LEFT JOIN files f
               ON f.drive_id = p.source_drive_id
              AND f.relative_path = p.source_relative_path
             WHERE p.id = ?1",
            params![planned_move_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(|error| format!("Unable to read planned move for transfer: {error}"))?;

    let Some((
        source_drive_id,
        source_relative_path,
        destination_location_id,
        destination_relative_path,
        total_bytes,
    )) = planned
    else {
        return Err("The planned move no longer exists.".to_string());
    };

    let created_at = now_unix();

    connection
        .execute(
            "INSERT INTO transfers (
                planned_move_id,
                source_drive_id,
                source_relative_path,
                destination_location_id,
                destination_relative_path,
                total_bytes,
                copied_bytes,
                status,
                created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 'pending', ?7)",
            params![
                planned_move_id,
                source_drive_id,
                source_relative_path,
                destination_location_id,
                destination_relative_path,
                total_bytes,
                created_at
            ],
        )
        .map_err(|error| format!("Unable to create transfer record: {error}"))?;

    let id = connection.last_insert_rowid();

    connection
        .query_row(
            "SELECT
                id,
                planned_move_id,
                source_drive_id,
                source_relative_path,
                destination_location_id,
                destination_relative_path,
                total_bytes,
                copied_bytes,
                status,
                error_message,
                created_at,
                started_at,
                completed_at
             FROM transfers
             WHERE id = ?1",
            params![id],
            read_transfer,
        )
        .map_err(|error| format!("Unable to read created transfer: {error}"))
}

fn list_transfer_records(connection: &Connection) -> Result<Vec<TransferRecord>, String> {
    let mut statement = connection
        .prepare(
            "SELECT
                id,
                planned_move_id,
                source_drive_id,
                source_relative_path,
                destination_location_id,
                destination_relative_path,
                total_bytes,
                copied_bytes,
                status,
                error_message,
                created_at,
                started_at,
                completed_at
             FROM transfers
             ORDER BY created_at DESC, id DESC",
        )
        .map_err(|error| format!("Unable to query transfers: {error}"))?;

    let rows = statement
        .query_map([], read_transfer)
        .map_err(|error| format!("Unable to read transfers: {error}"))?;

    let transfers = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Unable to read transfer rows: {error}"))?;

    Ok(transfers)
}

fn files_are_identical(first: &Path, second: &Path) -> Result<bool, String> {
    let first_file = fs::File::open(first)
        .map_err(|error| format!("Unable to open source for verification: {error}"))?;
    let second_file = fs::File::open(second)
        .map_err(|error| format!("Unable to open copied file for verification: {error}"))?;

    if first_file
        .metadata()
        .map_err(|error| error.to_string())?
        .len()
        != second_file
            .metadata()
            .map_err(|error| error.to_string())?
            .len()
    {
        return Ok(false);
    }

    let mut first_reader = BufReader::new(first_file);
    let mut second_reader = BufReader::new(second_file);
    let mut first_buffer = [0_u8; 1024 * 1024];
    let mut second_buffer = [0_u8; 1024 * 1024];

    loop {
        let first_count = first_reader
            .read(&mut first_buffer)
            .map_err(|error| format!("Unable to verify source file: {error}"))?;
        let second_count = second_reader
            .read(&mut second_buffer)
            .map_err(|error| format!("Unable to verify copied file: {error}"))?;

        if first_count != second_count {
            return Ok(false);
        }

        if first_count == 0 {
            return Ok(true);
        }

        if first_buffer[..first_count] != second_buffer[..second_count] {
            return Ok(false);
        }
    }
}

fn copy_file_verified(source: &Path, destination: &Path) -> Result<u64, String> {
    if destination.exists() {
        return Err(format!(
            "Destination already exists: {}",
            destination.display()
        ));
    }

    let parent = destination
        .parent()
        .ok_or_else(|| "Destination has no parent folder.".to_string())?;

    fs::create_dir_all(parent)
        .map_err(|error| format!("Unable to create destination folder: {error}"))?;

    let file_name = destination
        .file_name()
        .ok_or_else(|| "Destination has no file name.".to_string())?
        .to_string_lossy();

    let temporary = parent.join(format!(
        ".mediamapper-{}-{}.partial",
        std::process::id(),
        file_name
    ));

    if temporary.exists() {
        fs::remove_file(&temporary)
            .map_err(|error| format!("Unable to remove stale temporary file: {error}"))?;
    }

    let result = (|| -> Result<u64, String> {
        let source_file = fs::File::open(source)
            .map_err(|error| format!("Unable to open source file: {error}"))?;
        let mut reader = BufReader::new(source_file);

        let temporary_file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("Unable to create temporary destination file: {error}"))?;
        let mut writer = BufWriter::new(temporary_file);

        let copied = std::io::copy(&mut reader, &mut writer)
            .map_err(|error| format!("Unable to copy file: {error}"))?;

        writer
            .flush()
            .map_err(|error| format!("Unable to flush copied file: {error}"))?;

        writer
            .get_ref()
            .sync_all()
            .map_err(|error| format!("Unable to sync copied file: {error}"))?;

        drop(writer);

        if !files_are_identical(source, &temporary)? {
            return Err("Copied file failed byte-for-byte verification.".to_string());
        }

        // Refuse a late collision that appeared while the copy was running.
        if destination.exists() {
            return Err(format!(
                "Destination appeared while copying: {}",
                destination.display()
            ));
        }

        fs::rename(&temporary, destination)
            .map_err(|error| format!("Unable to finalise copied file: {error}"))?;

        Ok(copied)
    })();

    if result.is_err() && temporary.exists() {
        let _ = fs::remove_file(&temporary);
    }

    result
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

// Databases whose schema this process has already brought up to date.
static MIGRATED_DATABASES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

// Opens a connection, bringing the schema up to date the first time each
// database is opened in this run of the app.
//
// Every command calls this, including read-only ones, so it must not write
// during normal use. Migrations only write when a table, index or column is
// missing. Routine location synchronisation lives in `sync_drive_locations`.
fn open_database(path: &Path) -> Result<Connection, String> {
    let mut connection = Connection::open(path)
        .map_err(|error| format!("Unable to open catalogue database: {error}"))?;

    connection
        .busy_timeout(DATABASE_BUSY_TIMEOUT)
        .map_err(|error| format!("Unable to configure catalogue database: {error}"))?;
    register_search_function(&connection)
        .map_err(|error| format!("Unable to configure catalogue search: {error}"))?;
    // Foreign keys are a per-connection setting, so every connection sets it.
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(|error| format!("Unable to configure catalogue database: {error}"))?;

    // The lock is held while migrating, so two commands opening the database
    // at the same moment cannot both run the same migration. A failed
    // migration is not recorded, so the next open tries again.
    let mut migrated = MIGRATED_DATABASES
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map_err(|_| "Unable to access catalogue migration state.".to_string())?;
    if !migrated.contains(path) {
        migrate_database(&mut connection)?;
        migrated.insert(path.to_path_buf());
    }

    Ok(connection)
}

// Creates any missing tables, indexes and columns, and upgrades data from
// older schema versions. Every step checks first, so it is safe to run on a
// database that is already current.
fn migrate_database(connection: &mut Connection) -> Result<(), String> {
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
    if !existing_columns.contains("unreadable_folder_count") {
        connection
            .execute(
                "ALTER TABLE drives ADD COLUMN unreadable_folder_count INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|error| format!("Unable to add drive unreadable folder count: {error}"))?;
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

    if !file_columns.contains("unreadable") {
        connection
            .execute(
                "ALTER TABLE files ADD COLUMN unreadable INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|error| format!("Unable to add unreadable folder marker: {error}"))?;
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

            // Tables created before locations existed only have
            // `destination_drive_id`. Referencing `destination_location_id`
            // there fails with "no such column", so only use it when present.
            let destination_location_expression =
                if planned_move_columns.contains("destination_location_id") {
                    "COALESCE(
                        NULLIF(destination_location_id, ''),
                        'drive:' || destination_drive_id
                    )"
                } else {
                    "'drive:' || destination_drive_id"
                };

            transaction
                .execute_batch(&format!(
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
                        {destination_location_expression},
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
                ))
                .map_err(|error| {
                    format!("Unable to migrate planned moves to locations: {error}")
                })?;

            transaction
                .commit()
                .map_err(|error| format!("Unable to commit planned move migration: {error}"))?;
        }
    }

    // Transfers are an execution record, not part of the plan itself.
    // Source and destination values are snapshotted so transfer history remains
    // meaningful even if the corresponding planned move is later changed or
    // removed.
    connection
        .execute_batch(
            "
            CREATE TABLE IF NOT EXISTS transfers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                planned_move_id INTEGER,
                source_drive_id TEXT NOT NULL,
                source_relative_path TEXT NOT NULL,
                destination_location_id TEXT NOT NULL,
                destination_relative_path TEXT NOT NULL,
                total_bytes INTEGER,
                copied_bytes INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL
                    CHECK(status IN (
                        'pending',
                        'copying',
                        'verifying',
                        'completed',
                        'failed'
                    )),
                error_message TEXT,
                created_at INTEGER NOT NULL,
                started_at INTEGER,
                completed_at INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_transfers_status
                ON transfers(status);

            CREATE INDEX IF NOT EXISTS idx_transfers_created
                ON transfers(created_at);
            ",
        )
        .map_err(|error| format!("Unable to initialise transfer schema: {error}"))?;

    Ok(())
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

// Parses `diskutil info` output into its `Key: Value` pairs.
fn parse_diskutil_info(text: &str) -> std::collections::HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect()
}

fn parse_diskutil_bytes(value: Option<&String>) -> Option<u64> {
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

// Returns (total, available) bytes for a volume. "Disk Size" is the size of
// the volume's partition, or of its container on APFS. APFS volumes share
// their container's free space and report it only as "Container Free Space";
// other filesystems report "Volume Free Space".
fn diskutil_capacity(
    info: &std::collections::HashMap<String, String>,
) -> (Option<u64>, Option<u64>) {
    (
        parse_diskutil_bytes(info.get("Disk Size")),
        parse_diskutil_bytes(
            info.get("Volume Free Space")
                .or_else(|| info.get("Container Free Space")),
        ),
    )
}

#[cfg(target_os = "macos")]
fn external_drives() -> Result<Vec<DriveInfo>, String> {
    use std::process::Command;

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

        let info = parse_diskutil_info(&String::from_utf8_lossy(&output.stdout));
        let (total_bytes, available_bytes) = diskutil_capacity(&info);
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
            total_bytes,
            available_bytes,
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

// Tauri runs non-async commands on the main thread, which also drives the
// window, so a slow query or `diskutil` call would freeze the interface.
// Commands that touch SQLite or the filesystem are async and hand their work
// to the blocking thread pool through this helper, as `scan_drive` does.
async fn run_blocking<T, F>(work: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|error| format!("Background task failed: {error}"))?
}

#[tauri::command]
async fn list_external_drives() -> Result<Vec<DriveInfo>, String> {
    run_blocking(external_drives).await
}

#[tauri::command]
async fn list_catalogued_drives(app: tauri::AppHandle) -> Result<Vec<CataloguedDrive>, String> {
    run_blocking(move || {
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
                    d.catalogued_bytes,
                    d.unreadable_folder_count
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
                    unreadable_folder_count: row.get(10)?,
                })
            })
            .map_err(|error| format!("Unable to read catalogue: {error}"))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("Unable to read catalogue rows: {error}"))
    })
    .await
}

// Folders that macOS and Windows create at the root of a volume for Spotlight
// indexing, the trash, filesystem event logs, temporary files and document
// versions. They are not user content, can hold thousands of entries
// (including deleted files in the trash), and are often unreadable, which
// would abort the scan. They are only skipped at the volume root, so a user
// folder with the same name deeper in the drive is still catalogued.
const VOLUME_SYSTEM_FOLDERS: &[&str] = &[
    ".Spotlight-V100",
    ".Trashes",
    ".fseventsd",
    ".TemporaryItems",
    ".DocumentRevisions-V100",
    "$RECYCLE.BIN",
    "System Volume Information",
];

fn is_volume_system_folder(name: &str) -> bool {
    // FAT and exFAT volumes are case-insensitive, and Windows versions have
    // written `$RECYCLE.BIN` and `$Recycle.Bin`.
    VOLUME_SYSTEM_FOLDERS
        .iter()
        .any(|system_folder| system_folder.eq_ignore_ascii_case(name))
}

// Walks the drive and writes every entry through `insert_statement`.
//
// A folder that cannot be read, or an entry whose details cannot be read, is
// skipped and its folder is added to `unreadable_folders` ("" for the top
// folder), so one unreadable folder does not stop the whole scan. The scan
// still fails if the top folder of the drive cannot be listed at all, since
// nothing could be catalogued.
fn scan_directory(
    root: &Path,
    insert_statement: &mut rusqlite::Statement<'_>,
    drive_id: &str,
    counters: &mut (i64, i64, i64, i64),
    unreadable_folders: &mut Vec<String>,
    report_progress: &dyn Fn(&(i64, i64, i64, i64), &str),
    last_emit_at: &mut Instant,
) -> Result<(), String> {
    let relative_to_root = |path: &Path| {
        path.strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    };
    // Folders are read one at a time, so a repeat is always the latest entry.
    let note_unreadable = |unreadable_folders: &mut Vec<String>, folder: String| {
        if unreadable_folders.last() != Some(&folder) {
            unreadable_folders.push(folder);
        }
    };

    // Keep directory traversal on the heap rather than the call stack. This
    // avoids stack overflow on drives with unusually deep folder structures.
    let mut directories = vec![root.to_path_buf()];

    while let Some(current) = directories.pop() {
        if scan_is_cancelled(drive_id) {
            return Err(SCAN_CANCELLED.to_string());
        }

        let entries = match fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(error) if current == root => {
                return Err(format!(
                    "Scan could not read the top folder of the drive: {error}. The previous catalogue has been kept unchanged."
                ));
            }
            Err(_) => {
                note_unreadable(unreadable_folders, relative_to_root(&current));
                continue;
            }
        };

        for entry_result in entries {
            if scan_is_cancelled(drive_id) {
                return Err(SCAN_CANCELLED.to_string());
            }

            let Ok(entry) = entry_result else {
                // The listing broke off part way; the rest of this folder is
                // unknown.
                note_unreadable(unreadable_folders, relative_to_root(&current));
                break;
            };

            let name = entry.file_name().to_string_lossy().into_owned();

            // Ignore macOS Finder metadata rather than cataloguing it as user content.
            // AppleDouble sidecars mirror real files as tiny `._*` entries; .DS_Store
            // stores Finder folder preferences. Neither belongs in Media Mapper's catalogue.
            if name.starts_with("._") || name == ".DS_Store" {
                continue;
            }

            if current == root && is_volume_system_folder(&name) {
                continue;
            }

            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                note_unreadable(unreadable_folders, relative_to_root(&current));
                continue;
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
                report_progress(counters, &relative);
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

        // A rescan replaces the previous snapshot atomically. Cancellation,
        // database errors and an unreadable top folder roll everything back,
        // keeping the last catalogue. Folders deeper in the drive that cannot
        // be read are skipped and marked, so the new catalogue says where it
        // is incomplete.
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
        let mut unreadable_folders = Vec::new();
        let mut last_emit_at = Instant::now();
        emit_scan_progress(&progress_app, &drive_id, &counters, "");
        if let Err(error) = scan_directory(
            &root,
            &mut insert_statement,
            &drive_id,
            &mut counters,
            &mut unreadable_folders,
            &|counters, current_path| {
                emit_scan_progress(&progress_app, &drive_id, counters, current_path)
            },
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

        {
            let mut mark_unreadable = transaction
                .prepare(
                    "UPDATE files SET unreadable = 1 WHERE drive_id = ?1 AND relative_path = ?2",
                )
                .map_err(|error| format!("Unable to mark unreadable folders: {error}"))?;
            for folder in unreadable_folders.iter().filter(|folder| !folder.is_empty()) {
                mark_unreadable
                    .execute(params![drive_id, folder])
                    .map_err(|error| format!("Unable to mark unreadable folders: {error}"))?;
            }
        }
        let unreadable_folder_count = unreadable_folders.len() as i64;

        transaction
            .execute(
                "
                UPDATE drives
                SET last_scanned_at = ?1,
                    file_count = ?2,
                    directory_count = ?3,
                    catalogued_bytes = ?4,
                    unreadable_folder_count = ?5
                WHERE persistent_identifier = ?6
                ",
                params![
                    scanned_at,
                    counters.0,
                    counters.1,
                    counters.2,
                    unreadable_folder_count,
                    drive_id
                ],
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
            unreadable_folder_count,
            unreadable_examples: unreadable_folders.into_iter().take(3).collect(),
        })
    })
    .await
    .map_err(|error| format!("Drive scan task failed: {error}"))?
}

#[tauri::command]
async fn list_catalogue_entries(
    app: tauri::AppHandle,
    persistent_identifier: String,
    parent_path: String,
) -> Result<Vec<CatalogueEntry>, String> {
    run_blocking(move || {
        let connection = open_database(&database_path(&app)?)?;

        let mut statement = connection
            .prepare(
                "
                SELECT relative_path, name, is_directory, size_bytes, modified_at, unreadable
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
                    unreadable: row.get::<_, i64>(5)? != 0,
                })
            })
            .map_err(|error| format!("Unable to read catalogue entries: {error}"))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("Unable to read catalogue entry rows: {error}"))
    })
    .await
}

// Folds text for search: Unicode NFC normalisation, full lowercasing, and
// `.`, `_` and `-` treated as spaces so `My.Film_2024` matches `my film 2024`.
//
// SQLite's own lower() only folds A-Z, so `ÉMILE` never matched `émile`, and
// macOS can store names decomposed (é as e plus a combining accent), which
// never matched a typed, composed é. Names and queries go through this same
// function, so both sides always agree.
fn search_fold(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;

    fn separator_to_space(c: char) -> char {
        match c {
            '.' | '_' | '-' => ' ',
            c => c,
        }
    }

    // Search runs this on every catalogued name and path, and most are plain
    // ASCII, which is already normalised. Skip the Unicode work for them.
    if text.is_ascii() {
        return text
            .chars()
            .map(|c| separator_to_space(c.to_ascii_lowercase()))
            .collect();
    }

    text.nfc()
        .flat_map(char::to_lowercase)
        .map(separator_to_space)
        .collect()
}

// Makes search_fold available to SQL as mm_search_fold on this connection.
fn register_search_function(connection: &Connection) -> rusqlite::Result<()> {
    use rusqlite::functions::FunctionFlags;

    connection.create_scalar_function(
        "mm_search_fold",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            // Borrow the value rather than copying it into a String first.
            let text = context
                .get_raw(0)
                .as_str_or_null()
                .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))?;
            Ok(text.map(search_fold))
        },
    )
}

fn normalised_search_expression(column: &str) -> String {
    format!("mm_search_fold({column})")
}

fn search_tokens(query: &str) -> Vec<String> {
    search_fold(query)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

#[tauri::command]
async fn search_catalogue(
    app: tauri::AppHandle,
    persistent_identifier: String,
    query: String,
) -> Result<Vec<CatalogueEntry>, String> {
    run_blocking(move || {
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
            "SELECT relative_path, name, is_directory, size_bytes, modified_at, unreadable
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
                    unreadable: row.get::<_, i64>(5)? != 0,
                })
            })
            .map_err(|error| format!("Unable to read search results: {error}"))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("Unable to read search result rows: {error}"))
    })
    .await
}

#[tauri::command]
async fn search_all_catalogues(
    app: tauri::AppHandle,
    query: String,
) -> Result<Vec<LibrarySearchResult>, String> {
    run_blocking(move || {
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
    })
    .await
}

// Groups files with the same name (ignoring case) and exact size, largest
// potential saving first, and lists every copy.
//
// This used to run one query per group, and each of those read the whole
// files table because lower(name) cannot use an index: 101 full scans. Here
// the top groups are found once, then matched against the files in a single
// pass. CROSS JOIN makes SQLite scan files once and look each row up in the
// small groups table, instead of scanning files once per group.
fn find_probable_duplicates(connection: &Connection) -> Result<Vec<DuplicateGroup>, String> {
    let mut statement = connection
        .prepare(
            "WITH duplicate_groups AS MATERIALIZED (
                SELECT lower(name) AS name_key,
                       size_bytes,
                       MIN(name) AS display_name,
                       COUNT(*) AS copies
                FROM files
                WHERE is_directory = 0
                  AND size_bytes IS NOT NULL
                  AND size_bytes > 0
                GROUP BY lower(name), size_bytes
                HAVING COUNT(*) > 1
                ORDER BY (size_bytes * (COUNT(*) - 1)) DESC,
                         size_bytes DESC,
                         lower(MIN(name))
                LIMIT 100
             )
             SELECT g.name_key,
                    g.size_bytes,
                    g.display_name,
                    g.copies,
                    f.drive_id,
                    d.name,
                    f.relative_path,
                    f.name,
                    f.modified_at
             FROM files f
             CROSS JOIN duplicate_groups g
             JOIN drives d ON d.persistent_identifier = f.drive_id
             WHERE f.is_directory = 0
               AND f.size_bytes = g.size_bytes
               AND lower(f.name) = g.name_key
             ORDER BY (g.size_bytes * (g.copies - 1)) DESC,
                      g.size_bytes DESC,
                      lower(g.display_name),
                      g.name_key,
                      lower(d.name),
                      lower(f.relative_path)",
        )
        .map_err(|error| format!("Unable to query probable duplicates: {error}"))?;

    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                DuplicateFile {
                    drive_id: row.get(4)?,
                    drive_name: row.get(5)?,
                    relative_path: row.get(6)?,
                    name: row.get(7)?,
                    size_bytes: row.get(1)?,
                    modified_at: row.get(8)?,
                },
            ))
        })
        .map_err(|error| format!("Unable to read probable duplicates: {error}"))?;

    // Rows arrive grouped and in display order, so each group is a run.
    let mut results: Vec<DuplicateGroup> = Vec::new();
    let mut current_key: Option<(String, i64)> = None;
    for row in rows {
        let (name_key, size_bytes, display_name, copies, file) =
            row.map_err(|error| format!("Unable to read probable duplicate rows: {error}"))?;
        let key = (name_key, size_bytes);
        if current_key.as_ref() != Some(&key) {
            results.push(DuplicateGroup {
                name: display_name,
                size_bytes,
                copies,
                potential_wasted_bytes: size_bytes.saturating_mul(copies.saturating_sub(1)),
                files: Vec::new(),
            });
            current_key = Some(key);
        }
        if let Some(group) = results.last_mut() {
            group.files.push(file);
        }
    }

    Ok(results)
}

#[tauri::command]
async fn probable_duplicates(app: tauri::AppHandle) -> Result<Vec<DuplicateGroup>, String> {
    run_blocking(move || {
        let connection = open_database(&database_path(&app)?)?;
        find_probable_duplicates(&connection)
    })
    .await
}

#[tauri::command]
async fn largest_files(app: tauri::AppHandle) -> Result<Vec<LargestFile>, String> {
    run_blocking(move || {
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
    })
    .await
}

// Catalogue paths are `/`-separated and relative to the volume root. Checks
// the raw segments rather than Path::components, which silently drops `.` in
// the middle of a path and merges repeated slashes, so `a/./b` or `a//b`
// would pass but never match a catalogue path.
fn validate_catalogue_relative_path(path: &str) -> Result<(), String> {
    if path.trim().is_empty()
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(
            "Planned paths must be relative paths without empty, '.' or '..' folders.".to_string(),
        );
    }
    Ok(())
}

#[tauri::command]
async fn list_locations(app: tauri::AppHandle) -> Result<Vec<Location>, String> {
    run_blocking(move || {
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
    })
    .await
}

#[tauri::command]
async fn set_drive_label(
    app: tauri::AppHandle,
    persistent_identifier: String,
    label: String,
) -> Result<(), String> {
    run_blocking(move || {
        let trimmed = label.trim();

        // Counted in UTF-16 units, as the label field's maxLength counts them,
        // so the app and the field agree on what fits.
        if trimmed.encode_utf16().count() > 40 {
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
    })
    .await
}

// Returns the connected external drive whose volume contains `path`, if any.
fn external_drive_containing<'a>(path: &Path, drives: &'a [DriveInfo]) -> Option<&'a DriveInfo> {
    drives
        .iter()
        .find(|drive| path.starts_with(Path::new(&drive.mount_point)))
}

#[tauri::command]
async fn add_local_folder_location(
    app: tauri::AppHandle,
    path: String,
) -> Result<Location, String> {
    run_blocking(move || {
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

        // A folder on an external drive must be planned through that drive's
        // own location, or the same place would have two identities and the
        // drive's catalogue checks would not apply to it.
        let drives = external_drives().unwrap_or_default();
        if let Some(drive) = external_drive_containing(&canonical, &drives) {
            let connection = open_database(&database_path(&app)?)?;
            let drive_name = drive
                .persistent_identifier
                .as_deref()
                .and_then(|drive_id| {
                    connection
                        .query_row(
                            "SELECT COALESCE(NULLIF(user_label, ''), display_name)
                             FROM locations
                             WHERE drive_id = ?1",
                            params![drive_id],
                            |row| row.get::<_, String>(0),
                        )
                        .ok()
                })
                .unwrap_or_else(|| drive.name.clone());
            return Err(format!(
                "That folder is on the external drive {drive_name}. Scan the drive if you \
                 haven't, then choose {drive_name} in Move to and enter the folder."
            ));
        }

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
    })
    .await
}

// Validates a planned move and saves it, replacing any existing plan for the
// same source. The checks and the write share one transaction, so two plans
// cannot claim the same destination between the check and the insert.
//
// Drives Media Mapper sees are almost always case-insensitive (APFS and HFS+
// by default, exFAT and FAT always), so destinations that differ only in case
// are treated as the same place.
fn plan_move(
    connection: &mut Connection,
    source_drive_id: &str,
    source_relative_path: &str,
    destination_location_id: &str,
    destination_relative_path: &str,
) -> Result<i64, String> {
    validate_catalogue_relative_path(source_relative_path)?;
    validate_catalogue_relative_path(destination_relative_path)?;

    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| format!("Unable to start planning: {error}"))?;

    let (destination_kind, destination_drive_id, destination_local_path): (
        String,
        Option<String>,
        Option<String>,
    ) = transaction
        .query_row(
            "SELECT kind, drive_id, local_path FROM locations WHERE id = ?1",
            params![destination_location_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|_| "The destination location is not available in Media Mapper.".to_string())?;

    let same_drive = destination_drive_id.as_deref() == Some(source_drive_id);

    if same_drive && source_relative_path == destination_relative_path {
        return Err(
            "The planned destination is the same as the current catalogue location.".to_string(),
        );
    }

    let source_is_directory = match transaction.query_row(
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
        && same_drive
        && destination_relative_path
            .to_lowercase()
            .starts_with(&format!("{}/", source_relative_path.to_lowercase()))
    {
        return Err("A folder cannot be planned inside itself.".to_string());
    }

    // Planning must not silently target a location that is already occupied.
    // The catalogue is a snapshot, so this is an early safety check; execution
    // will re-check the live destination before any future copy. The source
    // itself is excluded so a plan that only changes letter case is allowed.
    match destination_kind.as_str() {
        "external_drive" => {
            let destination_drive_id = destination_drive_id
                .as_deref()
                .ok_or_else(|| "External drive location has no drive identity.".to_string())?;

            let destination_occupied: bool = transaction
                .query_row(
                    "SELECT EXISTS(
                        SELECT 1
                        FROM files
                        WHERE drive_id = ?1
                          AND relative_path = ?2 COLLATE NOCASE
                          AND NOT (drive_id = ?3 AND relative_path = ?4)
                    )",
                    params![
                        destination_drive_id,
                        destination_relative_path,
                        source_drive_id,
                        source_relative_path
                    ],
                    |row| row.get(0),
                )
                .map_err(|error| format!("Unable to check planned destination: {error}"))?;

            if destination_occupied {
                return Err(
                    "That destination already exists in the destination drive catalogue."
                        .to_string(),
                );
            }
        }
        "local_folder" => {
            // Folders on this Mac are always available, so check the real
            // folder rather than a catalogue.
            let local_path = destination_local_path
                .as_deref()
                .ok_or_else(|| "Folder location has no path.".to_string())?;

            if fs::symlink_metadata(Path::new(local_path).join(destination_relative_path)).is_ok() {
                return Err(
                    "That destination already exists in the folder on this Mac.".to_string()
                );
            }
        }
        _ => {}
    }

    // Two sources must never claim the same planned destination. Exclude this
    // source so an existing plan can still be edited or replaced.
    let destination_already_planned: bool = transaction
        .query_row(
            "SELECT EXISTS(
                SELECT 1
                FROM planned_moves
                WHERE destination_location_id = ?1
                  AND destination_relative_path = ?2 COLLATE NOCASE
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

    transaction
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

    let id = transaction
        .query_row(
            "SELECT id FROM planned_moves WHERE source_drive_id = ?1 AND source_relative_path = ?2",
            params![source_drive_id, source_relative_path],
            |row| row.get(0),
        )
        .map_err(|error| format!("Unable to read planned move id: {error}"))?;

    transaction
        .commit()
        .map_err(|error| format!("Unable to save planned move: {error}"))?;

    Ok(id)
}

#[tauri::command]
async fn create_planned_move(
    app: tauri::AppHandle,
    source_drive_id: String,
    source_relative_path: String,
    destination_location_id: String,
    destination_relative_path: String,
) -> Result<i64, String> {
    run_blocking(move || {
        let mut connection = open_database(&database_path(&app)?)?;
        plan_move(
            &mut connection,
            &source_drive_id,
            &source_relative_path,
            &destination_location_id,
            &destination_relative_path,
        )
    })
    .await
}

#[tauri::command]
async fn list_planned_moves(app: tauri::AppHandle) -> Result<Vec<PlannedMove>, String> {
    run_blocking(move || {
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
    })
    .await
}

#[derive(Debug)]
struct PreflightMove {
    id: i64,
    source_drive_id: String,
    source_relative_path: String,
    destination_location_id: String,
    destination_display_name: Option<String>,
    destination_kind: Option<String>,
    destination_available_bytes: Option<i64>,
}

fn plan_preflight(connection: &Connection) -> Result<PlanPreflight, String> {
    let moves = {
        let mut statement = connection
            .prepare(
                "SELECT p.id,
                        p.source_drive_id,
                        p.source_relative_path,
                        p.destination_location_id,
                        COALESCE(NULLIF(l.user_label, ''), l.display_name),
                        l.kind,
                        CASE WHEN l.kind = 'external_drive' THEN d.available_bytes ELSE NULL END
                 FROM planned_moves p
                 LEFT JOIN locations l ON l.id = p.destination_location_id
                 LEFT JOIN drives d ON d.persistent_identifier = l.drive_id
                 ORDER BY p.id",
            )
            .map_err(|error| format!("Unable to query plan preflight: {error}"))?;

        let rows = statement
            .query_map([], |row| {
                Ok(PreflightMove {
                    id: row.get(0)?,
                    source_drive_id: row.get(1)?,
                    source_relative_path: row.get(2)?,
                    destination_location_id: row.get(3)?,
                    destination_display_name: row.get(4)?,
                    destination_kind: row.get(5)?,
                    destination_available_bytes: row.get(6)?,
                })
            })
            .map_err(|error| format!("Unable to read plan preflight: {error}"))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("Unable to read plan preflight rows: {error}"))?
    };

    let mut issues = Vec::new();
    let mut counted_files: HashSet<(String, String)> = HashSet::new();
    let mut counted_files_by_destination: HashMap<String, HashSet<(String, String)>> =
        HashMap::new();
    let mut known_bytes = 0_i64;
    let mut unknown_size_count = 0_i64;
    let mut destination_totals: HashMap<String, (String, String, i64, i64, i64, Option<i64>)> =
        HashMap::new();
    let mut source_roots: Vec<(i64, String, String, bool)> = Vec::new();

    for planned in &moves {
        let Some(destination_name) = planned.destination_display_name.clone() else {
            issues.push(PlanPreflightIssue {
                code: "missing_destination".to_string(),
                message: format!(
                    "The destination for planned move {} is no longer available.",
                    planned.id
                ),
                move_id: Some(planned.id),
            });
            continue;
        };
        let destination_kind = planned
            .destination_kind
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let destination = destination_totals
            .entry(planned.destination_location_id.clone())
            .or_insert_with(|| {
                (
                    destination_name,
                    destination_kind,
                    0,
                    0,
                    0,
                    planned.destination_available_bytes,
                )
            });
        destination.2 += 1;

        let source: Option<(bool, Option<i64>)> = connection
            .query_row(
                "SELECT is_directory, size_bytes
                 FROM files
                 WHERE drive_id = ?1 AND relative_path = ?2",
                params![planned.source_drive_id, planned.source_relative_path],
                |row| Ok((row.get::<_, i64>(0)? != 0, row.get(1)?)),
            )
            .optional()
            .map_err(|error| format!("Unable to validate preflight source: {error}"))?;

        let Some((source_is_directory, source_size)) = source else {
            issues.push(PlanPreflightIssue {
                code: "missing_source".to_string(),
                message: format!(
                    "{} is no longer present in the catalogue.",
                    planned.source_relative_path
                ),
                move_id: Some(planned.id),
            });
            continue;
        };

        let source_lower = planned.source_relative_path.to_lowercase();
        let overlaps = source_roots
            .iter()
            .any(|(_, drive_id, path, is_directory)| {
                if drive_id != &planned.source_drive_id {
                    return false;
                }
                let path_lower = path.to_lowercase();
                (*is_directory && source_lower.starts_with(&format!("{path_lower}/")))
                    || (source_is_directory && path_lower.starts_with(&format!("{source_lower}/")))
            });
        if overlaps {
            issues.push(PlanPreflightIssue {
                code: "overlapping_source".to_string(),
                message: format!(
                    "{} overlaps another planned source. Its files are counted only once.",
                    planned.source_relative_path
                ),
                move_id: Some(planned.id),
            });
        }
        source_roots.push((
            planned.id,
            planned.source_drive_id.clone(),
            planned.source_relative_path.clone(),
            source_is_directory,
        ));

        let source_files: Vec<(String, Option<i64>)> = if source_is_directory {
            let mut statement = connection
                .prepare(
                    "SELECT relative_path, size_bytes
                     FROM files
                     WHERE drive_id = ?1
                       AND is_directory = 0
                       AND substr(relative_path, 1, length(?2) + 1) = ?2 || '/'",
                )
                .map_err(|error| format!("Unable to calculate planned folder size: {error}"))?;
            let rows = statement
                .query_map(
                    params![planned.source_drive_id, planned.source_relative_path],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|error| format!("Unable to read planned folder size: {error}"))?;

            let files = rows
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("Unable to read planned folder rows: {error}"))?;

            files
        } else {
            vec![(planned.source_relative_path.clone(), source_size)]
        };

        let mut has_unknown_size = false;
        let destination_files = counted_files_by_destination
            .entry(planned.destination_location_id.clone())
            .or_default();

        for (relative_path, size) in source_files {
            let key = (planned.source_drive_id.clone(), relative_path);

            if size.is_none() {
                has_unknown_size = true;
            }

            if counted_files.insert(key.clone()) {
                match size {
                    Some(bytes) => {
                        known_bytes = known_bytes.saturating_add(bytes);
                    }
                    None => {
                        unknown_size_count += 1;
                    }
                }
            }

            if destination_files.insert(key) {
                match size {
                    Some(bytes) => {
                        destination.3 = destination.3.saturating_add(bytes);
                    }
                    None => {
                        destination.4 += 1;
                    }
                }
            }
        }

        if has_unknown_size {
            issues.push(PlanPreflightIssue {
                code: "unknown_source_size".to_string(),
                message: format!(
                    "{} contains files whose size is unknown.",
                    planned.source_relative_path
                ),
                move_id: Some(planned.id),
            });
        }
    }

    let mut destinations: Vec<PlanPreflightDestination> = destination_totals
        .into_iter()
        .map(
            |(location_id, (display_name, kind, move_count, bytes, unknown, available))| {
                let projected = available.map(|free| free.saturating_sub(bytes).max(0));
                let sufficient = if unknown > 0 {
                    None
                } else {
                    available.map(|free| bytes <= free)
                };
                if available.is_some_and(|free| bytes > free) {
                    issues.push(PlanPreflightIssue {
                        code: "insufficient_capacity".to_string(),
                        message: format!(
                            "{display_name} does not have enough catalogued free space for the known planned data."
                        ),
                        move_id: None,
                    });
                }
                PlanPreflightDestination {
                    location_id,
                    display_name,
                    kind,
                    move_count,
                    known_bytes: bytes,
                    unknown_size_count: unknown,
                    available_bytes: available,
                    projected_available_bytes: projected,
                    capacity_sufficient: sufficient,
                }
            },
        )
        .collect();
    destinations.sort_by(|left, right| {
        left.display_name
            .to_lowercase()
            .cmp(&right.display_name.to_lowercase())
    });

    Ok(PlanPreflight {
        move_count: moves.len() as i64,
        known_bytes,
        unknown_size_count,
        destinations,
        issues,
    })
}

fn validate_plan_live(
    connection: &Connection,
    connected_drives: &[DriveInfo],
) -> Result<PlanLiveValidation, String> {
    let mut issues = Vec::new();

    let connected_by_id: HashMap<&str, &DriveInfo> = connected_drives
        .iter()
        .filter_map(|drive| drive.persistent_identifier.as_deref().map(|id| (id, drive)))
        .collect();

    let mut statement = connection
        .prepare(
            "SELECT p.id,
                    p.source_drive_id,
                    p.source_relative_path,
                    p.destination_location_id,
                    l.kind,
                    l.drive_id,
                    l.local_path
             FROM planned_moves p
             LEFT JOIN locations l ON l.id = p.destination_location_id
             ORDER BY p.id",
        )
        .map_err(|error| format!("Unable to prepare live plan validation: {error}"))?;

    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })
        .map_err(|error| format!("Unable to validate live plan: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Unable to read live plan validation: {error}"))?;

    for (
        move_id,
        source_drive_id,
        source_relative_path,
        destination_location_id,
        destination_kind,
        destination_drive_id,
        destination_local_path,
    ) in rows
    {
        match connected_by_id.get(source_drive_id.as_str()) {
            None => issues.push(PlanPreflightIssue {
                code: "source_drive_offline".to_string(),
                message: format!(
                    "The source drive for {} is not connected.",
                    source_relative_path
                ),
                move_id: Some(move_id),
            }),
            Some(source_drive) => {
                let source_path = Path::new(&source_drive.mount_point).join(&source_relative_path);

                match fs::symlink_metadata(&source_path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        issues.push(PlanPreflightIssue {
                            code: "source_missing_on_disk".to_string(),
                            message: format!(
                                "{} is in the catalogue but is not currently present on the connected source drive.",
                                source_relative_path
                            ),
                            move_id: Some(move_id),
                        });
                    }
                    Err(_) => {
                        issues.push(PlanPreflightIssue {
                            code: "source_unreadable_on_disk".to_string(),
                            message: format!(
                                "{} cannot currently be checked on the source drive.",
                                source_relative_path
                            ),
                            move_id: Some(move_id),
                        });
                    }
                    Ok(metadata) => {
                        let catalogued: Option<(bool, Option<i64>, Option<i64>)> = connection
                            .query_row(
                                "SELECT is_directory, size_bytes, modified_at
                                 FROM files
                                 WHERE drive_id = ?1 AND relative_path = ?2",
                                params![source_drive_id, source_relative_path],
                                |row| Ok((row.get::<_, i64>(0)? != 0, row.get(1)?, row.get(2)?)),
                            )
                            .optional()
                            .map_err(|error| {
                                format!("Unable to compare live source with catalogue: {error}")
                            })?;

                        if let Some((
                            catalogued_is_directory,
                            catalogued_size,
                            catalogued_modified,
                        )) = catalogued
                        {
                            let live_is_directory = metadata.is_dir();

                            if live_is_directory != catalogued_is_directory {
                                issues.push(PlanPreflightIssue {
                                    code: "source_type_changed".to_string(),
                                    message: format!(
                                        "{} has changed type since it was catalogued.",
                                        source_relative_path
                                    ),
                                    move_id: Some(move_id),
                                });
                            } else if !catalogued_is_directory {
                                let live_size = metadata.len().min(i64::MAX as u64) as i64;
                                let live_modified = system_time_unix(metadata.modified());

                                if catalogued_size.is_some_and(|size| size != live_size)
                                    || (catalogued_modified.is_some()
                                        && live_modified != catalogued_modified)
                                {
                                    issues.push(PlanPreflightIssue {
                                        code: "source_changed".to_string(),
                                        message: format!(
                                            "{} has changed since it was catalogued. Rescan the source drive before transferring.",
                                            source_relative_path
                                        ),
                                        move_id: Some(move_id),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        let destination_root = match destination_kind.as_deref() {
            Some("external_drive") => destination_drive_id
                .as_deref()
                .and_then(|drive_id| connected_by_id.get(drive_id))
                .map(|drive| PathBuf::from(&drive.mount_point)),
            Some("local_folder") => destination_local_path.as_deref().map(PathBuf::from),
            _ => None,
        };

        let destination_relative_path: Option<String> = connection
            .query_row(
                "SELECT destination_relative_path
                 FROM planned_moves
                 WHERE id = ?1",
                params![move_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("Unable to read planned destination path: {error}"))?;

        if let (Some(root), Some(relative_path)) = (
            destination_root.as_ref(),
            destination_relative_path.as_ref(),
        ) {
            let destination_path = root.join(relative_path);

            match fs::symlink_metadata(&destination_path) {
                Ok(_) => issues.push(PlanPreflightIssue {
                    code: "destination_exists".to_string(),
                    message: format!(
                        "{} already exists at the planned destination.",
                        relative_path
                    ),
                    move_id: Some(move_id),
                }),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => issues.push(PlanPreflightIssue {
                    code: "destination_unreadable".to_string(),
                    message: format!(
                        "Media Mapper cannot confirm whether {} is clear at the planned destination.",
                        relative_path
                    ),
                    move_id: Some(move_id),
                }),
            }
        }

        match destination_kind.as_deref() {
            Some("external_drive") => {
                let connected = destination_drive_id
                    .as_deref()
                    .and_then(|drive_id| connected_by_id.get(drive_id));

                if connected.is_none() {
                    issues.push(PlanPreflightIssue {
                        code: "destination_drive_offline".to_string(),
                        message: format!(
                            "Destination {} is not currently connected.",
                            destination_location_id
                        ),
                        move_id: Some(move_id),
                    });
                }
            }
            Some("local_folder") => match destination_local_path.as_deref() {
                Some(path) => {
                    let destination = Path::new(path);
                    if !destination.exists() {
                        issues.push(PlanPreflightIssue {
                            code: "destination_folder_missing".to_string(),
                            message: format!(
                                "The destination folder {} is no longer available.",
                                path
                            ),
                            move_id: Some(move_id),
                        });
                    } else if !destination.is_dir() {
                        issues.push(PlanPreflightIssue {
                            code: "destination_not_folder".to_string(),
                            message: format!("The destination {} is no longer a folder.", path),
                            move_id: Some(move_id),
                        });
                    }
                }
                None => issues.push(PlanPreflightIssue {
                    code: "destination_folder_missing".to_string(),
                    message: "The planned local destination no longer has a folder path."
                        .to_string(),
                    move_id: Some(move_id),
                }),
            },
            _ => issues.push(PlanPreflightIssue {
                code: "destination_missing".to_string(),
                message: format!(
                    "Destination {} is no longer available.",
                    destination_location_id
                ),
                move_id: Some(move_id),
            }),
        }
    }

    // Capacity is checked once per external destination using current diskutil
    // free space, rather than the value stored at the last catalogue scan.
    let preflight = plan_preflight(connection)?;
    for destination in &preflight.destinations {
        if destination.kind != "external_drive" {
            continue;
        }

        let drive_id = destination
            .location_id
            .strip_prefix("drive:")
            .unwrap_or(&destination.location_id);

        let Some(drive) = connected_by_id.get(drive_id) else {
            continue;
        };

        if destination.unknown_size_count > 0 {
            issues.push(PlanPreflightIssue {
                code: "live_capacity_unknown".to_string(),
                message: format!(
                    "{} contains planned files with unknown sizes, so current free-space requirements cannot be confirmed.",
                    destination.display_name
                ),
                move_id: None,
            });
        } else if let Some(available) = drive.available_bytes {
            if destination.known_bytes as u64 > available {
                issues.push(PlanPreflightIssue {
                    code: "live_insufficient_capacity".to_string(),
                    message: format!(
                        "{} does not currently have enough free space for the planned data.",
                        destination.display_name
                    ),
                    move_id: None,
                });
            }
        }
    }

    Ok(PlanLiveValidation {
        ready: issues.is_empty(),
        issues,
    })
}

#[tauri::command]
async fn validate_plan(app: tauri::AppHandle) -> Result<PlanLiveValidation, String> {
    let drives = external_drives()?;

    run_blocking(move || {
        let connection = open_database(&database_path(&app)?)?;
        validate_plan_live(&connection, &drives)
    })
    .await
}

#[tauri::command]
async fn get_plan_preflight(app: tauri::AppHandle) -> Result<PlanPreflight, String> {
    run_blocking(move || {
        let connection = open_database(&database_path(&app)?)?;
        plan_preflight(&connection)
    })
    .await
}

// Planned destinations can sit inside folders that do not exist yet, such as
// `New Folder/film.mp4` planned from the drive root. Returns the next folder
// below `parent_path` on the way to each such destination, so the planned view
// can show it and the user can open it. Items directly inside `parent_path`
// are not folders on the way to anything and are left out.
fn planned_intermediate_folders<'a>(
    destinations: impl IntoIterator<Item = &'a str>,
    parent_path: &str,
) -> Vec<String> {
    let prefix = if parent_path.is_empty() {
        String::new()
    } else {
        format!("{parent_path}/")
    };

    let mut folders = Vec::new();
    for destination in destinations {
        let Some(remainder) = destination.strip_prefix(&prefix) else {
            continue;
        };
        let Some((next_folder, _)) = remainder.split_once('/') else {
            continue;
        };
        let folder = format!("{prefix}{next_folder}");
        if !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    folders
}

#[tauri::command]
async fn list_planned_folder_entries(
    app: tauri::AppHandle,
    destination_location_id: String,
    parent_path: String,
) -> Result<Vec<PlannedFolderEntry>, String> {
    run_blocking(move || {
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
                        is_new_folder: false,
                    })
                })
                .map_err(|error| format!("Unable to read planned folder: {error}"))?;

            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("Unable to read planned folder rows: {error}"))?
        };

        let new_folders = planned_intermediate_folders(
            roots
                .iter()
                .map(|root| root.destination_relative_path.as_str()),
            &parent_path,
        );

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
                    .map_err(|error| {
                        format!("Unable to query planned directory contents: {error}")
                    })?;

                let rows = statement
                    .query_map(params![root.source_drive_id, source_parent], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)? != 0,
                            row.get::<_, Option<i64>>(3)?,
                        ))
                    })
                    .map_err(|error| {
                        format!("Unable to read planned directory contents: {error}")
                    })?;

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
                    is_new_folder: false,
                });
            }
        }

        // A planned folder move can already be the folder itself; only add
        // folders that nothing else in this listing represents.
        for folder in new_folders {
            if entries
                .iter()
                .any(|entry| entry.destination_relative_path == folder)
            {
                continue;
            }
            let name = folder.rsplit('/').next().unwrap_or(&folder).to_owned();
            entries.push(PlannedFolderEntry {
                move_id: 0,
                source_drive_id: String::new(),
                source_drive_name: String::new(),
                source_relative_path: String::new(),
                name,
                is_directory: true,
                size_bytes: None,
                destination_relative_path: folder,
                is_new_folder: true,
            });
        }

        entries.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()));

        Ok(entries)
    })
    .await
}

#[tauri::command]
async fn remove_planned_move(app: tauri::AppHandle, id: i64) -> Result<(), String> {
    run_blocking(move || {
        let connection = open_database(&database_path(&app)?)?;
        let changed = connection
            .execute("DELETE FROM planned_moves WHERE id = ?1", params![id])
            .map_err(|error| format!("Unable to remove planned move: {error}"))?;
        if changed == 0 {
            return Err("That planned move no longer exists.".to_string());
        }
        Ok(())
    })
    .await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // A failure here must not stop the app from opening. Commands
            // open the database themselves and will report the same error.
            if let Err(error) = initialise_database(app.handle()) {
                eprintln!("Media Mapper database initialisation failed: {error}");
            }

            if let Some(window) = app.get_webview_window("main") {
                window.show()?;
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
            get_plan_preflight,
            validate_plan,
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
        assert_eq!(
            visible_files, 0,
            "uncommitted scan rows must stay invisible"
        );

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

    // The catalogue and planned-move schema as created by 7b80a78, before
    // locations existed. Planned moves pointed straight at a drive.
    const LEGACY_SCHEMA: &str = "
        PRAGMA journal_mode = WAL;
        PRAGMA foreign_keys = ON;

        CREATE TABLE drives (
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

        CREATE TABLE files (
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

        CREATE TABLE planned_moves (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_drive_id TEXT NOT NULL,
            source_relative_path TEXT NOT NULL,
            destination_drive_id TEXT NOT NULL,
            destination_relative_path TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            UNIQUE(source_drive_id, source_relative_path),
            FOREIGN KEY(source_drive_id) REFERENCES drives(persistent_identifier) ON DELETE CASCADE,
            FOREIGN KEY(destination_drive_id) REFERENCES drives(persistent_identifier) ON DELETE CASCADE
        );

        CREATE INDEX idx_planned_moves_destination
            ON planned_moves(destination_drive_id, destination_relative_path);

        INSERT INTO drives (persistent_identifier, name, last_seen_at)
            VALUES ('UUID-A', 'Source', 0), ('UUID-B', 'Archive', 0);
        INSERT INTO files (drive_id, relative_path, name, parent_path, is_directory, size_bytes)
            VALUES ('UUID-A', 'film.mp4', 'film.mp4', '', 0, 100);
        INSERT INTO planned_moves (
            id, source_drive_id, source_relative_path,
            destination_drive_id, destination_relative_path, created_at
        ) VALUES (7, 'UUID-A', 'film.mp4', 'UUID-B', 'Video/film.mp4', 123);
    ";

    // c77c108 added a locations table (without user labels) but had not yet
    // migrated planned moves to it.
    const LEGACY_LOCATIONS_SCHEMA: &str = "
        CREATE TABLE locations (
            id TEXT PRIMARY KEY,
            kind TEXT NOT NULL CHECK(kind IN ('external_drive', 'local_folder')),
            display_name TEXT NOT NULL,
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

        CREATE UNIQUE INDEX idx_locations_drive
            ON locations(drive_id)
            WHERE drive_id IS NOT NULL;

        CREATE UNIQUE INDEX idx_locations_local_path
            ON locations(local_path)
            WHERE local_path IS NOT NULL;

        INSERT INTO locations (id, kind, display_name, drive_id, local_path, created_at)
            VALUES
                ('drive:UUID-A', 'external_drive', 'Source', 'UUID-A', NULL, 0),
                ('drive:UUID-B', 'external_drive', 'Archive', 'UUID-B', NULL, 0);
    ";

    fn assert_planned_moves_migrated(path: &Path) {
        let connection = open_database(path).expect("legacy database must migrate");

        let columns: HashSet<String> = connection
            .prepare("PRAGMA table_info(planned_moves)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(columns.contains("destination_location_id"));
        assert!(!columns.contains("destination_drive_id"));

        let row: (i64, String, String, String, String, i64) = connection
            .query_row(
                "SELECT id, source_drive_id, source_relative_path,
                        destination_location_id, destination_relative_path, created_at
                 FROM planned_moves",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            (
                7,
                "UUID-A".to_string(),
                "film.mp4".to_string(),
                "drive:UUID-B".to_string(),
                "Video/film.mp4".to_string(),
                123
            )
        );

        let index_columns: Vec<String> = connection
            .prepare("PRAGMA index_info(idx_planned_moves_destination)")
            .unwrap()
            .query_map([], |row| row.get(2))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            index_columns,
            ["destination_location_id", "destination_relative_path"]
        );

        let broken_references: i64 = connection
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(broken_references, 0);

        // The migrated plan is readable the same way the commands read it.
        let destination_name: String = connection
            .query_row(
                "SELECT COALESCE(NULLIF(dl.user_label, ''), dl.display_name)
                 FROM planned_moves p
                 JOIN locations dl ON dl.id = p.destination_location_id",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(destination_name, "Archive");

        // Opening again must not try to migrate a second time.
        drop(connection);
        open_database(path).expect("migrated database must reopen");
    }

    #[test]
    fn migrates_planned_moves_from_before_locations_existed() {
        let database = TestDatabase::new("legacy-7b80a78");
        Connection::open(&database.0)
            .unwrap()
            .execute_batch(LEGACY_SCHEMA)
            .unwrap();

        assert_planned_moves_migrated(&database.0);
    }

    #[test]
    fn migrates_planned_moves_when_locations_table_already_exists() {
        let database = TestDatabase::new("legacy-c77c108");
        {
            let legacy = Connection::open(&database.0).unwrap();
            legacy.execute_batch(LEGACY_SCHEMA).unwrap();
            legacy.execute_batch(LEGACY_LOCATIONS_SCHEMA).unwrap();
        }

        assert_planned_moves_migrated(&database.0);
    }

    // A temporary folder standing in for a mounted volume.
    struct TestVolume(PathBuf);

    impl Drop for TestVolume {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_volume_system_folders_only_at_the_root() {
        use std::os::unix::fs::PermissionsExt;

        let database = TestDatabase::new("system-folders");
        let volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-volume-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let _ = fs::remove_dir_all(&volume.0);

        for folder in [
            ".Trashes/501",
            ".Spotlight-V100/Store-V2",
            ".fseventsd",
            "$Recycle.Bin/S-1-5-21",
            "System Volume Information",
            "Video/.Trashes",
        ] {
            fs::create_dir_all(volume.0.join(folder)).unwrap();
        }
        fs::write(volume.0.join(".Trashes/501/deleted.mp4"), b"deleted").unwrap();
        fs::write(volume.0.join("Video/film.mp4"), b"film").unwrap();
        fs::write(volume.0.join("Video/.Trashes/kept.txt"), b"kept").unwrap();

        // Spotlight's store is usually unreadable. It must not abort the scan.
        let spotlight = volume.0.join(".Spotlight-V100");
        fs::set_permissions(&spotlight, fs::Permissions::from_mode(0o000)).unwrap();

        let mut connection = open_database(&database.0).unwrap();
        insert_drive(&connection, "UUID-SCAN", "Test");
        let transaction = connection.transaction().unwrap();
        let mut insert = transaction
            .prepare(
                "INSERT INTO files
                    (drive_id, relative_path, name, parent_path, is_directory, size_bytes, modified_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .unwrap();
        let mut counters = (0_i64, 0_i64, 0_i64, 0_i64);

        let result = scan_directory(
            &volume.0,
            &mut insert,
            "UUID-SCAN",
            &mut counters,
            &mut Vec::new(),
            &|_, _| {},
            &mut Instant::now(),
        );
        fs::set_permissions(&spotlight, fs::Permissions::from_mode(0o755)).unwrap();
        result.expect("system folders must not abort the scan");
        drop(insert);

        let paths: Vec<String> = transaction
            .prepare("SELECT relative_path FROM files ORDER BY relative_path")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            paths,
            [
                "Video",
                "Video/.Trashes",
                "Video/.Trashes/kept.txt",
                "Video/film.mp4"
            ]
        );

        // Two files of 4 bytes, two folders, and nothing counted as skipped.
        assert_eq!(counters, (2, 2, 8, 0));
    }

    #[test]
    fn planning_treats_destinations_that_differ_in_case_as_the_same() {
        let database = TestDatabase::new("plan-case");
        let folder = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-local-{}-{}",
            std::process::id(),
            now_unix()
        )));
        fs::create_dir_all(&folder.0).unwrap();
        fs::write(folder.0.join("exists.mp4"), b"x").unwrap();

        let mut connection = open_database(&database.0).unwrap();
        insert_drive(&connection, "UUID-A", "Source");
        insert_drive(&connection, "UUID-B", "Archive");
        sync_drive_locations(&connection).unwrap();
        connection
            .execute_batch(
                "INSERT INTO files (drive_id, relative_path, name, parent_path, is_directory)
                 VALUES ('UUID-A', 'film.mp4', 'film.mp4', '', 0),
                        ('UUID-A', 'Folder', 'Folder', '', 1),
                        ('UUID-A', 'Folder/clip.mp4', 'clip.mp4', 'Folder', 0),
                        ('UUID-B', 'Video', 'Video', '', 1),
                        ('UUID-B', 'Video/Film.mp4', 'Film.mp4', 'Video', 0);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO locations (id, kind, display_name, local_path, created_at)
                 VALUES ('local:test', 'local_folder', 'Test', ?1, 0)",
                params![folder.0.to_string_lossy()],
            )
            .unwrap();

        let mut plan = |source: &str, location: &str, destination: &str| {
            plan_move(&mut connection, "UUID-A", source, location, destination)
        };

        // Occupied in the destination catalogue, differing only in case.
        let error = plan("film.mp4", "drive:UUID-B", "video/film.mp4").unwrap_err();
        assert!(error.contains("already exists"), "{error}");

        let first = plan("film.mp4", "drive:UUID-B", "Video/new.mp4").unwrap();

        // Another source cannot claim the same destination in other case.
        let error = plan("Folder/clip.mp4", "drive:UUID-B", "VIDEO/NEW.mp4").unwrap_err();
        assert!(error.contains("Another planned move"), "{error}");

        // The same source can still re-plan, keeping its id.
        assert_eq!(
            plan("film.mp4", "drive:UUID-B", "Video/NEW.mp4").unwrap(),
            first
        );

        // Changing only letter case in place is not blocked by the file itself.
        plan("film.mp4", "drive:UUID-A", "Film.mp4").unwrap();

        let error = plan("Folder", "drive:UUID-A", "folder/Sub/Folder").unwrap_err();
        assert!(error.contains("inside itself"), "{error}");

        // Folders on this Mac are checked on disk.
        let error = plan("Folder/clip.mp4", "local:test", "exists.mp4").unwrap_err();
        assert!(error.contains("folder on this Mac"), "{error}");
        plan("Folder/clip.mp4", "local:test", "fresh.mp4").unwrap();
    }

    #[test]
    fn folders_on_external_drives_are_matched_by_mount_point() {
        let drive = |name: &str, mount_point: &str| DriveInfo {
            name: name.to_string(),
            mount_point: mount_point.to_string(),
            filesystem: None,
            total_bytes: None,
            available_bytes: None,
            persistent_identifier: None,
            device_identifier: None,
        };
        let drives = [
            drive("Backup", "/Volumes/Backup"),
            drive("Mars", "/Volumes/Mars"),
        ];

        let found = |path: &str| {
            external_drive_containing(Path::new(path), &drives).map(|drive| drive.name.as_str())
        };
        assert_eq!(found("/Volumes/Mars/Video/Archive"), Some("Mars"));
        assert_eq!(found("/Volumes/Mars"), Some("Mars"));
        // Whole path components only: a sibling volume with a longer name
        // is not inside Mars.
        assert_eq!(found("/Volumes/Mars 2/Video"), None);
        assert_eq!(found("/Users/me/Movies"), None);
    }

    #[test]
    fn search_matches_accents_and_case_beyond_ascii() {
        let database = TestDatabase::new("unicode-search");
        let connection = open_database(&database.0).unwrap();
        insert_drive(&connection, "UUID-U", "Test");

        // As HFS+ stores it: decomposed, e followed by a combining acute accent.
        let decomposed = "Cafe\u{301} E\u{301}mile.MOV";
        connection
            .execute(
                "INSERT INTO files (drive_id, relative_path, name, parent_path, is_directory)
                 VALUES ('UUID-U', ?1, ?1, '', 0)",
                params![decomposed],
            )
            .unwrap();

        let matches = |query: &str| -> bool {
            let tokens = search_tokens(query);
            assert!(!tokens.is_empty());
            let expression = normalised_search_expression("name");
            tokens.iter().all(|token| {
                connection
                    .query_row(
                        &format!(
                            "SELECT EXISTS(SELECT 1 FROM files
                             WHERE {expression} LIKE '%' || ?1 || '%')"
                        ),
                        params![token],
                        |row| row.get(0),
                    )
                    .unwrap()
            })
        };

        // Typed composed, in other case, or with separators.
        assert!(matches("café"));
        assert!(matches("ÉMILE"));
        assert!(matches("émile.mov"));
        assert!(matches("CAFÉ_émile"));
        assert!(!matches("cafe emil x"));
    }

    #[test]
    fn capacity_is_read_for_apfs_and_other_volumes() {
        // Trimmed `diskutil info` output from real volumes.
        let exfat = parse_diskutil_info(
            "   File System Personality:   ExFAT
   Disk Size:                 500.1 GB (500106788864 Bytes) (exactly 976771072 512-Byte-Units)
   Volume Used Space:         94.4 GB (94447075328 Bytes) (exactly 184466944 512-Byte-Units) (18.9%)
   Volume Free Space:         405.6 GB (405643067392 Bytes) (exactly 792271616 512-Byte-Units) (81.1%)",
        );
        assert_eq!(
            diskutil_capacity(&exfat),
            (Some(500_106_788_864), Some(405_643_067_392))
        );

        let apfs = parse_diskutil_info(
            "   File System Personality:   APFS
   Disk Size:                 494.4 GB (494384795648 Bytes) (exactly 965595304 512-Byte-Units)
   Volume Used Space:         13.7 GB (13658537984 Bytes) (exactly 26676832 512-Byte-Units)
   Container Total Space:     494.4 GB (494384795648 Bytes) (exactly 965595304 512-Byte-Units)
   Container Free Space:      273.5 GB (273512333312 Bytes) (exactly 534203776 512-Byte-Units)",
        );
        assert_eq!(
            diskutil_capacity(&apfs),
            (Some(494_384_795_648), Some(273_512_333_312))
        );
    }

    #[test]
    fn probable_duplicates_are_grouped_by_name_ignoring_case_and_size() {
        let database = TestDatabase::new("duplicates");
        let connection = open_database(&database.0).unwrap();
        insert_drive(&connection, "UUID-A", "Alpha");
        insert_drive(&connection, "UUID-B", "Beta");
        connection
            .execute_batch(
                "INSERT INTO files (drive_id, relative_path, name, parent_path, is_directory, size_bytes)
                 VALUES ('UUID-B', 'x/film.MP4', 'film.MP4', 'x', 0, 100),
                        ('UUID-A', 'Film.mp4', 'Film.mp4', '', 0, 100),
                        ('UUID-B', 'Film.mp4', 'Film.mp4', '', 0, 50),
                        ('UUID-A', 'clip.mov', 'clip.mov', '', 0, 10),
                        ('UUID-A', 'sub/clip.mov', 'clip.mov', 'sub', 0, 10),
                        ('UUID-A', 'sub/clip2.mov', 'clip2.mov', 'sub', 0, 10),
                        ('UUID-A', 'empty.txt', 'empty.txt', '', 0, 0),
                        ('UUID-B', 'empty.txt', 'empty.txt', '', 0, 0),
                        ('UUID-A', 'sub', 'sub', '', 1, NULL),
                        ('UUID-B', 'sub', 'sub', '', 1, NULL);",
            )
            .unwrap();

        let groups = find_probable_duplicates(&connection).unwrap();
        let summary: Vec<(String, i64, i64, i64, Vec<String>)> = groups
            .iter()
            .map(|group| {
                (
                    group.name.clone(),
                    group.size_bytes,
                    group.copies,
                    group.potential_wasted_bytes,
                    group
                        .files
                        .iter()
                        .map(|file| format!("{}:{}", file.drive_name, file.relative_path))
                        .collect(),
                )
            })
            .collect();

        // Largest saving first; each group lists copies by drive, then path.
        // Different sizes, empty files and folders are never duplicates.
        assert_eq!(
            summary,
            [
                (
                    "Film.mp4".to_string(),
                    100,
                    2,
                    100,
                    vec!["Alpha:Film.mp4".to_string(), "Beta:x/film.MP4".to_string()]
                ),
                (
                    "clip.mov".to_string(),
                    10,
                    2,
                    10,
                    vec![
                        "Alpha:clip.mov".to_string(),
                        "Alpha:sub/clip.mov".to_string()
                    ]
                ),
            ]
        );
    }

    #[test]
    fn planned_paths_reject_empty_dot_and_parent_segments() {
        for valid in [
            "film.mp4",
            "Video/Archive/film.mp4",
            "My Films/ spaced .mp4",
        ] {
            assert!(validate_catalogue_relative_path(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            " ",
            "/Video/film.mp4",
            "Video/",
            "Video//film.mp4",
            "./film.mp4",
            "Video/./film.mp4",
            "../film.mp4",
            "Video/../film.mp4",
        ] {
            assert!(
                validate_catalogue_relative_path(invalid).is_err(),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn preflight_counts_folder_contents_and_deduplicates_overlapping_sources() {
        let database = TestDatabase::new("preflight-folder-overlap");
        let mut connection = open_database(&database.0).unwrap();
        insert_drive(&connection, "UUID-A", "Source");
        insert_drive(&connection, "UUID-B", "Backup");
        connection
            .execute(
                "UPDATE drives SET available_bytes = 150 WHERE persistent_identifier = 'UUID-B'",
                [],
            )
            .unwrap();
        sync_drive_locations(&connection).unwrap();
        connection
            .execute_batch(
                "INSERT INTO files
                    (drive_id, relative_path, name, parent_path, is_directory, size_bytes)
                 VALUES
                    ('UUID-A', 'Folder', 'Folder', '', 1, NULL),
                    ('UUID-A', 'Folder/a.mov', 'a.mov', 'Folder', 0, 100),
                    ('UUID-A', 'Folder/b.mov', 'b.mov', 'Folder', 0, NULL);",
            )
            .unwrap();

        plan_move(
            &mut connection,
            "UUID-A",
            "Folder",
            "drive:UUID-B",
            "Archive/Folder",
        )
        .unwrap();
        plan_move(
            &mut connection,
            "UUID-A",
            "Folder/a.mov",
            "drive:UUID-B",
            "Singles/a.mov",
        )
        .unwrap();

        let result = plan_preflight(&connection).unwrap();
        assert_eq!(result.move_count, 2);
        assert_eq!(result.known_bytes, 100);
        assert_eq!(result.unknown_size_count, 1);
        assert_eq!(result.destinations.len(), 1);

        let destination = &result.destinations[0];
        assert_eq!(destination.location_id, "drive:UUID-B");
        assert_eq!(destination.move_count, 2);
        assert_eq!(destination.known_bytes, 100);
        assert_eq!(destination.unknown_size_count, 1);
        assert_eq!(destination.available_bytes, Some(150));
        assert_eq!(destination.projected_available_bytes, Some(50));
        assert_eq!(destination.capacity_sufficient, None);
        assert!(result
            .issues
            .iter()
            .any(|issue| issue.code == "overlapping_source"));
        assert!(result
            .issues
            .iter()
            .any(|issue| issue.code == "unknown_source_size"));
    }

    #[test]
    fn preflight_counts_overlapping_sources_per_destination_without_double_counting_headline() {
        let database = TestDatabase::new("preflight-overlap-destinations");
        let mut connection = open_database(&database.0).unwrap();

        insert_drive(&connection, "UUID-A", "Source");
        insert_drive(&connection, "UUID-B", "Backup");
        insert_drive(&connection, "UUID-C", "Archive");
        sync_drive_locations(&connection).unwrap();

        connection
            .execute_batch(
                "INSERT INTO files
                    (drive_id, relative_path, name, parent_path, is_directory, size_bytes)
                 VALUES
                    ('UUID-A', 'Folder', 'Folder', '', 1, NULL),
                    ('UUID-A', 'Folder/a.mov', 'a.mov', 'Folder', 0, 100);",
            )
            .unwrap();

        plan_move(
            &mut connection,
            "UUID-A",
            "Folder",
            "drive:UUID-B",
            "Folder",
        )
        .unwrap();

        plan_move(
            &mut connection,
            "UUID-A",
            "Folder/a.mov",
            "drive:UUID-C",
            "a.mov",
        )
        .unwrap();

        let result = plan_preflight(&connection).unwrap();

        assert_eq!(result.move_count, 2);
        assert_eq!(result.known_bytes, 100);
        assert_eq!(result.unknown_size_count, 0);

        let backup = result
            .destinations
            .iter()
            .find(|destination| destination.location_id == "drive:UUID-B")
            .unwrap();
        let archive = result
            .destinations
            .iter()
            .find(|destination| destination.location_id == "drive:UUID-C")
            .unwrap();

        assert_eq!(backup.known_bytes, 100);
        assert_eq!(archive.known_bytes, 100);
        assert_eq!(backup.move_count, 1);
        assert_eq!(archive.move_count, 1);
    }

    #[test]
    fn preflight_reports_insufficient_capacity_and_missing_sources() {
        let database = TestDatabase::new("preflight-issues");
        let mut connection = open_database(&database.0).unwrap();
        insert_drive(&connection, "UUID-A", "Source");
        insert_drive(&connection, "UUID-B", "Backup");
        connection
            .execute(
                "UPDATE drives SET available_bytes = 50 WHERE persistent_identifier = 'UUID-B'",
                [],
            )
            .unwrap();
        sync_drive_locations(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO files
                    (drive_id, relative_path, name, parent_path, is_directory, size_bytes)
                 VALUES ('UUID-A', 'film.mov', 'film.mov', '', 0, 100)",
                [],
            )
            .unwrap();

        plan_move(
            &mut connection,
            "UUID-A",
            "film.mov",
            "drive:UUID-B",
            "film.mov",
        )
        .unwrap();
        connection
            .execute(
                "DELETE FROM files WHERE drive_id = 'UUID-A' AND relative_path = 'film.mov'",
                [],
            )
            .unwrap();

        let stale = plan_preflight(&connection).unwrap();
        assert_eq!(stale.move_count, 1);
        assert_eq!(stale.known_bytes, 0);
        assert!(stale
            .issues
            .iter()
            .any(|issue| issue.code == "missing_source"));

        connection
            .execute(
                "INSERT INTO files
                    (drive_id, relative_path, name, parent_path, is_directory, size_bytes)
                 VALUES ('UUID-A', 'film.mov', 'film.mov', '', 0, 100)",
                [],
            )
            .unwrap();
        let capacity = plan_preflight(&connection).unwrap();
        assert_eq!(capacity.known_bytes, 100);
        assert_eq!(capacity.destinations[0].capacity_sufficient, Some(false));
        assert_eq!(capacity.destinations[0].projected_available_bytes, Some(0));
        assert!(capacity
            .issues
            .iter()
            .any(|issue| issue.code == "insufficient_capacity"));
    }

    #[test]
    fn preflight_handles_empty_plan_and_unknown_local_capacity() {
        let database = TestDatabase::new("preflight-empty-local");
        let mut connection = open_database(&database.0).unwrap();
        insert_drive(&connection, "UUID-A", "Source");
        sync_drive_locations(&connection).unwrap();

        let empty = plan_preflight(&connection).unwrap();
        assert_eq!(empty.move_count, 0);
        assert_eq!(empty.known_bytes, 0);
        assert!(empty.destinations.is_empty());
        assert!(empty.issues.is_empty());

        connection
            .execute_batch(
                "INSERT INTO files
                    (drive_id, relative_path, name, parent_path, is_directory, size_bytes)
                 VALUES ('UUID-A', 'film.mov', 'film.mov', '', 0, 25);
                 INSERT INTO locations
                    (id, kind, display_name, drive_id, local_path, created_at)
                 VALUES ('local:test', 'local_folder', 'Movies', NULL, '/tmp', 0);",
            )
            .unwrap();
        plan_move(
            &mut connection,
            "UUID-A",
            "film.mov",
            "local:test",
            "media-mapper-preflight-film.mov",
        )
        .unwrap();

        let local = plan_preflight(&connection).unwrap();
        assert_eq!(local.known_bytes, 25);
        assert_eq!(local.destinations[0].available_bytes, None);
        assert_eq!(local.destinations[0].projected_available_bytes, None);
        assert_eq!(local.destinations[0].capacity_sufficient, None);
    }

    fn test_drive(id: &str, name: &str, mount_point: &Path, available: u64) -> DriveInfo {
        DriveInfo {
            name: name.to_string(),
            mount_point: mount_point.to_string_lossy().into_owned(),
            filesystem: Some("APFS".to_string()),
            total_bytes: Some(1_000),
            available_bytes: Some(available),
            persistent_identifier: Some(id.to_string()),
            device_identifier: None,
        }
    }

    #[test]
    fn live_validation_reports_offline_source_and_destination() {
        let database = TestDatabase::new("live-offline");
        let mut connection = open_database(&database.0).unwrap();

        insert_drive(&connection, "UUID-A", "Source");
        insert_drive(&connection, "UUID-B", "Backup");
        sync_drive_locations(&connection).unwrap();

        connection
            .execute(
                "INSERT INTO files
                    (drive_id, relative_path, name, parent_path, is_directory, size_bytes)
                 VALUES ('UUID-A', 'film.mov', 'film.mov', '', 0, 100)",
                [],
            )
            .unwrap();

        plan_move(
            &mut connection,
            "UUID-A",
            "film.mov",
            "drive:UUID-B",
            "film.mov",
        )
        .unwrap();

        let result = validate_plan_live(&connection, &[]).unwrap();

        assert!(!result.ready);
        assert!(result
            .issues
            .iter()
            .any(|issue| issue.code == "source_drive_offline"));
        assert!(result
            .issues
            .iter()
            .any(|issue| issue.code == "destination_drive_offline"));
    }

    #[test]
    fn live_validation_checks_source_disk_and_current_capacity() {
        let database = TestDatabase::new("live-disk-capacity");
        let mut connection = open_database(&database.0).unwrap();

        insert_drive(&connection, "UUID-A", "Source");
        insert_drive(&connection, "UUID-B", "Backup");
        sync_drive_locations(&connection).unwrap();

        connection
            .execute(
                "INSERT INTO files
                    (drive_id, relative_path, name, parent_path, is_directory, size_bytes)
                 VALUES ('UUID-A', 'film.mov', 'film.mov', '', 0, 100)",
                [],
            )
            .unwrap();

        plan_move(
            &mut connection,
            "UUID-A",
            "film.mov",
            "drive:UUID-B",
            "film.mov",
        )
        .unwrap();

        let source = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-live-source-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let destination = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-live-destination-{}-{}",
            std::process::id(),
            now_unix()
        )));
        fs::create_dir_all(&source.0).unwrap();
        fs::create_dir_all(&destination.0).unwrap();

        let drives = vec![
            test_drive("UUID-A", "Source", &source.0, 1_000),
            test_drive("UUID-B", "Backup", &destination.0, 50),
        ];

        let missing = validate_plan_live(&connection, &drives).unwrap();
        assert!(!missing.ready);
        assert!(missing
            .issues
            .iter()
            .any(|issue| issue.code == "source_missing_on_disk"));
        assert!(missing
            .issues
            .iter()
            .any(|issue| issue.code == "live_insufficient_capacity"));

        fs::write(source.0.join("film.mov"), vec![0_u8; 100]).unwrap();

        let enough_space = vec![
            test_drive("UUID-A", "Source", &source.0, 1_000),
            test_drive("UUID-B", "Backup", &destination.0, 500),
        ];

        let valid = validate_plan_live(&connection, &enough_space).unwrap();
        assert!(valid.ready, "{:?}", valid.issues);
        assert!(valid.issues.is_empty());
    }

    #[test]
    fn live_validation_checks_local_destination_folder() {
        let database = TestDatabase::new("live-local-folder");
        let mut connection = open_database(&database.0).unwrap();

        insert_drive(&connection, "UUID-A", "Source");
        sync_drive_locations(&connection).unwrap();

        let source = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-live-local-source-{}-{}",
            std::process::id(),
            now_unix()
        )));
        fs::create_dir_all(&source.0).unwrap();
        fs::write(source.0.join("film.mov"), b"film").unwrap();

        let local_path = std::env::temp_dir().join(format!(
            "media-mapper-live-local-destination-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let _ = fs::remove_dir_all(&local_path);

        connection
            .execute_batch(
                "INSERT INTO files
                    (drive_id, relative_path, name, parent_path, is_directory, size_bytes)
                 VALUES ('UUID-A', 'film.mov', 'film.mov', '', 0, 4);",
            )
            .unwrap();

        connection
            .execute(
                "INSERT INTO locations
                    (id, kind, display_name, drive_id, local_path, created_at)
                 VALUES ('local:test-live', 'local_folder', 'Local', NULL, ?1, 0)",
                params![local_path.to_string_lossy()],
            )
            .unwrap();

        plan_move(
            &mut connection,
            "UUID-A",
            "film.mov",
            "local:test-live",
            "film.mov",
        )
        .unwrap();

        let drives = vec![test_drive("UUID-A", "Source", &source.0, 1_000)];

        let missing = validate_plan_live(&connection, &drives).unwrap();
        assert!(!missing.ready);
        assert!(missing
            .issues
            .iter()
            .any(|issue| issue.code == "destination_folder_missing"));

        fs::create_dir_all(&local_path).unwrap();

        let valid = validate_plan_live(&connection, &drives).unwrap();
        assert!(valid.ready, "{:?}", valid.issues);

        fs::remove_dir_all(&local_path).unwrap();
    }

    #[test]
    fn live_validation_detects_changed_source_file() {
        let database = TestDatabase::new("live-source-changed");
        let mut connection = open_database(&database.0).unwrap();

        insert_drive(&connection, "UUID-A", "Source");
        insert_drive(&connection, "UUID-B", "Backup");
        sync_drive_locations(&connection).unwrap();

        let source = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-source-changed-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let destination = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-source-changed-destination-{}-{}",
            std::process::id(),
            now_unix()
        )));

        fs::create_dir_all(&source.0).unwrap();
        fs::create_dir_all(&destination.0).unwrap();
        fs::write(source.0.join("film.mov"), b"changed contents").unwrap();

        connection
            .execute(
                "INSERT INTO files
                    (drive_id, relative_path, name, parent_path,
                     is_directory, size_bytes, modified_at)
                 VALUES
                    ('UUID-A', 'film.mov', 'film.mov', '', 0, 4, NULL)",
                [],
            )
            .unwrap();

        plan_move(
            &mut connection,
            "UUID-A",
            "film.mov",
            "drive:UUID-B",
            "film.mov",
        )
        .unwrap();

        let drives = vec![
            test_drive("UUID-A", "Source", &source.0, 1_000),
            test_drive("UUID-B", "Backup", &destination.0, 1_000),
        ];

        let result = validate_plan_live(&connection, &drives).unwrap();

        assert!(!result.ready);
        assert!(
            result
                .issues
                .iter()
                .any(|issue| issue.code == "source_changed"),
            "{:?}",
            result.issues
        );
    }

    #[test]
    fn live_validation_detects_existing_destination() {
        let database = TestDatabase::new("live-destination-exists");
        let mut connection = open_database(&database.0).unwrap();

        insert_drive(&connection, "UUID-A", "Source");
        insert_drive(&connection, "UUID-B", "Backup");
        sync_drive_locations(&connection).unwrap();

        let source = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-collision-source-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let destination = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-collision-destination-{}-{}",
            std::process::id(),
            now_unix()
        )));

        fs::create_dir_all(&source.0).unwrap();
        fs::create_dir_all(&destination.0).unwrap();

        fs::write(source.0.join("film.mov"), b"film").unwrap();
        fs::write(destination.0.join("film.mov"), b"existing").unwrap();

        connection
            .execute(
                "INSERT INTO files
                    (drive_id, relative_path, name, parent_path,
                     is_directory, size_bytes, modified_at)
                 VALUES
                    ('UUID-A', 'film.mov', 'film.mov', '', 0, 4, NULL)",
                [],
            )
            .unwrap();

        plan_move(
            &mut connection,
            "UUID-A",
            "film.mov",
            "drive:UUID-B",
            "film.mov",
        )
        .unwrap();

        let drives = vec![
            test_drive("UUID-A", "Source", &source.0, 1_000),
            test_drive("UUID-B", "Backup", &destination.0, 1_000),
        ];

        let result = validate_plan_live(&connection, &drives).unwrap();

        assert!(!result.ready);
        assert!(
            result
                .issues
                .iter()
                .any(|issue| issue.code == "destination_exists"),
            "{:?}",
            result.issues
        );
    }

    #[test]
    fn verified_copy_creates_identical_destination_and_preserves_source() {
        let volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-copy-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let _ = fs::remove_dir_all(&volume.0);
        fs::create_dir_all(&volume.0).unwrap();

        let source = volume.0.join("source.bin");
        let destination = volume.0.join("nested/destination.bin");
        let contents = vec![0x5a_u8; 2 * 1024 * 1024 + 17];

        fs::write(&source, &contents).unwrap();

        let copied = copy_file_verified(&source, &destination).unwrap();

        assert_eq!(copied, contents.len() as u64);
        assert_eq!(fs::read(&source).unwrap(), contents);
        assert_eq!(fs::read(&destination).unwrap(), contents);
        assert!(source.exists());
    }

    #[test]
    fn verified_copy_refuses_existing_destination() {
        let volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-copy-collision-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let _ = fs::remove_dir_all(&volume.0);
        fs::create_dir_all(&volume.0).unwrap();

        let source = volume.0.join("source.bin");
        let destination = volume.0.join("destination.bin");

        fs::write(&source, b"source").unwrap();
        fs::write(&destination, b"existing").unwrap();

        let error = copy_file_verified(&source, &destination).unwrap_err();

        assert!(error.contains("Destination already exists"), "{error}");
        assert_eq!(fs::read(&source).unwrap(), b"source");
        assert_eq!(fs::read(&destination).unwrap(), b"existing");
    }

    #[test]
    fn creates_transfer_from_planned_move_snapshot() {
        let database = TestDatabase::new("create-transfer");
        let mut connection = open_database(&database.0).unwrap();

        insert_drive(&connection, "UUID-A", "Source");
        insert_drive(&connection, "UUID-B", "Backup");
        sync_drive_locations(&connection).unwrap();

        connection
            .execute(
                "INSERT INTO files (
                    drive_id,
                    relative_path,
                    name,
                    parent_path,
                    is_directory,
                    size_bytes
                 ) VALUES ('UUID-A', 'film.mov', 'film.mov', '', 0, 987)",
                [],
            )
            .unwrap();

        let move_id = plan_move(
            &mut connection,
            "UUID-A",
            "film.mov",
            "drive:UUID-B",
            "Archive/film.mov",
        )
        .unwrap();

        let transfer = create_transfer_record(&connection, move_id).unwrap();

        assert_eq!(transfer.planned_move_id, Some(move_id));
        assert_eq!(transfer.source_drive_id, "UUID-A");
        assert_eq!(transfer.source_relative_path, "film.mov");
        assert_eq!(transfer.destination_location_id, "drive:UUID-B");
        assert_eq!(transfer.destination_relative_path, "Archive/film.mov");
        assert_eq!(transfer.total_bytes, Some(987));
        assert_eq!(transfer.copied_bytes, 0);
        assert_eq!(transfer.status, "pending");
        assert_eq!(transfer.error_message, None);
    }

    #[test]
    fn transfer_snapshot_survives_plan_removal() {
        let database = TestDatabase::new("transfer-snapshot");
        let mut connection = open_database(&database.0).unwrap();

        insert_drive(&connection, "UUID-A", "Source");
        insert_drive(&connection, "UUID-B", "Backup");
        sync_drive_locations(&connection).unwrap();

        connection
            .execute(
                "INSERT INTO files (
                    drive_id,
                    relative_path,
                    name,
                    parent_path,
                    is_directory,
                    size_bytes
                 ) VALUES ('UUID-A', 'film.mov', 'film.mov', '', 0, 123)",
                [],
            )
            .unwrap();

        let move_id = plan_move(
            &mut connection,
            "UUID-A",
            "film.mov",
            "drive:UUID-B",
            "Archive/film.mov",
        )
        .unwrap();

        let transfer = create_transfer_record(&connection, move_id).unwrap();

        connection
            .execute("DELETE FROM planned_moves WHERE id = ?1", params![move_id])
            .unwrap();

        let records = list_transfer_records(&connection).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, transfer.id);
        assert_eq!(records[0].source_relative_path, "film.mov");
        assert_eq!(records[0].destination_relative_path, "Archive/film.mov");
        assert_eq!(records[0].status, "pending");
    }

    #[test]
    fn creating_transfer_requires_existing_planned_move() {
        let database = TestDatabase::new("missing-transfer-plan");
        let connection = open_database(&database.0).unwrap();

        let error = create_transfer_record(&connection, 999).unwrap_err();

        assert!(error.contains("planned move no longer exists"), "{error}");
        assert!(list_transfer_records(&connection).unwrap().is_empty());
    }

    #[test]
    fn transfer_schema_persists_execution_snapshot() {
        let database = TestDatabase::new("transfer-schema");
        let connection = open_database(&database.0).unwrap();

        connection
            .execute(
                "INSERT INTO transfers (
                    planned_move_id,
                    source_drive_id,
                    source_relative_path,
                    destination_location_id,
                    destination_relative_path,
                    total_bytes,
                    copied_bytes,
                    status,
                    created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    42_i64,
                    "UUID-A",
                    "Films/film.mov",
                    "drive:UUID-B",
                    "Archive/film.mov",
                    1234_i64,
                    0_i64,
                    "pending",
                    100_i64
                ],
            )
            .unwrap();

        let transfer = connection
            .query_row(
                "SELECT
                    id,
                    planned_move_id,
                    source_drive_id,
                    source_relative_path,
                    destination_location_id,
                    destination_relative_path,
                    total_bytes,
                    copied_bytes,
                    status,
                    error_message,
                    created_at,
                    started_at,
                    completed_at
                 FROM transfers",
                [],
                |row| {
                    Ok(TransferRecord {
                        id: row.get(0)?,
                        planned_move_id: row.get(1)?,
                        source_drive_id: row.get(2)?,
                        source_relative_path: row.get(3)?,
                        destination_location_id: row.get(4)?,
                        destination_relative_path: row.get(5)?,
                        total_bytes: row.get(6)?,
                        copied_bytes: row.get(7)?,
                        status: row.get(8)?,
                        error_message: row.get(9)?,
                        created_at: row.get(10)?,
                        started_at: row.get(11)?,
                        completed_at: row.get(12)?,
                    })
                },
            )
            .unwrap();

        assert_eq!(transfer.planned_move_id, Some(42));
        assert_eq!(transfer.source_drive_id, "UUID-A");
        assert_eq!(transfer.source_relative_path, "Films/film.mov");
        assert_eq!(transfer.destination_location_id, "drive:UUID-B");
        assert_eq!(transfer.destination_relative_path, "Archive/film.mov");
        assert_eq!(transfer.total_bytes, Some(1234));
        assert_eq!(transfer.copied_bytes, 0);
        assert_eq!(transfer.status, "pending");
        assert_eq!(transfer.error_message, None);
        assert_eq!(transfer.created_at, 100);
        assert_eq!(transfer.started_at, None);
        assert_eq!(transfer.completed_at, None);
    }

    #[test]
    fn transfer_schema_rejects_invalid_status() {
        let database = TestDatabase::new("transfer-status");
        let connection = open_database(&database.0).unwrap();

        let result = connection.execute(
            "INSERT INTO transfers (
                source_drive_id,
                source_relative_path,
                destination_location_id,
                destination_relative_path,
                status,
                created_at
             ) VALUES ('UUID-A', 'film.mov', 'drive:UUID-B', 'film.mov', 'deleted', 100)",
            [],
        );

        assert!(result.is_err());
    }

    #[test]
    fn schema_is_migrated_once_per_database() {
        let database = TestDatabase::new("migrate-once");
        open_database(&database.0).unwrap();

        // Remove something migrations would create. A later open must not
        // run them again, so it stays missing until the next app run.
        Connection::open(&database.0)
            .unwrap()
            .execute_batch("DROP INDEX idx_files_drive_parent;")
            .unwrap();
        let connection = open_database(&database.0).unwrap();
        let index_exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'idx_files_drive_parent')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!index_exists);
    }

    #[test]
    fn concurrent_first_opens_migrate_an_old_database_once() {
        let database = TestDatabase::new("migrate-concurrently");
        Connection::open(&database.0)
            .unwrap()
            .execute_batch(LEGACY_SCHEMA)
            .unwrap();

        let path = database.0.clone();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || open_database(&path).map(|_| ()))
            })
            .collect();
        for thread in threads {
            thread.join().unwrap().expect("every open must succeed");
        }

        assert_planned_moves_migrated(&database.0);
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_unreadable_folders_and_reports_them() {
        use std::os::unix::fs::PermissionsExt;

        let database = TestDatabase::new("unreadable-folders");
        let volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-unreadable-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let _ = fs::remove_dir_all(&volume.0);
        for folder in ["Video", "Private", "Public/Nested/Locked"] {
            fs::create_dir_all(volume.0.join(folder)).unwrap();
        }
        fs::write(volume.0.join("Video/film.mp4"), b"film").unwrap();
        fs::write(volume.0.join("Public/ok.txt"), b"ok").unwrap();
        fs::write(volume.0.join("Private/secret.mp4"), b"secret").unwrap();
        fs::write(volume.0.join("Public/Nested/Locked/x.mp4"), b"x").unwrap();

        let set_mode = |relative: &str, mode: u32| {
            fs::set_permissions(volume.0.join(relative), fs::Permissions::from_mode(mode)).unwrap();
        };

        let mut connection = open_database(&database.0).unwrap();
        insert_drive(&connection, "UUID-LOCK", "Test");
        let transaction = connection.transaction().unwrap();
        let mut insert = transaction
            .prepare(
                "INSERT INTO files
                    (drive_id, relative_path, name, parent_path, is_directory, size_bytes, modified_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .unwrap();
        let mut scan = |counters: &mut (i64, i64, i64, i64), unreadable: &mut Vec<String>| {
            transaction
                .execute("DELETE FROM files WHERE drive_id = 'UUID-LOCK'", [])
                .unwrap();
            scan_directory(
                &volume.0,
                &mut insert,
                "UUID-LOCK",
                counters,
                unreadable,
                &|_, _| {},
                &mut Instant::now(),
            )
        };

        set_mode("Private", 0o000);
        set_mode("Public/Nested/Locked", 0o000);
        let mut counters = (0_i64, 0_i64, 0_i64, 0_i64);
        let mut unreadable = Vec::new();
        let result = scan(&mut counters, &mut unreadable);
        set_mode("Private", 0o755);
        set_mode("Public/Nested/Locked", 0o755);

        result.expect("an unreadable folder must not stop the scan");
        unreadable.sort();
        assert_eq!(unreadable, ["Private", "Public/Nested/Locked"]);

        // The unreadable folders themselves are catalogued; their contents
        // are not.
        let paths: Vec<String> = transaction
            .prepare("SELECT relative_path FROM files ORDER BY relative_path")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            paths,
            [
                "Private",
                "Public",
                "Public/Nested",
                "Public/Nested/Locked",
                "Public/ok.txt",
                "Video",
                "Video/film.mp4"
            ]
        );
        assert_eq!(counters, (2, 5, 6, 0));

        // With nothing readable at the top of the drive the scan still fails,
        // keeping the previous catalogue.
        set_mode("", 0o000);
        let result = scan(&mut (0, 0, 0, 0), &mut Vec::new());
        set_mode("", 0o755);
        let error = result.unwrap_err();
        assert!(error.contains("top folder"), "{error}");
    }

    #[test]
    fn planned_view_shows_folders_that_exist_only_in_the_plan() {
        let destinations = [
            "New Folder/film.mp4",
            "New Folder/Extras/clip.mp4",
            "Archive/2024/Deep/scan.tif",
            "top.mp4",
        ];

        // At the drive root: the first folder on the way to each destination,
        // once each, and nothing for items that sit directly at the root.
        assert_eq!(
            planned_intermediate_folders(destinations, ""),
            ["New Folder", "Archive"]
        );

        // Inside a new folder: its planned subfolders, but not its own files.
        assert_eq!(
            planned_intermediate_folders(destinations, "New Folder"),
            ["New Folder/Extras"]
        );
        assert_eq!(
            planned_intermediate_folders(destinations, "Archive/2024"),
            ["Archive/2024/Deep"]
        );

        // A folder name that only shares a prefix is not a parent.
        assert!(planned_intermediate_folders(destinations, "New").is_empty());
        assert!(planned_intermediate_folders(destinations, "Archive/2024/Deep").is_empty());
    }
}
