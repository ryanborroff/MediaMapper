use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    fs,
    io::{BufReader, BufWriter, Read, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
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
    // When a scan of this drive last completed. It is written only in the
    // scan's own transaction, so a failed or cancelled scan leaves it as it
    // was. The catalogue is a snapshot from this moment; the drive itself may
    // have changed since.
    last_scanned_at: Option<i64>,
    // When Media Mapper last saw this drive connected, stored in the drive's
    // `last_seen_at` column. Connecting a drive is not a scan.
    last_connected_at: Option<i64>,
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
    // The destination an issue is about when it is not about one move, such
    // as a destination without enough free space.
    location_id: Option<String>,
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

fn update_transfer_status(
    connection: &Connection,
    transfer_id: i64,
    status: &str,
    copied_bytes: Option<i64>,
    error_message: Option<&str>,
) -> Result<(), String> {
    let now = now_unix();

    let changed = connection
        .execute(
            "UPDATE transfers
             SET status = ?1,
                 copied_bytes = COALESCE(?2, copied_bytes),
                 error_message = ?3,
                 started_at = CASE
                     WHEN ?1 = 'copying' AND started_at IS NULL THEN ?4
                     ELSE started_at
                 END,
                 completed_at = CASE
                     WHEN ?1 IN ('completed', 'failed') THEN ?4
                     ELSE completed_at
                 END
             WHERE id = ?5",
            params![status, copied_bytes, error_message, now, transfer_id],
        )
        .map_err(|error| format!("Unable to update transfer status: {error}"))?;

    if changed == 0 {
        return Err("The transfer record no longer exists.".to_string());
    }

    Ok(())
}

fn resolve_transfer_paths(
    connection: &Connection,
    planned_move_id: i64,
    connected_drives: &[DriveInfo],
) -> Result<(PathBuf, PathBuf), String> {
    let planned: Option<(
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    )> = connection
        .query_row(
            "SELECT
                    p.source_drive_id,
                    p.source_relative_path,
                    p.destination_location_id,
                    p.destination_relative_path,
                    l.kind,
                    l.drive_id,
                    l.local_path
                 FROM planned_moves p
                 LEFT JOIN locations l ON l.id = p.destination_location_id
                 WHERE p.id = ?1",
            params![planned_move_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()
        .map_err(|error| format!("Unable to resolve planned transfer: {error}"))?;

    let Some((
        source_drive_id,
        source_relative_path,
        destination_location_id,
        destination_relative_path,
        destination_kind,
        destination_drive_id,
        destination_local_path,
    )) = planned
    else {
        return Err("The planned move no longer exists.".to_string());
    };

    let source_drive = connected_drives
        .iter()
        .find(|drive| drive.persistent_identifier.as_deref() == Some(source_drive_id.as_str()))
        .ok_or_else(|| "The source drive is not connected.".to_string())?;

    let source = PathBuf::from(&source_drive.mount_point).join(&source_relative_path);

    let destination_root = match destination_kind.as_str() {
        "external_drive" => {
            let drive_id = destination_drive_id.ok_or_else(|| {
                format!("Destination location {destination_location_id} has no drive identity.")
            })?;

            let drive = connected_drives
                .iter()
                .find(|drive| drive.persistent_identifier.as_deref() == Some(drive_id.as_str()))
                .ok_or_else(|| "The destination drive is not connected.".to_string())?;

            PathBuf::from(&drive.mount_point)
        }
        "local_folder" => {
            let path = destination_local_path.ok_or_else(|| {
                format!("Destination location {destination_location_id} has no local folder.")
            })?;
            PathBuf::from(path)
        }
        _ => {
            return Err(format!(
                "Destination location {destination_location_id} has an unsupported type."
            ))
        }
    };

    let destination = destination_root.join(&destination_relative_path);

    Ok((source, destination))
}

// Only one transfer or drive scan runs at a time, across threads and across a
// second copy of the app. A scan holds the database's write lock for its whole
// run, so a transfer during a scan could copy and finalise a file but then fail
// to record it, leaving a finished copy recorded as interrupted. The lock is an
// exclusive OS file lock on a file beside the database. Closing the file
// releases it, and so does the process ending, so a crash never leaves it held.
struct TransferLock {
    _file: fs::File,
}

// Returns None while another transfer holds the lock.
fn try_lock_transfers(connection: &Connection) -> Result<Option<TransferLock>, String> {
    let path = match connection.path() {
        Some(path) if !path.is_empty() => PathBuf::from(format!("{path}.transfer-lock")),
        _ => return Err("Transfers need a catalogue database file.".to_string()),
    };
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|error| format!("Unable to open the transfer lock: {error}"))?;

    match file.try_lock() {
        Ok(()) => Ok(Some(TransferLock { _file: file })),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(error)) => Err(format!("Unable to lock transfers: {error}")),
    }
}

const INTERRUPTED_TRANSFER_MESSAGE: &str = "Interrupted before verification completed.";
const MISMATCHED_DESTINATION_MESSAGE: &str =
    "Interrupted after copying. The file now at the destination does not match the source.";
const UNCONFIRMED_DESTINATION_MESSAGE: &str =
    "Interrupted after copying. The source is no longer available to confirm the file at the destination.";

#[derive(Debug, Default, PartialEq)]
struct TransferRecovery {
    // Unfinished records resolved to failed.
    interrupted: usize,
    // Unfinished records whose copy had already been finalised, confirmed
    // byte for byte against the source and resolved to completed.
    completed: usize,
    temporary_files_removed: usize,
}

// What a transfer left in `verifying` actually reached before it stopped.
enum VerifyingOutcome {
    // The drives needed to tell are not available yet.
    Undetermined,
    Failed(&'static str),
    // The final file is in place and identical to the source.
    Finalised(i64),
}

// The rename into place happens only after verification succeeds, so a
// record left in `verifying` either stopped before the rename, leaving its
// temporary file, or stopped after it, before `completed` was recorded. In
// the second case the file at the destination is compared with the source
// again rather than trusted, since something else could have put it there.
fn verifying_outcome(
    source: Option<&Path>,
    destination: &Path,
    temporary: &Path,
    copied_bytes: i64,
) -> VerifyingOutcome {
    match fs::symlink_metadata(temporary) {
        Ok(_) => return VerifyingOutcome::Failed(INTERRUPTED_TRANSFER_MESSAGE),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return VerifyingOutcome::Undetermined,
    }

    let destination_length = match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_file() => metadata.len(),
        Ok(_) => return VerifyingOutcome::Failed(INTERRUPTED_TRANSFER_MESSAGE),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return VerifyingOutcome::Failed(INTERRUPTED_TRANSFER_MESSAGE)
        }
        Err(_) => return VerifyingOutcome::Undetermined,
    };

    let Some(source) = source else {
        return VerifyingOutcome::Undetermined;
    };
    match fs::symlink_metadata(source) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return VerifyingOutcome::Failed(UNCONFIRMED_DESTINATION_MESSAGE),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return VerifyingOutcome::Failed(UNCONFIRMED_DESTINATION_MESSAGE)
        }
        Err(_) => return VerifyingOutcome::Undetermined,
    }

    if i64::try_from(destination_length).ok() != Some(copied_bytes) {
        return VerifyingOutcome::Failed(MISMATCHED_DESTINATION_MESSAGE);
    }

    match files_are_identical(source, destination) {
        Ok(true) => VerifyingOutcome::Finalised(copied_bytes),
        Ok(false) => VerifyingOutcome::Failed(MISMATCHED_DESTINATION_MESSAGE),
        Err(error) => {
            eprintln!("Media Mapper transfer recovery: {error}");
            VerifyingOutcome::Undetermined
        }
    }
}

// The folder a location's relative paths start from, if it is available now.
fn connected_location_root(
    kind: Option<&str>,
    drive_id: Option<&str>,
    local_path: Option<&str>,
    connected_drives: &[DriveInfo],
) -> Option<PathBuf> {
    match kind {
        Some("external_drive") => drive_id.and_then(|drive_id| {
            connected_drives
                .iter()
                .find(|drive| drive.persistent_identifier.as_deref() == Some(drive_id))
                .map(|drive| PathBuf::from(&drive.mount_point))
        }),
        Some("local_folder") => local_path.map(PathBuf::from),
        _ => None,
    }
}

// Resolves each record left in `verifying` whose outcome can be determined
// with the drives connected now. The rest stay unresolved until it can.
fn resolve_verifying_transfers(
    connection: &Connection,
    connected_drives: &[DriveInfo],
    recovery: &mut TransferRecovery,
) -> Result<(), String> {
    let mut statement = connection
        .prepare(
            "SELECT t.id,
                    t.created_at,
                    t.planned_move_id,
                    t.source_drive_id,
                    t.source_relative_path,
                    t.destination_location_id,
                    t.destination_relative_path,
                    t.copied_bytes,
                    l.kind,
                    l.drive_id,
                    l.local_path
             FROM transfers t
             LEFT JOIN locations l ON l.id = t.destination_location_id
             WHERE t.status = 'verifying'",
        )
        .map_err(|error| format!("Unable to find unverified transfers: {error}"))?;
    let verifying = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
            ))
        })
        .map_err(|error| format!("Unable to find unverified transfers: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Unable to read unverified transfers: {error}"))?;

    for (
        id,
        created_at,
        planned_move_id,
        source_drive_id,
        source_relative_path,
        destination_location_id,
        destination_relative_path,
        copied_bytes,
        kind,
        drive_id,
        local_path,
    ) in verifying
    {
        let Some(root) = connected_location_root(
            kind.as_deref(),
            drive_id.as_deref(),
            local_path.as_deref(),
            connected_drives,
        ) else {
            continue;
        };
        let destination = root.join(&destination_relative_path);
        let Ok(temporary) = transfer_temporary_path(&destination, id, created_at) else {
            continue;
        };
        let source = connected_drives
            .iter()
            .find(|drive| drive.persistent_identifier.as_deref() == Some(&source_drive_id))
            .map(|drive| PathBuf::from(&drive.mount_point).join(&source_relative_path));

        match verifying_outcome(source.as_deref(), &destination, &temporary, copied_bytes) {
            VerifyingOutcome::Undetermined => {}
            VerifyingOutcome::Failed(message) => {
                update_transfer_status(connection, id, "failed", None, Some(message))?;
                recovery.interrupted += 1;
            }
            VerifyingOutcome::Finalised(bytes) => {
                // Record the completion and clear the plan together, as a
                // normal completed transfer does. Only the plan this transfer
                // came from is removed, and only if it still describes it.
                let transaction = connection
                    .unchecked_transaction()
                    .map_err(|error| format!("Unable to record recovered transfer: {error}"))?;
                update_transfer_status(&transaction, id, "completed", Some(bytes), None)?;
                transaction
                    .execute(
                        "DELETE FROM planned_moves
                         WHERE id = ?1
                           AND source_drive_id = ?2
                           AND source_relative_path = ?3
                           AND destination_location_id = ?4
                           AND destination_relative_path = ?5",
                        params![
                            planned_move_id,
                            source_drive_id,
                            source_relative_path,
                            destination_location_id,
                            destination_relative_path
                        ],
                    )
                    .map_err(|error| format!("Unable to clear recovered plan: {error}"))?;
                transaction
                    .commit()
                    .map_err(|error| format!("Unable to record recovered transfer: {error}"))?;
                recovery.completed += 1;
            }
        }
    }

    Ok(())
}

// Removes one transfer's temporary file. Only a regular file at exactly this
// path is removed; a folder, a symbolic link or anything else is left alone.
fn remove_transfer_temporary(temporary: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(temporary) {
        Ok(metadata) if metadata.file_type().is_file() => {
            fs::remove_file(temporary)
                .map_err(|error| format!("Unable to remove {}: {error}", temporary.display()))?;
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Unable to check {}: {error}", temporary.display())),
    }
}

// Resolves transfers left unfinished by an earlier run, and removes the
// temporary files of failed transfers. The caller must hold the transfer
// lock, so nothing resolved here can still be running.
//
// Records still `pending` or `copying` never reached the rename into place,
// so they are resolved to failed without needing any drive. Records left in
// `verifying` need the drives to tell whether their copy was finalised, so
// they are resolved only when drives are given.
//
// With connected drives it also removes the exact temporary file of each
// failed transfer whose destination is available, optionally limited to one
// destination. Temporary file paths are derived from each record, so no
// folder is scanned and no other file is touched. Running it again changes
// nothing.
fn recover_interrupted_transfers(
    connection: &Connection,
    connected_drives: Option<&[DriveInfo]>,
    only_destination: Option<(&str, &str)>,
) -> Result<TransferRecovery, String> {
    let interrupted = connection
        .execute(
            "UPDATE transfers
             SET status = 'failed', error_message = ?1, completed_at = ?2
             WHERE status IN ('pending', 'copying')",
            params![INTERRUPTED_TRANSFER_MESSAGE, now_unix()],
        )
        .map_err(|error| format!("Unable to recover interrupted transfers: {error}"))?;

    let mut recovery = TransferRecovery {
        interrupted,
        ..TransferRecovery::default()
    };

    let Some(connected_drives) = connected_drives else {
        return Ok(recovery);
    };

    resolve_verifying_transfers(connection, connected_drives, &mut recovery)?;

    let (location_filter, path_filter) = only_destination.unzip();
    let mut statement = connection
        .prepare(
            "SELECT t.id,
                    t.created_at,
                    t.destination_relative_path,
                    l.kind,
                    l.drive_id,
                    l.local_path
             FROM transfers t
             LEFT JOIN locations l ON l.id = t.destination_location_id
             WHERE t.status = 'failed'
               AND (?1 IS NULL OR (
                   t.destination_location_id = ?1
                   AND t.destination_relative_path = ?2
               ))",
        )
        .map_err(|error| format!("Unable to find failed transfers: {error}"))?;
    let failed = statement
        .query_map(params![location_filter, path_filter], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(|error| format!("Unable to find failed transfers: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Unable to read failed transfers: {error}"))?;

    for (id, created_at, relative_path, kind, drive_id, local_path) in failed {
        // An unavailable destination is cleaned up when it is next available.
        let Some(root) = connected_location_root(
            kind.as_deref(),
            drive_id.as_deref(),
            local_path.as_deref(),
            connected_drives,
        ) else {
            continue;
        };
        let Ok(temporary) = transfer_temporary_path(&root.join(&relative_path), id, created_at)
        else {
            continue;
        };

        match remove_transfer_temporary(&temporary) {
            Ok(true) => recovery.temporary_files_removed += 1,
            Ok(false) => {}
            // One stuck file must not stop the rest being cleaned up.
            Err(error) => eprintln!("Media Mapper transfer recovery: {error}"),
        }
    }

    Ok(recovery)
}

// Runs once at launch. Records that need no drive are resolved immediately,
// so the first history the window shows is as accurate as it can be. Records
// left in `verifying`, and temporary files, need the connected drives, which
// are slow to list, so they are handled on a background thread that tells the
// window when history has changed. Both steps are skipped while another copy
// of the app is transferring.
fn recover_transfers_at_startup(app: &tauri::AppHandle) -> Result<(), String> {
    let database = database_path(app)?;
    let connection = open_database(&database)?;

    if let Some(_lock) = try_lock_transfers(&connection)? {
        recover_interrupted_transfers(&connection, None, None)?;
    }

    let app = app.clone();
    std::thread::spawn(move || {
        let cleanup = || -> Result<TransferRecovery, String> {
            let drives = external_drives()?;
            let connection = open_database(&database)?;
            match try_lock_transfers(&connection)? {
                Some(_lock) => recover_interrupted_transfers(&connection, Some(&drives), None),
                None => Ok(TransferRecovery::default()),
            }
        };
        match cleanup() {
            Ok(recovery) if recovery.interrupted > 0 || recovery.completed > 0 => {
                let _ = app.emit("transfers-recovered", ());
            }
            Ok(_) => {}
            Err(error) => eprintln!("Media Mapper transfer recovery failed: {error}"),
        }
    });

    Ok(())
}

// Whether an issue stops one planned move from being copied: issues about that
// move, issues about its destination (such as free space), and any issue that
// names neither.
fn issue_blocks_move(
    issue: &PlanPreflightIssue,
    move_id: i64,
    destination_location_id: &str,
) -> bool {
    match (issue.move_id, issue.location_id.as_deref()) {
        (Some(issue_move_id), _) => issue_move_id == move_id,
        (None, Some(location_id)) => location_id == destination_location_id,
        (None, None) => true,
    }
}

const FOLDER_TRANSFER_UNSUPPORTED: &str =
    "Folders can't be copied yet. Plan the files inside the folder instead.";

#[cfg(test)]
fn execute_planned_transfer(
    connection: &Connection,
    planned_move_id: i64,
    connected_drives: &[DriveInfo],
) -> Result<TransferRecord, String> {
    execute_planned_transfer_reporting(
        connection,
        planned_move_id,
        connected_drives,
        &|_| {},
        &|| false,
    )
}

fn execute_planned_transfer_reporting(
    connection: &Connection,
    planned_move_id: i64,
    connected_drives: &[DriveInfo],
    report: &dyn Fn(&TransferProgress),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<TransferRecord, String> {
    let Some(_lock) = try_lock_transfers(connection)? else {
        return Err(
            "Another transfer or a drive scan is running. Wait for it to finish, then try again."
                .to_string(),
        );
    };

    // While the lock is held nothing else can be transferring, so any
    // unfinished record is stale. Resolve those, and remove the temporary
    // files of earlier attempts at this destination, before trying again.
    let destination: Option<(String, String)> = connection
        .query_row(
            "SELECT destination_location_id, destination_relative_path
             FROM planned_moves
             WHERE id = ?1",
            params![planned_move_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| format!("Unable to read planned transfer: {error}"))?;
    match &destination {
        Some((location_id, relative_path)) => recover_interrupted_transfers(
            connection,
            Some(connected_drives),
            Some((location_id, relative_path)),
        )?,
        None => recover_interrupted_transfers(connection, None, None)?,
    };

    // An earlier attempt at this plan may turn out to have been finalised
    // before it was interrupted. Recovery then confirmed it and cleared the
    // plan, so report that transfer rather than copying again.
    if destination.is_some() {
        let recovered = connection
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
                 WHERE planned_move_id = ?1
                   AND status = 'completed'
                   AND NOT EXISTS (SELECT 1 FROM planned_moves WHERE id = ?1)
                 ORDER BY id DESC
                 LIMIT 1",
                params![planned_move_id],
                read_transfer,
            )
            .optional()
            .map_err(|error| format!("Unable to read recovered transfer: {error}"))?;
        if let Some(recovered) = recovered {
            return Ok(recovered);
        }
    }

    let Some((destination_location_id, _)) = &destination else {
        return Err("The planned move no longer exists.".to_string());
    };

    // Transfers copy one file. Folder moves can be planned and previewed, but
    // copying one is refused here, before any record or folder is created,
    // rather than relying on the window to leave them out.
    let source_is_directory: bool = connection
        .query_row(
            "SELECT COALESCE(f.is_directory, 0) != 0
             FROM planned_moves p
             LEFT JOIN files f
               ON f.drive_id = p.source_drive_id
              AND f.relative_path = p.source_relative_path
             WHERE p.id = ?1",
            params![planned_move_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("Unable to read planned transfer: {error}"))?;
    if source_is_directory {
        return Err(FOLDER_TRANSFER_UNSUPPORTED.to_string());
    }

    // Only issues that concern this move stop it. A problem with another
    // planned move, such as its drive being disconnected, does not.
    let validation = validate_plan_live(connection, connected_drives)?;
    let preflight = plan_preflight(connection)?;
    if let Some(issue) = validation
        .issues
        .iter()
        .chain(preflight.issues.iter())
        .find(|issue| issue_blocks_move(issue, planned_move_id, destination_location_id))
    {
        return Err(format!(
            "Transfer blocked by final validation: {}",
            issue.message
        ));
    }

    let (source, destination) =
        resolve_transfer_paths(connection, planned_move_id, connected_drives)?;

    execute_transfer_paths_reporting(
        connection,
        planned_move_id,
        &source,
        &destination,
        report,
        is_cancelled,
    )
}

// Which part of a transfer is running: copying the file, then reading both
// files back to compare them.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum TransferStage {
    Copying,
    Verifying,
}

// Sent to the window as a large file copies, so progress is visible.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TransferProgress {
    planned_move_id: i64,
    stage: TransferStage,
    bytes: u64,
    total_bytes: u64,
}

// Set by Cancel copy and checked as each chunk is copied or verified.
static TRANSFER_CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);
const TRANSFER_CANCELLED: &str = "Copy cancelled.";
// A copy that was verified and put in place, but whose completion could not
// be saved. Media Mapper confirms it against the source when it next opens,
// or when the copy is tried again.
const COPIED_NOT_RECORDED: &str = "The file was copied and verified, but Media Mapper couldn't save that it finished. It will check the copy again the next time it opens.";

#[tauri::command]
fn cancel_transfer() {
    TRANSFER_CANCEL_REQUESTED.store(true, Ordering::SeqCst);
}

// How long transfer bookkeeping waits for the database. Once a copy has been
// finalised it must be recorded, so waiting out brief contention is far
// better than failing. Transfers run on a background thread, so the wait
// never freezes the window.
const TRANSFER_BUSY_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
fn execute_transfer_paths(
    connection: &Connection,
    planned_move_id: i64,
    source: &Path,
    destination: &Path,
) -> Result<TransferRecord, String> {
    execute_transfer_paths_reporting(
        connection,
        planned_move_id,
        source,
        destination,
        &|_| {},
        &|| false,
    )
}

fn execute_transfer_paths_reporting(
    connection: &Connection,
    planned_move_id: i64,
    source: &Path,
    destination: &Path,
    report: &dyn Fn(&TransferProgress),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<TransferRecord, String> {
    connection
        .busy_timeout(TRANSFER_BUSY_TIMEOUT)
        .map_err(|error| format!("Unable to configure transfer database: {error}"))?;

    let transfer = create_transfer_record(connection, planned_move_id)?;

    let temporary = match transfer_temporary_path(destination, transfer.id, transfer.created_at) {
        Ok(temporary) => temporary,
        Err(error) => {
            let _ = update_transfer_status(connection, transfer.id, "failed", None, Some(&error));
            return Err(error);
        }
    };

    if let Err(error) = update_transfer_status(connection, transfer.id, "copying", None, None) {
        return Err(error);
    }

    // Progress goes to the window at most every 150 ms, and whenever the
    // stage changes or a stage finishes. A cancel request stops the transfer
    // at the next chunk.
    let total_bytes = fs::metadata(source)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let mut last_report: Option<(TransferStage, Instant)> = None;
    let mut on_progress = |stage: TransferStage, bytes: u64| -> Result<(), String> {
        if is_cancelled() {
            return Err(TRANSFER_CANCELLED.to_string());
        }
        let due = match last_report {
            Some((last_stage, reported_at)) => {
                last_stage != stage || reported_at.elapsed() >= Duration::from_millis(150)
            }
            None => true,
        };
        if due || bytes == total_bytes {
            report(&TransferProgress {
                planned_move_id,
                stage,
                bytes,
                total_bytes,
            });
            last_report = Some((stage, Instant::now()));
        }
        Ok(())
    };

    let copy_result = copy_file_verified_with_progress(
        source,
        destination,
        &temporary,
        |copied_bytes| {
            let copied_bytes = i64::try_from(copied_bytes)
                .map_err(|_| "Copied file is too large to record.".to_string())?;

            update_transfer_status(
                connection,
                transfer.id,
                "verifying",
                Some(copied_bytes),
                None,
            )
        },
        &mut on_progress,
    );

    match copy_result {
        Ok(copied_bytes) => {
            let copied_bytes = i64::try_from(copied_bytes)
                .map_err(|_| "Copied file is too large to record.".to_string())?;

            // Recording the completion and clearing the plan happen together,
            // so a finished copy never keeps an active plan item. The transfer
            // record contains its own source/destination snapshot, so removing
            // the plan does not remove execution history.
            let record_completion = || -> Result<(), String> {
                let completion = connection
                    .unchecked_transaction()
                    .map_err(|error| format!("Unable to record completed transfer: {error}"))?;
                update_transfer_status(
                    &completion,
                    transfer.id,
                    "completed",
                    Some(copied_bytes),
                    None,
                )?;
                completion
                    .execute(
                        "DELETE FROM planned_moves WHERE id = ?1",
                        params![planned_move_id],
                    )
                    .map_err(|error| {
                        format!("Transfer completed, but the plan could not be cleared: {error}")
                    })?;
                completion
                    .commit()
                    .map_err(|error| format!("Unable to record completed transfer: {error}"))
            };
            // The copy is already verified and in place. The record stays
            // `verifying`, so it is never shown as completed until recovery
            // compares it with the source again, but the error must not
            // suggest that nothing was copied.
            if let Err(error) = record_completion() {
                eprintln!("Media Mapper transfer {}: {error}", transfer.id);
                return Err(COPIED_NOT_RECORDED.to_string());
            }
        }
        Err(error) => {
            // The transfer snapshot survives the failure and records why it
            // stopped. The copy primitive removes its temporary file and never
            // deletes the source.
            let _ = update_transfer_status(connection, transfer.id, "failed", None, Some(&error));

            return Err(error);
        }
    }

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
            params![transfer.id],
            read_transfer,
        )
        .map_err(|error| format!("Unable to read completed transfer: {error}"))
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

// Bypasses the memory cache for reads and writes through this file. A copy is
// written this way so none of it stays cached, and read back this way so
// verification has to read what the drive actually stored. Without it, the
// check compared the source with the copy still held in memory, which could
// not catch a drive or cable corrupting data on its way to the disk.
#[cfg(target_os = "macos")]
fn bypass_cache(file: &fs::File) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    use std::os::raw::c_int;

    const F_NOCACHE: c_int = 48;

    unsafe extern "C" {
        fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    }

    if unsafe { fcntl(file.as_raw_fd(), F_NOCACHE, 1 as c_int) } == -1 {
        return Err(format!(
            "Unable to bypass the file cache: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn bypass_cache(_file: &fs::File) -> Result<(), String> {
    Ok(())
}

const SOURCE_NOT_A_FILE: &str = "The source is no longer a file, so it was not copied.";

// Opens a file for reading only if the path itself is a regular file. A
// symbolic link at the path is never followed, so a source swapped for a
// link after validation cannot hand the copy some other file. Non-blocking
// mode keeps a named pipe from stalling the open; it changes nothing for
// regular files. Returns None for anything that is not a regular file.
fn open_regular_file(path: &Path) -> std::io::Result<Option<fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;

    #[cfg(target_os = "macos")]
    const O_NONBLOCK: i32 = 0x0004;
    #[cfg(target_os = "macos")]
    const O_NOFOLLOW: i32 = 0x0100;
    #[cfg(target_os = "macos")]
    const ELOOP: i32 = 62;
    #[cfg(not(target_os = "macos"))]
    const O_NONBLOCK: i32 = 0o4000;
    #[cfg(not(target_os = "macos"))]
    const O_NOFOLLOW: i32 = 0o400000;
    #[cfg(not(target_os = "macos"))]
    const ELOOP: i32 = 40;

    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(ELOOP) => return Ok(None),
        Err(error) => return Err(error),
    };
    if !file.metadata()?.file_type().is_file() {
        return Ok(None);
    }
    Ok(Some(file))
}

// Compares a source with its copy byte for byte. The copy is read with the
// cache bypassed, so its bytes come from the drive.
fn files_are_identical(first: &Path, second: &Path) -> Result<bool, String> {
    files_are_identical_with_progress(first, second, &mut |_| Ok(()))
}

// As files_are_identical, reporting how many bytes have been compared.
// Returning an error from on_progress stops the comparison.
fn files_are_identical_with_progress(
    first: &Path,
    second: &Path,
    on_progress: &mut dyn FnMut(u64) -> Result<(), String>,
) -> Result<bool, String> {
    let first_file = open_regular_file(first)
        .map_err(|error| format!("Unable to open source for verification: {error}"))?
        .ok_or_else(|| SOURCE_NOT_A_FILE.to_string())?;
    let second_file = open_regular_file(second)
        .map_err(|error| format!("Unable to open copied file for verification: {error}"))?
        .ok_or_else(|| "The copied file is no longer a file.".to_string())?;
    bypass_cache(&second_file)?;

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
    let mut first_buffer = vec![0_u8; 1024 * 1024];
    let mut second_buffer = vec![0_u8; 1024 * 1024];
    let mut compared = 0_u64;

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

        compared += first_count as u64;
        on_progress(compared)?;
    }
}

#[cfg(target_os = "macos")]
fn rename_exclusive(source: &Path, destination: &Path) -> Result<(), String> {
    // Darwin's RENAME_EXCL flag. renamex_np returns EEXIST rather than
    // replacing an existing destination. If the filesystem does not support
    // exclusive rename, the operation fails safely instead of falling back
    // to overwrite semantics.
    const RENAME_EXCL: u32 = 0x00000004;

    unsafe extern "C" {
        fn renamex_np(
            from: *const std::os::raw::c_char,
            to: *const std::os::raw::c_char,
            flags: u32,
        ) -> std::os::raw::c_int;
    }

    let source_c = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| "Temporary path contains an invalid null byte.".to_string())?;
    let destination_c = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| "Destination path contains an invalid null byte.".to_string())?;

    let result = unsafe { renamex_np(source_c.as_ptr(), destination_c.as_ptr(), RENAME_EXCL) };

    if result == 0 {
        return Ok(());
    }

    let error = std::io::Error::last_os_error();

    if error.kind() == std::io::ErrorKind::AlreadyExists {
        return Err(format!(
            "Destination already exists: {}",
            destination.display()
        ));
    }

    Err(format!(
        "Unable to finalise copied file without overwrite: {error}"
    ))
}

#[cfg(not(target_os = "macos"))]
fn rename_exclusive(_source: &Path, _destination: &Path) -> Result<(), String> {
    Err("Exclusive transfer finalisation is not supported on this platform.".to_string())
}

// Carries the source's details onto the copy: permissions, modification and
// creation dates, and extended attributes such as Finder tags. A raw byte copy
// would otherwise date every copy to the moment it was made, which matters
// for media sorted and archived by date.
#[cfg(target_os = "macos")]
fn copy_file_details(source: &Path, copy: &Path) -> Result<(), String> {
    use std::os::macos::fs::FileTimesExt;
    use std::os::raw::{c_char, c_int, c_void};

    const COPYFILE_STAT: u32 = 1 << 1;
    const COPYFILE_XATTR: u32 = 1 << 2;

    unsafe extern "C" {
        fn copyfile(
            from: *const c_char,
            to: *const c_char,
            state: *mut c_void,
            flags: u32,
        ) -> c_int;
    }

    let details_error = |error: std::io::Error| {
        format!("Unable to copy the file's dates, permissions and tags: {error}")
    };

    // Opened before copyfile, which may copy a read-only mode onto the copy
    // and would then stop it being reopened to set its dates.
    let copy_file = fs::OpenOptions::new()
        .write(true)
        .open(copy)
        .map_err(details_error)?;

    let source_c = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| "Source path contains an invalid null byte.".to_string())?;
    let copy_c = CString::new(copy.as_os_str().as_bytes())
        .map_err(|_| "Temporary path contains an invalid null byte.".to_string())?;
    let result = unsafe {
        copyfile(
            source_c.as_ptr(),
            copy_c.as_ptr(),
            std::ptr::null_mut(),
            COPYFILE_STAT | COPYFILE_XATTR,
        )
    };
    if result != 0 {
        return Err(details_error(std::io::Error::last_os_error()));
    }

    // copyfile does not carry the creation date over, so set both dates
    // from the source explicitly.
    let metadata = fs::metadata(source).map_err(details_error)?;
    let mut times = fs::FileTimes::new()
        .set_accessed(metadata.accessed().map_err(details_error)?)
        .set_modified(metadata.modified().map_err(details_error)?);
    if let Ok(created) = metadata.created() {
        times = times.set_created(created);
    }
    copy_file.set_times(times).map_err(details_error)?;
    copy_file.sync_all().map_err(details_error)?;

    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn copy_file_details(_source: &Path, _copy: &Path) -> Result<(), String> {
    Ok(())
}

// The temporary file for one transfer record. It is named from the record
// alone, so recovery after a crash can derive this exact path again and never
// has to guess which hidden files belong to Media Mapper.
fn transfer_temporary_path(
    destination: &Path,
    transfer_id: i64,
    created_at: i64,
) -> Result<PathBuf, String> {
    let parent = destination
        .parent()
        .ok_or_else(|| "Destination has no parent folder.".to_string())?;
    let temporary = parent.join(format!(
        ".mediamapper-transfer-{transfer_id}-{created_at}.partial"
    ));

    if temporary == destination {
        return Err("The destination has the same name as a transfer temporary file.".to_string());
    }

    Ok(temporary)
}

#[cfg(test)]
fn copy_file_verified_with_stage<F>(
    source: &Path,
    destination: &Path,
    temporary: &Path,
    on_verifying: F,
) -> Result<u64, String>
where
    F: FnMut(u64) -> Result<(), String>,
{
    copy_file_verified_with_progress(source, destination, temporary, on_verifying, &mut |_, _| {
        Ok(())
    })
}

// As copy_file_verified_with_stage, also reporting bytes copied and then
// bytes verified as it goes. Returning an error from on_progress stops the
// transfer; its temporary file is removed like any other failure.
fn copy_file_verified_with_progress<F>(
    source: &Path,
    destination: &Path,
    temporary: &Path,
    mut on_verifying: F,
    on_progress: &mut dyn FnMut(TransferStage, u64) -> Result<(), String>,
) -> Result<u64, String>
where
    F: FnMut(u64) -> Result<(), String>,
{
    if destination.exists() {
        return Err(format!(
            "Destination already exists: {}",
            destination.display()
        ));
    }

    if temporary.parent() != destination.parent() || temporary == destination {
        return Err("The temporary file must sit beside the destination.".to_string());
    }

    let parent = destination
        .parent()
        .ok_or_else(|| "Destination has no parent folder.".to_string())?;

    fs::create_dir_all(parent)
        .map_err(|error| format!("Unable to create destination folder: {error}"))?;

    // Set once this call has created the temporary file, so a failure only
    // ever removes a file this call wrote. A file already at that path is
    // never adopted or deleted; create_new refuses it instead. The handle
    // identifies that file for as long as the transfer runs.
    let mut created_temporary = false;
    let mut temporary_handle: Option<fs::File> = None;

    let mut attempt = || -> Result<u64, String> {
        let source_file = open_regular_file(source)
            .map_err(|error| format!("Unable to open source file: {error}"))?
            .ok_or_else(|| SOURCE_NOT_A_FILE.to_string())?;
        let mut reader = BufReader::new(source_file);

        let temporary_file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temporary)
            .map_err(|error| format!("Unable to create temporary destination file: {error}"))?;
        created_temporary = true;
        let handle: &fs::File = temporary_handle.insert(
            temporary_file
                .try_clone()
                .map_err(|error| format!("Unable to track temporary destination file: {error}"))?,
        );
        bypass_cache(&temporary_file)?;

        // An open file keeps accepting writes after its folder is renamed,
        // moved or deleted, so the copy would otherwise only notice a lost
        // destination once it finished. Check at intervals that the
        // temporary path still leads to this file.
        let mut last_check = Instant::now();
        let mut check_destination = |now: bool| -> Result<(), String> {
            if now || last_check.elapsed() >= DESTINATION_CHECK_INTERVAL {
                if !path_leads_to(temporary, handle) {
                    return Err(DESTINATION_UNAVAILABLE.to_string());
                }
                last_check = Instant::now();
            }
            Ok(())
        };
        // Uncached writes go straight to the drive, so write in large chunks.
        let mut writer = BufWriter::with_capacity(1024 * 1024, temporary_file);

        let mut buffer = vec![0_u8; 1024 * 1024];
        let mut copied = 0_u64;
        loop {
            let count = reader
                .read(&mut buffer)
                .map_err(|error| format!("Unable to copy file: {error}"))?;
            if count == 0 {
                break;
            }
            writer
                .write_all(&buffer[..count])
                .map_err(|error| format!("Unable to copy file: {error}"))?;
            copied += count as u64;
            on_progress(TransferStage::Copying, copied)?;
            check_destination(false)?;
        }

        writer
            .flush()
            .map_err(|error| format!("Unable to flush copied file: {error}"))?;

        writer
            .get_ref()
            .sync_all()
            .map_err(|error| format!("Unable to sync copied file: {error}"))?;

        drop(writer);

        check_destination(true)?;
        copy_file_details(source, temporary)?;

        on_verifying(copied)?;

        if !files_are_identical_with_progress(source, temporary, &mut |verified| {
            on_progress(TransferStage::Verifying, verified)?;
            check_destination(false)
        })? {
            return Err("Copied file failed byte-for-byte verification.".to_string());
        }

        // Finalise with no-overwrite semantics. A destination created after
        // preflight or during the copy must never be replaced.
        rename_exclusive(temporary, destination)?;

        Ok(copied)
    };
    let mut result = attempt();

    if let Err(error) = &mut result {
        // A destination lost between checks surfaces as whatever step then
        // failed, such as copying the file's details or the final rename.
        if let Some(handle) = &temporary_handle {
            if error != TRANSFER_CANCELLED && !path_leads_to(temporary, handle) {
                *error = DESTINATION_UNAVAILABLE.to_string();
            }
        }
        match &temporary_handle {
            Some(handle) => remove_created_temporary(temporary, handle),
            None if created_temporary => {
                let _ = fs::remove_file(temporary);
            }
            None => {}
        }
    }

    result
}

// How often a running copy or verification confirms that its destination
// folder is still where it was. Each check is a single metadata lookup.
const DESTINATION_CHECK_INTERVAL: Duration = Duration::from_millis(500);
const DESTINATION_UNAVAILABLE: &str = "The destination became unavailable during the copy.";

// Whether a path still leads to exactly this open file.
fn path_leads_to(path: &Path, file: &fs::File) -> bool {
    use std::os::unix::fs::MetadataExt;

    match (fs::symlink_metadata(path), file.metadata()) {
        (Ok(at_path), Ok(open)) => {
            at_path.file_type().is_file()
                && at_path.dev() == open.dev()
                && at_path.ino() == open.ino()
        }
        _ => false,
    }
}

// Removes the temporary file a failed transfer created. If its folder was
// renamed or moved during the copy, the file is found where it now is. Only
// that same file, still under its temporary name, is ever removed.
fn remove_created_temporary(temporary: &Path, file: &fs::File) {
    if path_leads_to(temporary, file) {
        let _ = fs::remove_file(temporary);
        return;
    }
    if let Some(moved) = current_path_of(file) {
        if moved.file_name() == temporary.file_name() && path_leads_to(&moved, file) {
            let _ = fs::remove_file(moved);
        }
    }
}

#[cfg(target_os = "macos")]
fn current_path_of(file: &fs::File) -> Option<PathBuf> {
    use std::ffi::{CStr, OsStr};
    use std::os::fd::AsRawFd;
    use std::os::raw::{c_char, c_int};

    const F_GETPATH: c_int = 50;
    const MAXPATHLEN: usize = 1024;

    unsafe extern "C" {
        fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    }

    let mut buffer = vec![0 as c_char; MAXPATHLEN];
    if unsafe { fcntl(file.as_raw_fd(), F_GETPATH, buffer.as_mut_ptr()) } == -1 {
        return None;
    }
    let path = unsafe { CStr::from_ptr(buffer.as_ptr()) };
    Some(PathBuf::from(OsStr::from_bytes(path.to_bytes())))
}

#[cfg(not(target_os = "macos"))]
fn current_path_of(_file: &fs::File) -> Option<PathBuf> {
    None
}

#[cfg(test)]
fn copy_file_verified(source: &Path, destination: &Path) -> Result<u64, String> {
    let temporary = transfer_temporary_path(destination, 0, 0)?;
    copy_file_verified_with_stage(source, destination, &temporary, |_| Ok(()))
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

// The window checks for drives every two seconds. Recording each sighting
// would write that often, so a drive's connection is recorded when it is
// first seen in this run of the app and then at most this often while it
// stays connected.
const CONNECTION_RECORD_INTERVAL: Duration = Duration::from_secs(10 * 60);

// When each drive's connection was last recorded in this run of the app.
static RECORDED_CONNECTIONS: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

// Records that known drives are connected now. Drives that were never
// scanned have no catalogue record and are left alone. Only the connection
// time changes, never the scan time.
fn record_drive_connections(
    connection: &Connection,
    drive_ids: &[String],
    connected_at: i64,
) -> Result<(), String> {
    for drive_id in drive_ids {
        connection
            .execute(
                "UPDATE drives
                 SET last_seen_at = MAX(last_seen_at, ?1)
                 WHERE persistent_identifier = ?2",
                params![connected_at, drive_id],
            )
            .map_err(|error| format!("Unable to record connected drive: {error}"))?;
    }
    Ok(())
}

// Best effort: a failure only means the connection is recorded on a later
// check. A scan holds the database's write lock for its whole run, so this
// gives up at once rather than waiting and delaying the drive list.
fn note_connected_drives(database: &Path, drives: &[DriveInfo]) {
    let Ok(mut recorded) = RECORDED_CONNECTIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    else {
        return;
    };
    let due: Vec<String> = drives
        .iter()
        .filter_map(|drive| drive.persistent_identifier.clone())
        .filter(|drive_id| {
            recorded
                .get(drive_id)
                .is_none_or(|at| at.elapsed() >= CONNECTION_RECORD_INTERVAL)
        })
        .collect();
    if due.is_empty() {
        return;
    }

    let result = open_database(database).and_then(|connection| {
        connection
            .busy_timeout(Duration::ZERO)
            .map_err(|error| format!("Unable to configure catalogue database: {error}"))?;
        record_drive_connections(&connection, &due, now_unix())
    });
    if result.is_ok() {
        let now = Instant::now();
        for drive_id in due {
            recorded.insert(drive_id, now);
        }
    }
}

#[tauri::command]
async fn list_external_drives(app: tauri::AppHandle) -> Result<Vec<DriveInfo>, String> {
    run_blocking(move || {
        let drives = external_drives()?;
        if let Ok(database) = database_path(&app) {
            note_connected_drives(&database, &drives);
        }
        Ok(drives)
    })
    .await
}

// Drives are listed most recently scanned first, so the catalogues most likely
// to still match their drives come first. Drives never scanned come last, and
// the name, then the identifier, keep the order stable. The window lists
// connected drives before offline ones, each group in this order.
fn catalogued_drives(connection: &Connection) -> Result<Vec<CataloguedDrive>, String> {
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
                d.last_seen_at,
                d.file_count,
                d.directory_count,
                d.catalogued_bytes,
                d.unreadable_folder_count
            FROM drives d
            ORDER BY
                d.last_scanned_at IS NULL,
                d.last_scanned_at DESC,
                lower(d.name),
                d.persistent_identifier
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
                last_connected_at: row.get(7)?,
                file_count: row.get(8)?,
                directory_count: row.get(9)?,
                catalogued_bytes: row.get(10)?,
                unreadable_folder_count: row.get(11)?,
            })
        })
        .map_err(|error| format!("Unable to read catalogue: {error}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Unable to read catalogue rows: {error}"))
}

#[tauri::command]
async fn list_catalogued_drives(app: tauri::AppHandle) -> Result<Vec<CataloguedDrive>, String> {
    run_blocking(move || {
        let connection = open_database(&database_path(&app)?)?;
        catalogued_drives(&connection)
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

// The scan itself, run on the blocking thread pool. Everything happens in one
// transaction: only a scan that reaches the end commits, and with it the new
// `last_scanned_at`. A failed or cancelled scan rolls back, keeping the
// previous catalogue and its scan time.
fn scan_drive_job(
    database: PathBuf,
    drive: DriveInfo,
    report_progress: impl Fn(&str, &(i64, i64, i64, i64), &str) + Send + 'static,
) -> impl FnOnce() -> Result<ScanResult, String> + Send + 'static {
    move || {
        let drive_id = drive
            .persistent_identifier
            .clone()
            .ok_or_else(|| "This drive does not provide a stable volume identifier.".to_string())?;
        let root = PathBuf::from(&drive.mount_point);

        if !root.is_dir() {
            return Err("The drive mount point is no longer available.".to_string());
        }

        let mut connection = open_database(&database)?;
        // Held for the whole scan, so no transfer can run until it finishes.
        let Some(_transfer_lock) = try_lock_transfers(&connection)? else {
            return Err(
                "A transfer is running. Wait for it to finish, then scan the drive.".to_string(),
            );
        };
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
                    drive
                        .total_bytes
                        .map(|value| value.min(i64::MAX as u64) as i64),
                    drive
                        .available_bytes
                        .map(|value| value.min(i64::MAX as u64) as i64),
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
        report_progress(&drive_id, &counters, "");
        if let Err(error) = scan_directory(
            &root,
            &mut insert_statement,
            &drive_id,
            &mut counters,
            &mut unreadable_folders,
            &|counters, current_path| report_progress(&drive_id, counters, current_path),
            &mut last_emit_at,
        ) {
            clear_scan_cancel(&drive_id);
            if error == SCAN_CANCELLED {
                return Err("Scan cancelled.".to_string());
            }
            return Err(error);
        }
        report_progress(&drive_id, &counters, "");
        clear_scan_cancel(&drive_id);

        drop(insert_statement);

        {
            let mut mark_unreadable = transaction
                .prepare(
                    "UPDATE files SET unreadable = 1 WHERE drive_id = ?1 AND relative_path = ?2",
                )
                .map_err(|error| format!("Unable to mark unreadable folders: {error}"))?;
            for folder in unreadable_folders
                .iter()
                .filter(|folder| !folder.is_empty())
            {
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
    }
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

    tauri::async_runtime::spawn_blocking(scan_drive_job(
        database,
        drive,
        move |drive_id, counters, current_path| {
            emit_scan_progress(&progress_app, drive_id, counters, current_path)
        },
    ))
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
    destination_local_path: Option<String>,
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
                        l.local_path,
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
                    destination_local_path: row.get(6)?,
                    destination_available_bytes: row.get(7)?,
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
                location_id: None,
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
                let available_bytes = match destination_kind.as_str() {
                    "local_folder" => planned
                        .destination_local_path
                        .as_deref()
                        .and_then(|path| free_bytes_at(Path::new(path)).ok())
                        .map(|free| free.saturating_sub(LOCAL_FOLDER_FREE_SPACE_RESERVE))
                        .map(|free| free.min(i64::MAX as u64) as i64),
                    _ => planned.destination_available_bytes,
                };
                (destination_name, destination_kind, 0, 0, 0, available_bytes)
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
                location_id: None,
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
                location_id: None,
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
                location_id: None,
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
                        location_id: Some(location_id.clone()),
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

// Space kept free when copying into a folder on this Mac, so a copy can never
// fill the startup disk, which can leave macOS unstable.
const LOCAL_FOLDER_FREE_SPACE_RESERVE: u64 = 1024 * 1024 * 1024;

// Free bytes on the volume holding `path`, as `df` reports them.
fn free_bytes_at(path: &Path) -> Result<u64, String> {
    let output = std::process::Command::new("/bin/df")
        .args(["-k", "-P"])
        .arg(path)
        .output()
        .map_err(|error| format!("Unable to check free space: {error}"))?;
    if !output.status.success() {
        return Err(format!("Unable to check free space at {}.", path.display()));
    }
    parse_df_available(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| format!("Unable to read free space at {}.", path.display()))
}

// Reads the available space from `df -k -P` output. The value sits just
// before the capacity percentage; counting from there copes with filesystem
// names that contain spaces.
fn parse_df_available(text: &str) -> Option<u64> {
    let line = text.lines().nth(1)?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    let capacity = fields.iter().position(|field| field.ends_with('%'))?;
    let kilobytes: u64 = fields.get(capacity.checked_sub(1)?)?.parse().ok()?;
    kilobytes.checked_mul(1024)
}

// A validation message naming a destination as the user knows it, never by
// its path or location id, such as "The destination “Backup” is not
// currently connected." Without a usable name the destination goes unnamed.
fn destination_message(subject: &str, name: Option<&str>, problem: &str) -> String {
    match name {
        Some(name) if !name.is_empty() => format!("{subject} “{name}” {problem}"),
        _ => format!("{subject} {problem}"),
    }
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
                    l.local_path,
                    COALESCE(NULLIF(l.user_label, ''), l.display_name)
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
                row.get::<_, Option<String>>(7)?,
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
        destination_name,
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
                location_id: None,
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
                            location_id: None,
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
                            location_id: None,
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
                            // Scans never catalogue symbolic links, so a
                            // catalogued file that is now a link (or anything
                            // else but a file) has changed type too. Copying
                            // it would copy whatever the link points to.
                            let file_no_longer_file =
                                !catalogued_is_directory && !metadata.file_type().is_file();

                            if live_is_directory != catalogued_is_directory || file_no_longer_file {
                                issues.push(PlanPreflightIssue {
                                    code: "source_type_changed".to_string(),
                                    message: format!(
                                        "{} has changed type since it was catalogued.",
                                        source_relative_path
                                    ),
                                    move_id: Some(move_id),
                                    location_id: None,
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
                                        location_id: None,
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
                    location_id: None,
                }),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => issues.push(PlanPreflightIssue {
                    code: "destination_unreadable".to_string(),
                    message: format!(
                        "Media Mapper cannot confirm whether {} is clear at the planned destination.",
                        relative_path
                    ),
                    move_id: Some(move_id),
                    location_id: None,
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
                        message: destination_message(
                            "The destination",
                            destination_name.as_deref(),
                            "is not currently connected.",
                        ),
                        move_id: Some(move_id),
                        location_id: None,
                    });
                }
            }
            Some("local_folder") => match destination_local_path.as_deref() {
                Some(path) => {
                    let destination = Path::new(path);
                    if !destination.exists() {
                        issues.push(PlanPreflightIssue {
                            code: "destination_folder_missing".to_string(),
                            message: destination_message(
                                "The destination folder",
                                destination_name.as_deref(),
                                "is no longer available.",
                            ),
                            move_id: Some(move_id),
                            location_id: None,
                        });
                    } else if !destination.is_dir() {
                        issues.push(PlanPreflightIssue {
                            code: "destination_not_folder".to_string(),
                            message: destination_message(
                                "The destination",
                                destination_name.as_deref(),
                                "is no longer a folder.",
                            ),
                            move_id: Some(move_id),
                            location_id: None,
                        });
                    }
                }
                None => issues.push(PlanPreflightIssue {
                    code: "destination_folder_missing".to_string(),
                    message: "The planned local destination no longer has a folder path."
                        .to_string(),
                    move_id: Some(move_id),
                    location_id: None,
                }),
            },
            _ => issues.push(PlanPreflightIssue {
                code: "destination_missing".to_string(),
                message: format!(
                    "Destination {} is no longer available.",
                    destination_location_id
                ),
                move_id: Some(move_id),
                location_id: None,
            }),
        }
    }

    // Capacity is checked once per destination using current free space,
    // rather than the value stored at the last catalogue scan: diskutil for
    // external drives, df for folders on this Mac.
    let preflight = plan_preflight(connection)?;
    for destination in &preflight.destinations {
        let local_folder = destination.kind == "local_folder";
        let available = match destination.kind.as_str() {
            "external_drive" => {
                let drive_id = destination
                    .location_id
                    .strip_prefix("drive:")
                    .unwrap_or(&destination.location_id);
                let Some(drive) = connected_by_id.get(drive_id) else {
                    continue;
                };
                drive.available_bytes
            }
            "local_folder" => {
                let local_path: Option<String> = connection
                    .query_row(
                        "SELECT local_path FROM locations WHERE id = ?1",
                        params![destination.location_id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|error| format!("Unable to read destination folder: {error}"))?
                    .flatten();
                // A missing folder is reported by its own check above.
                let Some(local_path) = local_path else {
                    continue;
                };
                free_bytes_at(Path::new(&local_path))
                    .ok()
                    .map(|free| free.saturating_sub(LOCAL_FOLDER_FREE_SPACE_RESERVE))
            }
            _ => continue,
        };

        if destination.unknown_size_count > 0 {
            issues.push(PlanPreflightIssue {
                code: "live_capacity_unknown".to_string(),
                message: format!(
                    "{} contains planned files with unknown sizes, so current free-space requirements cannot be confirmed.",
                    destination.display_name
                ),
                move_id: None,
                location_id: Some(destination.location_id.clone()),
            });
        } else if let Some(available) = available {
            if destination.known_bytes as u64 > available {
                issues.push(PlanPreflightIssue {
                    code: "live_insufficient_capacity".to_string(),
                    message: if local_folder {
                        format!(
                            "{} does not currently have enough free space for the planned data while keeping 1 GB free on this Mac.",
                            destination.display_name
                        )
                    } else {
                        format!(
                            "{} does not currently have enough free space for the planned data.",
                            destination.display_name
                        )
                    },
                    move_id: None,
                    location_id: Some(destination.location_id.clone()),
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
    run_blocking(move || {
        let drives = external_drives()?;
        let connection = open_database(&database_path(&app)?)?;
        validate_plan_live(&connection, &drives)
    })
    .await
}

#[tauri::command]
async fn list_transfers(app: tauri::AppHandle) -> Result<Vec<TransferRecord>, String> {
    run_blocking(move || {
        let connection = open_database(&database_path(&app)?)?;
        list_transfer_records(&connection)
    })
    .await
}

#[tauri::command]
async fn execute_planned_move(
    app: tauri::AppHandle,
    planned_move_id: i64,
) -> Result<TransferRecord, String> {
    // A cancel requested before this copy began belongs to an earlier one.
    TRANSFER_CANCEL_REQUESTED.store(false, Ordering::SeqCst);

    run_blocking(move || {
        // Discover the physical drives immediately before execution. This is
        // deliberately inside the blocking worker because diskutil and file
        // verification must never block the window thread.
        let drives = external_drives()?;
        let connection = open_database(&database_path(&app)?)?;

        execute_planned_transfer_reporting(
            &connection,
            planned_move_id,
            &drives,
            &|progress| {
                let _ = app.emit("transfer-progress", progress);
            },
            &|| TRANSFER_CANCEL_REQUESTED.load(Ordering::SeqCst),
        )
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

#[tauri::command]
async fn open_catalogued_file(
    app: tauri::AppHandle,
    drive_id: String,
    relative_path: String,
) -> Result<(), String> {
    run_blocking(move || {
        validate_catalogue_relative_path(&relative_path)?;

        let connection = open_database(&database_path(&app)?)?;
        let is_directory: Option<i64> = connection
            .query_row(
                "SELECT is_directory FROM files WHERE drive_id = ?1 AND relative_path = ?2",
                params![drive_id, relative_path],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("Unable to check the catalogue entry: {error}"))?;

        match is_directory {
            Some(0) => {}
            Some(_) => return Err("That catalogue entry is a folder.".to_string()),
            None => return Err("That file is no longer in the catalogue.".to_string()),
        }

        let drives = external_drives()?;
        let drive = drives
            .iter()
            .find(|drive| drive.persistent_identifier.as_deref() == Some(drive_id.as_str()))
            .ok_or_else(|| "Connect this drive to open the file.".to_string())?;

        let path = resolve_catalogued_file(Path::new(&drive.mount_point), &relative_path)?;

        // Any format goes to the same opener; which app handles it, and whether
        // one can, is macOS's file association. `open` returns once Launch
        // Services has accepted or refused the file, so waiting is brief.
        let output = std::process::Command::new("/usr/bin/open")
            .arg(&path)
            .output()
            .map_err(|_| "Unable to ask macOS to open the file.".to_string())?;
        if !output.status.success() {
            return Err(open_failure_message(&String::from_utf8_lossy(
                &output.stderr,
            )));
        }

        Ok(())
    })
    .await
}

// The physical file a catalogue entry names on a mounted drive. Resolving
// symlinks and requiring the result to stay on the drive means a link inside
// the catalogue cannot hand the opener a file somewhere else.
fn resolve_catalogued_file(mount_point: &Path, relative_path: &str) -> Result<PathBuf, String> {
    let unavailable =
        || "The file is not currently available at its catalogued location.".to_string();
    let mount = fs::canonicalize(mount_point).map_err(|_| unavailable())?;
    let path = fs::canonicalize(mount.join(relative_path)).map_err(|_| unavailable())?;
    if !path.starts_with(&mount) {
        return Err("That file points outside its drive, so it was not opened.".to_string());
    }
    if !path.is_file() {
        return Err(unavailable());
    }
    Ok(path)
}

// Turns `open`'s stderr into something to show the user. Launch Services
// reports a missing app as "No application knows how to open ..." (-10814).
fn open_failure_message(stderr: &str) -> String {
    if stderr.contains("No application knows how to open") || stderr.contains("-10814") {
        "No app on this Mac can open this type of file.".to_string()
    } else {
        "macOS couldn't open this file.".to_string()
    }
}

// Where the file a completed transfer copied is now. Built from the stored
// destination location and path, so it only succeeds while that location is
// available and the file has not been moved or deleted since.
fn completed_transfer_file(
    connection: &Connection,
    transfer_id: i64,
    connected_drives: &[DriveInfo],
) -> Result<PathBuf, String> {
    let transfer = connection
        .query_row(
            "SELECT t.status, t.destination_relative_path, l.kind, l.drive_id, l.local_path
             FROM transfers t
             LEFT JOIN locations l ON l.id = t.destination_location_id
             WHERE t.id = ?1",
            params![transfer_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|error| format!("Unable to read the transfer: {error}"))?;

    let Some((status, relative_path, kind, drive_id, local_path)) = transfer else {
        return Err("That transfer is no longer in the history.".to_string());
    };
    if status != "completed" {
        return Err("Only completed transfers can be shown in Finder.".to_string());
    }

    let not_found =
        || "The copied file is no longer at its destination. It may have been moved or deleted.";
    validate_catalogue_relative_path(&relative_path).map_err(|_| not_found().to_string())?;

    let Some(root) = connected_location_root(
        kind.as_deref(),
        drive_id.as_deref(),
        local_path.as_deref(),
        connected_drives,
    ) else {
        return Err(if kind.as_deref() == Some("external_drive") {
            "Connect the destination drive to show this file in Finder.".to_string()
        } else {
            not_found().to_string()
        });
    };

    let path = root.join(&relative_path);
    // `open -R` is given this path as an argument, so it must never read as
    // an option.
    if !path.is_absolute() {
        return Err(not_found().to_string());
    }
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(path),
        _ => Err(not_found().to_string()),
    }
}

#[tauri::command]
async fn reveal_transferred_file(app: tauri::AppHandle, transfer_id: i64) -> Result<(), String> {
    run_blocking(move || {
        let drives = external_drives()?;
        let connection = open_database(&database_path(&app)?)?;
        let path = completed_transfer_file(&connection, transfer_id, &drives)?;

        // Selects the file in a Finder window rather than opening it.
        std::process::Command::new("/usr/bin/open")
            .arg("-R")
            .arg(&path)
            .spawn()
            .map_err(|_| "Unable to show the file in Finder.".to_string())?;

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

            if let Err(error) = recover_transfers_at_startup(app.handle()) {
                eprintln!("Media Mapper transfer recovery failed: {error}");
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
            cancel_transfer,
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
            execute_planned_move,
            list_transfers,
            list_planned_folder_entries,
            remove_planned_move,
            open_catalogued_file,
            reveal_transferred_file
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
    fn catalogued_files_resolve_only_to_files_on_their_drive() {
        let root = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-open-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let _ = fs::remove_dir_all(&root.0);
        let volume = root.0.join("Drive");
        fs::create_dir_all(volume.join("Films")).unwrap();
        fs::write(volume.join("Films/Down.by.Law.1986.mkv"), b"film").unwrap();
        fs::write(root.0.join("outside.txt"), b"elsewhere").unwrap();
        std::os::unix::fs::symlink(root.0.join("outside.txt"), volume.join("escape.txt")).unwrap();
        std::os::unix::fs::symlink(
            volume.join("Films/Down.by.Law.1986.mkv"),
            volume.join("alias.mkv"),
        )
        .unwrap();

        let resolved = resolve_catalogued_file(&volume, "Films/Down.by.Law.1986.mkv").unwrap();
        assert!(resolved.ends_with("Drive/Films/Down.by.Law.1986.mkv"));
        assert!(resolve_catalogued_file(&volume, "alias.mkv").is_ok());

        assert_eq!(
            resolve_catalogued_file(&volume, "escape.txt").unwrap_err(),
            "That file points outside its drive, so it was not opened."
        );
        for unavailable in ["Films", "Films/missing.mov"] {
            assert_eq!(
                resolve_catalogued_file(&volume, unavailable).unwrap_err(),
                "The file is not currently available at its catalogued location."
            );
        }
    }

    #[test]
    fn open_failures_become_friendly_messages() {
        assert_eq!(
            open_failure_message(
                "No application knows how to open URL file:///Volumes/Drive/a.xyz (Error Domain=NSOSStatusErrorDomain Code=-10814)"
            ),
            "No app on this Mac can open this type of file."
        );
        assert_eq!(
            open_failure_message("something else"),
            "macOS couldn't open this file."
        );
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
        assert_eq!(local.destinations[0].kind, "local_folder");
        match local.destinations[0].available_bytes {
            Some(available) => {
                assert_eq!(
                    local.destinations[0].projected_available_bytes,
                    Some(available.saturating_sub(25))
                );
                assert_eq!(
                    local.destinations[0].capacity_sufficient,
                    Some(25 <= available)
                );
            }
            None => {
                assert_eq!(local.destinations[0].projected_available_bytes, None);
                assert_eq!(local.destinations[0].capacity_sufficient, None);
            }
        }
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
        let offline = result
            .issues
            .iter()
            .find(|issue| issue.code == "destination_drive_offline")
            .unwrap();
        assert_eq!(
            offline.message,
            "The destination “Backup” is not currently connected."
        );
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
        let issue = missing
            .issues
            .iter()
            .find(|issue| issue.code == "destination_folder_missing")
            .unwrap();
        assert_eq!(
            issue.message,
            "The destination folder “Local” is no longer available."
        );

        fs::write(&local_path, b"not a folder").unwrap();
        let not_folder = validate_plan_live(&connection, &drives).unwrap();
        let issue = not_folder
            .issues
            .iter()
            .find(|issue| issue.code == "destination_not_folder")
            .unwrap();
        assert_eq!(
            issue.message,
            "The destination “Local” is no longer a folder."
        );
        fs::remove_file(&local_path).unwrap();

        fs::create_dir_all(&local_path).unwrap();

        let valid = validate_plan_live(&connection, &drives).unwrap();
        assert!(valid.ready, "{:?}", valid.issues);

        fs::remove_dir_all(&local_path).unwrap();
    }

    #[test]
    fn destination_messages_never_fall_back_to_a_path_or_id() {
        for name in [None, Some("")] {
            assert_eq!(
                destination_message("The destination", name, "is no longer a folder."),
                "The destination is no longer a folder."
            );
            assert_eq!(
                destination_message("The destination", name, "is not currently connected."),
                "The destination is not currently connected."
            );
            assert_eq!(
                destination_message("The destination folder", name, "is no longer available."),
                "The destination folder is no longer available."
            );
        }
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

    // The fraction of a file's pages currently held in the memory cache.
    #[cfg(target_os = "macos")]
    fn cached_fraction(path: &Path) -> f64 {
        use std::os::fd::AsRawFd;
        use std::os::raw::{c_char, c_int, c_void};

        unsafe extern "C" {
            fn mmap(
                addr: *mut c_void,
                len: usize,
                prot: c_int,
                flags: c_int,
                fd: c_int,
                offset: i64,
            ) -> *mut c_void;
            fn mincore(addr: *const c_void, len: usize, vec: *mut c_char) -> c_int;
            fn munmap(addr: *mut c_void, len: usize) -> c_int;
            fn getpagesize() -> c_int;
        }
        const PROT_READ: c_int = 1;
        const MAP_SHARED: c_int = 1;

        let file = fs::File::open(path).unwrap();
        let len = file.metadata().unwrap().len() as usize;
        let page = unsafe { getpagesize() } as usize;
        let pages = len.div_ceil(page);
        let mut residency = vec![0 as c_char; pages];
        unsafe {
            let mapping = mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ,
                MAP_SHARED,
                file.as_raw_fd(),
                0,
            );
            assert_ne!(mapping as isize, -1, "mmap failed");
            assert_eq!(mincore(mapping, len, residency.as_mut_ptr()), 0);
            munmap(mapping, len);
        }
        residency.iter().filter(|page| **page & 1 != 0).count() as f64 / pages as f64
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn verified_copy_is_checked_against_the_drive_not_memory() {
        let volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-uncached-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let _ = fs::remove_dir_all(&volume.0);
        fs::create_dir_all(&volume.0).unwrap();
        let source = volume.0.join("clip.mov");
        let destination = volume.0.join("Archive/clip.mov");
        let contents: Vec<u8> = (0..32 * 1024 * 1024_u32).map(|i| (i % 251) as u8).collect();
        fs::write(&source, &contents).unwrap();

        // Control: a file written normally stays cached, so the measurement
        // can see cached pages.
        assert!(cached_fraction(&source) > 0.9);

        copy_file_verified(&source, &destination).unwrap();

        // Neither writing the copy nor verifying it left it cached, so the
        // verification read the copy from the drive. Measured before anything
        // else reads the copy, which would cache it again.
        assert!(
            cached_fraction(&destination) < 0.05,
            "the verified copy should not be held in memory"
        );
        assert_eq!(fs::read(&destination).unwrap(), contents);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn verified_copy_keeps_dates_permissions_and_finder_tags() {
        use std::os::macos::fs::{FileTimesExt, MetadataExt};
        use std::os::unix::fs::PermissionsExt;

        let volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-details-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let _ = fs::remove_dir_all(&volume.0);
        fs::create_dir_all(&volume.0).unwrap();
        let source = volume.0.join("clip.mov");
        let destination = volume.0.join("Archive/clip.mov");
        fs::write(&source, vec![7_u8; 4096]).unwrap();

        // Created 1 Jan 2019, modified 1 Jan 2020, tagged Red, read-only.
        let created = UNIX_EPOCH + Duration::from_secs(1_546_300_800);
        let modified = UNIX_EPOCH + Duration::from_secs(1_577_836_800);
        fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_created(created)
                    .set_modified(modified)
                    .set_accessed(modified),
            )
            .unwrap();
        let tagged = std::process::Command::new("xattr")
            .args(["-w", "com.apple.metadata:_kMDItemUserTags", "Red"])
            .arg(&source)
            .status()
            .unwrap();
        assert!(tagged.success());
        fs::set_permissions(&source, fs::Permissions::from_mode(0o444)).unwrap();

        copy_file_verified(&source, &destination).unwrap();

        let copied = fs::metadata(&destination).unwrap();
        assert_eq!(copied.modified().unwrap(), modified);
        assert_eq!(copied.created().unwrap(), created);
        assert_eq!(copied.st_mode() & 0o777, 0o444);
        let tag = std::process::Command::new("xattr")
            .args(["-p", "com.apple.metadata:_kMDItemUserTags"])
            .arg(&destination)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&tag.stdout).trim(), "Red");

        // The original is untouched.
        assert_eq!(fs::metadata(&source).unwrap().modified().unwrap(), modified);

        // Let the test folder be removed.
        for path in [&source, &destination] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
        }
    }

    #[test]
    fn verified_copy_reports_verifying_after_full_copy() {
        let volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-copy-stage-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let _ = fs::remove_dir_all(&volume.0);
        fs::create_dir_all(&volume.0).unwrap();

        let source = volume.0.join("source.bin");
        let destination = volume.0.join("destination.bin");
        let contents = vec![0x31_u8; 1024 * 1024 + 29];

        fs::write(&source, &contents).unwrap();

        let mut verifying_bytes = None;

        let temporary = transfer_temporary_path(&destination, 1, 1).unwrap();
        let copied = copy_file_verified_with_stage(&source, &destination, &temporary, |bytes| {
            verifying_bytes = Some(bytes);
            Ok(())
        })
        .unwrap();

        assert_eq!(copied, contents.len() as u64);
        assert_eq!(verifying_bytes, Some(contents.len() as u64));
        assert_eq!(fs::read(&destination).unwrap(), contents);
        assert!(source.exists());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn exclusive_rename_never_replaces_existing_destination() {
        let volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-exclusive-rename-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let _ = fs::remove_dir_all(&volume.0);
        fs::create_dir_all(&volume.0).unwrap();

        let temporary = volume.0.join("temporary.partial");
        let destination = volume.0.join("destination.mov");

        fs::write(&temporary, b"new contents").unwrap();
        fs::write(&destination, b"existing contents").unwrap();

        let error = rename_exclusive(&temporary, &destination).unwrap_err();

        assert!(error.contains("Destination already exists"), "{error}");
        assert_eq!(fs::read(&destination).unwrap(), b"existing contents");
        assert_eq!(fs::read(&temporary).unwrap(), b"new contents");
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
    fn planned_transfer_executes_only_after_final_live_validation() {
        let database = TestDatabase::new("planned-transfer-valid");
        let source_volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-gated-source-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let destination_volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-gated-destination-{}-{}",
            std::process::id(),
            now_unix()
        )));

        let _ = fs::remove_dir_all(&source_volume.0);
        let _ = fs::remove_dir_all(&destination_volume.0);
        fs::create_dir_all(&source_volume.0).unwrap();
        fs::create_dir_all(&destination_volume.0).unwrap();

        let source_path = source_volume.0.join("film.mov");
        let contents = b"verified transfer contents";
        fs::write(&source_path, contents).unwrap();

        let metadata = fs::metadata(&source_path).unwrap();
        let modified_at = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_secs() as i64);

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
                    size_bytes,
                    modified_at
                 ) VALUES ('UUID-A', 'film.mov', 'film.mov', '', 0, ?1, ?2)",
                params![contents.len() as i64, modified_at],
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

        let drives = vec![
            test_drive("UUID-A", "Source", &source_volume.0, 10_000),
            test_drive("UUID-B", "Backup", &destination_volume.0, 10_000),
        ];

        let transfer = execute_planned_transfer(&connection, move_id, &drives).unwrap();

        assert_eq!(transfer.status, "completed");
        assert_eq!(
            fs::read(destination_volume.0.join("Archive/film.mov")).unwrap(),
            contents
        );
        assert_eq!(fs::read(&source_path).unwrap(), contents);
    }

    fn insert_transfer_record(
        connection: &Connection,
        location_id: &str,
        relative_path: &str,
        status: &str,
        error_message: Option<&str>,
    ) -> i64 {
        connection
            .execute(
                "INSERT INTO transfers (
                    source_drive_id,
                    source_relative_path,
                    destination_location_id,
                    destination_relative_path,
                    status,
                    error_message,
                    created_at
                 ) VALUES ('UUID-A', 'film.mov', ?1, ?2, ?3, ?4, 0)",
                params![location_id, relative_path, status, error_message],
            )
            .unwrap();
        connection.last_insert_rowid()
    }

    #[test]
    fn completed_transfer_file_is_the_exact_destination_file() {
        let database = TestDatabase::new("reveal-drive");
        let volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-reveal-drive-{}-{}",
            std::process::id(),
            now_unix()
        )));
        fs::create_dir_all(volume.0.join("Archive")).unwrap();
        fs::write(volume.0.join("Archive/film.mov"), b"film").unwrap();

        let connection = open_database(&database.0).unwrap();
        insert_drive(&connection, "UUID-B", "Backup");
        sync_drive_locations(&connection).unwrap();
        let drives = vec![test_drive("UUID-B", "Backup", &volume.0, 10_000)];

        let completed = insert_transfer_record(
            &connection,
            "drive:UUID-B",
            "Archive/film.mov",
            "completed",
            None,
        );
        assert_eq!(
            completed_transfer_file(&connection, completed, &drives).unwrap(),
            volume.0.join("Archive/film.mov")
        );

        // The drive is not connected.
        let error = completed_transfer_file(&connection, completed, &[]).unwrap_err();
        assert_eq!(
            error,
            "Connect the destination drive to show this file in Finder."
        );

        // The file was moved or deleted after the transfer.
        fs::rename(volume.0.join("Archive/film.mov"), volume.0.join("film.mov")).unwrap();
        let error = completed_transfer_file(&connection, completed, &drives).unwrap_err();
        assert!(error.contains("moved or deleted"), "{error}");

        // A folder now at that path is not the copied file.
        fs::create_dir(volume.0.join("Archive/film.mov")).unwrap();
        let error = completed_transfer_file(&connection, completed, &drives).unwrap_err();
        assert!(error.contains("moved or deleted"), "{error}");
    }

    #[test]
    fn only_completed_transfers_have_a_file_to_show() {
        let database = TestDatabase::new("reveal-status");
        let volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-reveal-status-{}-{}",
            std::process::id(),
            now_unix()
        )));
        fs::create_dir_all(&volume.0).unwrap();
        // Even with a file at the destination path, only a completed
        // transfer put it there.
        fs::write(volume.0.join("film.mov"), b"film").unwrap();

        let connection = open_database(&database.0).unwrap();
        insert_drive(&connection, "UUID-B", "Backup");
        sync_drive_locations(&connection).unwrap();
        let drives = vec![test_drive("UUID-B", "Backup", &volume.0, 10_000)];

        let failed = insert_transfer_record(
            &connection,
            "drive:UUID-B",
            "film.mov",
            "failed",
            Some("Verification failed."),
        );
        let cancelled = insert_transfer_record(
            &connection,
            "drive:UUID-B",
            "film.mov",
            "failed",
            Some(TRANSFER_CANCELLED),
        );
        let copying =
            insert_transfer_record(&connection, "drive:UUID-B", "film.mov", "copying", None);

        for transfer in [failed, cancelled, copying] {
            assert_eq!(
                completed_transfer_file(&connection, transfer, &drives).unwrap_err(),
                "Only completed transfers can be shown in Finder."
            );
        }
        assert_eq!(
            completed_transfer_file(&connection, 9_999, &drives).unwrap_err(),
            "That transfer is no longer in the history."
        );
    }

    #[test]
    fn completed_transfer_file_resolves_local_folder_destinations() {
        let database = TestDatabase::new("reveal-folder");
        let folder = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-reveal-folder-{}-{}",
            std::process::id(),
            now_unix()
        )));
        fs::create_dir_all(&folder.0).unwrap();
        fs::write(folder.0.join("film.mov"), b"film").unwrap();

        let connection = open_database(&database.0).unwrap();
        connection
            .execute(
                "INSERT INTO locations (id, kind, display_name, drive_id, local_path, created_at)
                 VALUES ('folder:test', 'local_folder', 'Films', NULL, ?1, 0)",
                params![folder.0.to_string_lossy()],
            )
            .unwrap();

        let completed =
            insert_transfer_record(&connection, "folder:test", "film.mov", "completed", None);
        assert_eq!(
            completed_transfer_file(&connection, completed, &[]).unwrap(),
            folder.0.join("film.mov")
        );

        fs::remove_file(folder.0.join("film.mov")).unwrap();
        let error = completed_transfer_file(&connection, completed, &[]).unwrap_err();
        assert!(error.contains("moved or deleted"), "{error}");
    }

    #[test]
    fn only_issues_about_a_move_or_its_destination_block_it() {
        let issue = |move_id: Option<i64>, location_id: Option<&str>| PlanPreflightIssue {
            code: "test".to_string(),
            message: "test".to_string(),
            move_id,
            location_id: location_id.map(str::to_string),
        };

        // About this move, or another move.
        assert!(issue_blocks_move(&issue(Some(1), None), 1, "drive:B"));
        assert!(!issue_blocks_move(&issue(Some(2), None), 1, "drive:B"));
        // About this move's destination, or another destination.
        assert!(issue_blocks_move(
            &issue(None, Some("drive:B")),
            1,
            "drive:B"
        ));
        assert!(!issue_blocks_move(
            &issue(None, Some("drive:C")),
            1,
            "drive:B"
        ));
        // About nothing in particular: blocks everything, to be safe.
        assert!(issue_blocks_move(&issue(None, None), 1, "drive:B"));
    }

    #[test]
    fn a_problem_with_one_planned_move_does_not_block_the_others() {
        let database = TestDatabase::new("independent-moves");
        let volume = |name: &str| {
            TestVolume(std::env::temp_dir().join(format!(
                "media-mapper-independent-{name}-{}-{}",
                std::process::id(),
                now_unix()
            )))
        };
        let source_volume = volume("source");
        let destination_volume = volume("destination");
        for folder in [&source_volume.0, &destination_volume.0] {
            let _ = fs::remove_dir_all(folder);
            fs::create_dir_all(folder).unwrap();
        }
        let source_path = source_volume.0.join("film.mov");
        fs::write(&source_path, b"film").unwrap();
        let modified_at = system_time_unix(fs::metadata(&source_path).unwrap().modified());

        let mut connection = open_database(&database.0).unwrap();
        insert_drive(&connection, "UUID-A", "Source");
        insert_drive(&connection, "UUID-B", "Backup");
        insert_drive(&connection, "UUID-C", "Unplugged");
        sync_drive_locations(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO files (drive_id, relative_path, name, parent_path, is_directory, size_bytes, modified_at)
                 VALUES ('UUID-A', 'film.mov', 'film.mov', '', 0, 4, ?1),
                        ('UUID-C', 'clip.mov', 'clip.mov', '', 0, 4, ?1)",
                params![modified_at],
            )
            .unwrap();
        let ready = plan_move(
            &mut connection,
            "UUID-A",
            "film.mov",
            "drive:UUID-B",
            "film.mov",
        )
        .unwrap();
        let unplugged = plan_move(
            &mut connection,
            "UUID-C",
            "clip.mov",
            "drive:UUID-B",
            "clip.mov",
        )
        .unwrap();

        // The drive holding the second plan's source is not connected.
        let drives = vec![
            test_drive("UUID-A", "Source", &source_volume.0, 10_000),
            test_drive("UUID-B", "Backup", &destination_volume.0, 10_000),
        ];

        let transfer = execute_planned_transfer(&connection, ready, &drives)
            .expect("an unrelated offline drive must not block this copy");
        assert_eq!(transfer.status, "completed");
        assert_eq!(
            fs::read(destination_volume.0.join("film.mov")).unwrap(),
            b"film"
        );

        let error = execute_planned_transfer(&connection, unplugged, &drives).unwrap_err();
        assert!(error.contains("clip.mov"), "{error}");
        assert!(!destination_volume.0.join("clip.mov").exists());
    }

    #[test]
    fn planned_transfer_refuses_offline_destination_before_creating_record() {
        let database = TestDatabase::new("planned-transfer-offline");
        let source_volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-gated-offline-{}-{}",
            std::process::id(),
            now_unix()
        )));

        let _ = fs::remove_dir_all(&source_volume.0);
        fs::create_dir_all(&source_volume.0).unwrap();

        let source_path = source_volume.0.join("film.mov");
        fs::write(&source_path, b"source").unwrap();

        let metadata = fs::metadata(&source_path).unwrap();
        let modified_at = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_secs() as i64);

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
                    size_bytes,
                    modified_at
                 ) VALUES ('UUID-A', 'film.mov', 'film.mov', '', 0, 6, ?1)",
                params![modified_at],
            )
            .unwrap();

        let move_id = plan_move(
            &mut connection,
            "UUID-A",
            "film.mov",
            "drive:UUID-B",
            "film.mov",
        )
        .unwrap();

        let drives = vec![test_drive("UUID-A", "Source", &source_volume.0, 10_000)];

        let error = execute_planned_transfer(&connection, move_id, &drives).unwrap_err();

        assert!(error.contains("final validation"), "{error}");

        assert!(list_transfer_records(&connection).unwrap().is_empty());
        assert_eq!(fs::read(&source_path).unwrap(), b"source");
    }

    #[test]
    fn planned_transfer_refuses_changed_source_before_creating_record() {
        let database = TestDatabase::new("planned-transfer-changed");
        let source_volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-gated-changed-source-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let destination_volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-gated-changed-destination-{}-{}",
            std::process::id(),
            now_unix()
        )));

        let _ = fs::remove_dir_all(&source_volume.0);
        let _ = fs::remove_dir_all(&destination_volume.0);
        fs::create_dir_all(&source_volume.0).unwrap();
        fs::create_dir_all(&destination_volume.0).unwrap();

        let source_path = source_volume.0.join("film.mov");
        fs::write(&source_path, b"changed contents").unwrap();

        let mut connection = open_database(&database.0).unwrap();

        insert_drive(&connection, "UUID-A", "Source");
        insert_drive(&connection, "UUID-B", "Backup");
        sync_drive_locations(&connection).unwrap();

        // Deliberately stale catalogue size. The live file is larger.
        connection
            .execute(
                "INSERT INTO files (
                    drive_id,
                    relative_path,
                    name,
                    parent_path,
                    is_directory,
                    size_bytes,
                    modified_at
                 ) VALUES ('UUID-A', 'film.mov', 'film.mov', '', 0, 3, NULL)",
                [],
            )
            .unwrap();

        let move_id = plan_move(
            &mut connection,
            "UUID-A",
            "film.mov",
            "drive:UUID-B",
            "film.mov",
        )
        .unwrap();

        let drives = vec![
            test_drive("UUID-A", "Source", &source_volume.0, 10_000),
            test_drive("UUID-B", "Backup", &destination_volume.0, 10_000),
        ];

        let error = execute_planned_transfer(&connection, move_id, &drives).unwrap_err();

        assert!(error.contains("final validation"), "{error}");

        assert!(list_transfer_records(&connection).unwrap().is_empty());
        assert!(!destination_volume.0.join("film.mov").exists());
        assert_eq!(fs::read(&source_path).unwrap(), b"changed contents");
    }

    #[test]
    fn transfer_paths_use_current_external_drive_mount_points() {
        let database = TestDatabase::new("transfer-paths-external");
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
                 ) VALUES ('UUID-A', 'Films/source.mov', 'source.mov', 'Films', 0, 10)",
                [],
            )
            .unwrap();

        let move_id = plan_move(
            &mut connection,
            "UUID-A",
            "Films/source.mov",
            "drive:UUID-B",
            "Archive/source.mov",
        )
        .unwrap();

        let drives = [
            test_drive("UUID-A", "Source", Path::new("/Volumes/Source-New"), 1_000),
            test_drive("UUID-B", "Backup", Path::new("/Volumes/Backup-New"), 1_000),
        ];

        let (source, destination) = resolve_transfer_paths(&connection, move_id, &drives).unwrap();

        assert_eq!(
            source,
            PathBuf::from("/Volumes/Source-New/Films/source.mov")
        );
        assert_eq!(
            destination,
            PathBuf::from("/Volumes/Backup-New/Archive/source.mov")
        );
    }

    #[test]
    fn transfer_paths_resolve_local_folder_destination() {
        let database = TestDatabase::new("transfer-paths-local");
        let mut connection = open_database(&database.0).unwrap();

        insert_drive(&connection, "UUID-A", "Source");
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
                 ) VALUES ('UUID-A', 'source.mov', 'source.mov', '', 0, 10)",
                [],
            )
            .unwrap();

        connection
            .execute(
                "INSERT INTO locations (
                    id,
                    kind,
                    display_name,
                    local_path,
                    created_at
                 ) VALUES (
                    'local:test',
                    'local_folder',
                    'Local Test',
                    '/Users/test/Media',
                    1
                 )",
                [],
            )
            .unwrap();

        let move_id = plan_move(
            &mut connection,
            "UUID-A",
            "source.mov",
            "local:test",
            "Archive/source.mov",
        )
        .unwrap();

        let drives = [test_drive(
            "UUID-A",
            "Source",
            Path::new("/Volumes/Source"),
            1_000,
        )];

        let (source, destination) = resolve_transfer_paths(&connection, move_id, &drives).unwrap();

        assert_eq!(source, PathBuf::from("/Volumes/Source/source.mov"));
        assert_eq!(
            destination,
            PathBuf::from("/Users/test/Media/Archive/source.mov")
        );
    }

    // A planned move from a 5 MB source file, ready to execute.
    struct ProgressFixture {
        database: TestDatabase,
        volume: TestVolume,
        connection: Connection,
        move_id: i64,
        source: PathBuf,
        destination: PathBuf,
        contents: Vec<u8>,
    }

    fn progress_fixture(name: &str) -> ProgressFixture {
        let database = TestDatabase::new(name);
        let volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-{name}-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let _ = fs::remove_dir_all(&volume.0);
        fs::create_dir_all(&volume.0).unwrap();
        let source = volume.0.join("clip.mov");
        let destination = volume.0.join("Archive/clip.mov");
        let contents: Vec<u8> = (0..5 * 1024 * 1024_u32).map(|i| (i % 249) as u8).collect();
        fs::write(&source, &contents).unwrap();

        let mut connection = open_database(&database.0).unwrap();
        insert_drive(&connection, "UUID-A", "Source");
        insert_drive(&connection, "UUID-B", "Backup");
        sync_drive_locations(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO files (drive_id, relative_path, name, parent_path, is_directory, size_bytes)
                 VALUES ('UUID-A', 'clip.mov', 'clip.mov', '', 0, ?1)",
                params![contents.len() as i64],
            )
            .unwrap();
        let move_id = plan_move(
            &mut connection,
            "UUID-A",
            "clip.mov",
            "drive:UUID-B",
            "Archive/clip.mov",
        )
        .unwrap();

        ProgressFixture {
            database,
            volume,
            connection,
            move_id,
            source,
            destination,
            contents,
        }
    }

    #[test]
    fn transfer_reports_copy_then_verify_progress() {
        let fixture = progress_fixture("transfer-progress");
        let reports = std::cell::RefCell::new(Vec::new());

        let transfer = execute_transfer_paths_reporting(
            &fixture.connection,
            fixture.move_id,
            &fixture.source,
            &fixture.destination,
            &|progress| reports.borrow_mut().push(progress.clone()),
            &|| false,
        )
        .unwrap();
        assert_eq!(transfer.status, "completed");

        let reports = reports.into_inner();
        let total = fixture.contents.len() as u64;
        assert!(
            reports
                .iter()
                .all(|report| report.total_bytes == total
                    && report.planned_move_id == fixture.move_id)
        );

        // Copying first, then verifying, each rising to the full size.
        let split = reports
            .iter()
            .position(|report| report.stage == TransferStage::Verifying)
            .expect("verification progress was reported");
        let (copying, verifying) = reports.split_at(split);
        assert!(!copying.is_empty());
        assert!(copying
            .iter()
            .all(|report| report.stage == TransferStage::Copying));
        assert!(verifying
            .iter()
            .all(|report| report.stage == TransferStage::Verifying));
        for stage in [copying, verifying] {
            assert!(stage.windows(2).all(|pair| pair[0].bytes <= pair[1].bytes));
            assert_eq!(stage.last().unwrap().bytes, total);
        }
    }

    #[test]
    fn cancelled_transfer_leaves_no_copy_and_keeps_the_plan() {
        let fixture = progress_fixture("transfer-cancel");

        // Cancel once the copy is under way.
        let checks = std::cell::Cell::new(0);
        let error = execute_transfer_paths_reporting(
            &fixture.connection,
            fixture.move_id,
            &fixture.source,
            &fixture.destination,
            &|_| {},
            &|| {
                checks.set(checks.get() + 1);
                checks.get() > 2
            },
        )
        .unwrap_err();

        assert_eq!(error, TRANSFER_CANCELLED);
        assert!(!fixture.destination.exists());
        let leftovers: Vec<_> = fs::read_dir(fixture.destination.parent().unwrap())
            .unwrap()
            .collect();
        assert!(leftovers.is_empty(), "the temporary file was removed");
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);

        let history = list_transfer_records(&fixture.connection).unwrap();
        assert_eq!(history[0].status, "failed");
        assert_eq!(
            history[0].error_message.as_deref(),
            Some(TRANSFER_CANCELLED)
        );
        let planned: i64 = fixture
            .connection
            .query_row("SELECT COUNT(*) FROM planned_moves", [], |row| row.get(0))
            .unwrap();
        assert_eq!(planned, 1, "a cancelled copy stays planned");
        let _ = (&fixture.database, &fixture.volume);
    }

    // Runs the fixture's transfer, calling `lose` with the destination folder
    // at the first progress report of `stage`, then waiting long enough for
    // the next destination check to be due.
    fn transfer_losing_destination(
        fixture: &ProgressFixture,
        stage: TransferStage,
        lose: &dyn Fn(&Path),
    ) -> (String, Vec<TransferProgress>) {
        let reports = std::cell::RefCell::new(Vec::new());
        let error = execute_transfer_paths_reporting(
            &fixture.connection,
            fixture.move_id,
            &fixture.source,
            &fixture.destination,
            &|progress| {
                let first = !reports
                    .borrow()
                    .iter()
                    .any(|report: &TransferProgress| report.stage == stage);
                reports.borrow_mut().push(progress.clone());
                if first && progress.stage == stage {
                    lose(fixture.destination.parent().unwrap());
                    std::thread::sleep(DESTINATION_CHECK_INTERVAL + Duration::from_millis(100));
                }
            },
            &|| false,
        )
        .unwrap_err();
        (error, reports.into_inner())
    }

    fn assert_stopped_for_lost_destination(fixture: &ProgressFixture, error: &str) {
        assert_eq!(error, DESTINATION_UNAVAILABLE);
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
        let history = list_transfer_records(&fixture.connection).unwrap();
        assert_eq!(history[0].status, "failed");
        assert_eq!(
            history[0].error_message.as_deref(),
            Some(DESTINATION_UNAVAILABLE)
        );
        assert!(planned_move_exists(&fixture.connection, fixture.move_id));
    }

    #[test]
    fn destination_renamed_during_copy_stops_the_copy_promptly() {
        let fixture = progress_fixture("transfer-lost-copying");
        let renamed = fixture.volume.0.join("Archive renamed");

        let (error, reports) =
            transfer_losing_destination(&fixture, TransferStage::Copying, &|folder| {
                fs::rename(folder, &renamed).unwrap()
            });

        assert_stopped_for_lost_destination(&fixture, &error);
        // Stopped mid-copy, never reaching verification.
        let total = fixture.contents.len() as u64;
        assert!(reports
            .iter()
            .all(|report| report.stage == TransferStage::Copying && report.bytes < total));
        // No copy at the original path, and the partial copy that moved with
        // the folder was removed from its new place.
        assert!(!fixture.destination.exists());
        assert!(folder_names(&renamed).is_empty());

        // With the folder back, the plan can be retried.
        fs::rename(&renamed, fixture.destination.parent().unwrap()).unwrap();
        let transfer = execute_transfer_paths(
            &fixture.connection,
            fixture.move_id,
            &fixture.source,
            &fixture.destination,
        )
        .unwrap();
        assert_eq!(transfer.status, "completed");
        assert_eq!(fs::read(&fixture.destination).unwrap(), fixture.contents);
        let _ = &fixture.database;
    }

    #[test]
    fn destination_renamed_during_verification_is_never_completed() {
        let fixture = progress_fixture("transfer-lost-verifying");
        let renamed = fixture.volume.0.join("Archive renamed");

        let (error, reports) =
            transfer_losing_destination(&fixture, TransferStage::Verifying, &|folder| {
                fs::rename(folder, &renamed).unwrap()
            });

        assert_stopped_for_lost_destination(&fixture, &error);
        let total = fixture.contents.len() as u64;
        assert!(reports
            .iter()
            .all(|report| report.stage == TransferStage::Copying || report.bytes < total));
        assert!(!fixture.destination.exists());
        assert!(folder_names(&renamed).is_empty());
        let _ = &fixture.database;
    }

    #[test]
    fn destination_deleted_during_copy_stops_the_copy() {
        let fixture = progress_fixture("transfer-lost-deleted");

        let (error, _) = transfer_losing_destination(&fixture, TransferStage::Copying, &|folder| {
            fs::remove_dir_all(folder).unwrap()
        });

        assert_stopped_for_lost_destination(&fixture, &error);
        assert!(!fixture.destination.parent().unwrap().exists());
        let _ = &fixture.database;
    }

    #[test]
    fn df_available_space_is_read_from_before_the_capacity_column() {
        let apfs = "Filesystem   1024-blocks      Used Available Capacity  Mounted on
/dev/disk3s5   482797652 196612856 260423516    44%    /System/Volumes/Data";
        assert_eq!(parse_df_available(apfs), Some(260_423_516 * 1024));

        // A filesystem name with spaces does not shift the columns read.
        let spaced = "Filesystem 1024-blocks Used Available Capacity Mounted on
map auto_home 0 0 0 100% /System/Volumes/Data/home";
        assert_eq!(parse_df_available(spaced), Some(0));

        assert_eq!(parse_df_available("Filesystem\nnot a df line"), None);

        assert!(free_bytes_at(&std::env::temp_dir()).unwrap() > 0);
    }

    #[test]
    fn copies_into_a_folder_on_this_mac_check_its_free_space() {
        let database = TestDatabase::new("local-space");
        let source_volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-local-space-source-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let folder = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-local-space-folder-{}-{}",
            std::process::id(),
            now_unix()
        )));
        for path in [&source_volume.0, &folder.0] {
            let _ = fs::remove_dir_all(path);
            fs::create_dir_all(path).unwrap();
        }
        fs::write(source_volume.0.join("small.mov"), b"small").unwrap();
        let modified_at = system_time_unix(
            fs::metadata(source_volume.0.join("small.mov"))
                .unwrap()
                .modified(),
        );

        let mut connection = open_database(&database.0).unwrap();
        insert_drive(&connection, "UUID-A", "Source");
        sync_drive_locations(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO locations (id, kind, display_name, local_path, created_at)
                 VALUES ('local:test', 'local_folder', 'Media', ?1, 0)",
                params![folder.0.to_string_lossy()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO files (drive_id, relative_path, name, parent_path, is_directory, size_bytes, modified_at)
                 VALUES ('UUID-A', 'small.mov', 'small.mov', '', 0, 5, ?1),
                        ('UUID-A', 'huge.mov', 'huge.mov', '', 0, 1000000000000000000, ?1)",
                params![modified_at],
            )
            .unwrap();
        let drives = vec![test_drive("UUID-A", "Source", &source_volume.0, 10_000)];
        let capacity_issues = |connection: &Connection| {
            validate_plan_live(connection, &drives)
                .unwrap()
                .issues
                .into_iter()
                .filter(|issue| issue.code == "live_insufficient_capacity")
                .collect::<Vec<_>>()
        };

        plan_move(
            &mut connection,
            "UUID-A",
            "small.mov",
            "local:test",
            "small.mov",
        )
        .unwrap();
        assert!(capacity_issues(&connection).is_empty());

        // An exabyte cannot fit on this Mac.
        plan_move(
            &mut connection,
            "UUID-A",
            "huge.mov",
            "local:test",
            "huge.mov",
        )
        .unwrap();
        let issues = capacity_issues(&connection);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].location_id.as_deref(), Some("local:test"));
        assert!(
            issues[0].message.contains("keeping 1 GB free"),
            "{}",
            issues[0].message
        );
    }

    #[test]
    fn transfer_records_its_completion_despite_brief_database_contention() {
        let database = TestDatabase::new("transfer-contention");
        let volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-contention-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let _ = fs::remove_dir_all(&volume.0);
        fs::create_dir_all(&volume.0).unwrap();
        let source = volume.0.join("source.mov");
        let destination = volume.0.join("Archive/film.mov");
        fs::write(&source, b"contents").unwrap();

        let mut connection = open_database(&database.0).unwrap();
        insert_drive(&connection, "UUID-A", "Source");
        insert_drive(&connection, "UUID-B", "Backup");
        sync_drive_locations(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO files (drive_id, relative_path, name, parent_path, is_directory, size_bytes)
                 VALUES ('UUID-A', 'source.mov', 'source.mov', '', 0, 8)",
                [],
            )
            .unwrap();
        let move_id = plan_move(
            &mut connection,
            "UUID-A",
            "source.mov",
            "drive:UUID-B",
            "Archive/film.mov",
        )
        .unwrap();

        // Another connection holds the write lock for longer than the normal
        // two-second wait, as a short write elsewhere might.
        let (locked, wait_for_lock) = std::sync::mpsc::channel();
        let path = database.0.clone();
        let holder = std::thread::spawn(move || {
            let other = Connection::open(&path).unwrap();
            other.execute_batch("BEGIN IMMEDIATE").unwrap();
            locked.send(()).unwrap();
            std::thread::sleep(Duration::from_secs(3));
            other.execute_batch("COMMIT").unwrap();
        });
        wait_for_lock.recv().unwrap();

        let transfer = execute_transfer_paths(&connection, move_id, &source, &destination)
            .expect("the transfer waits out the contention instead of failing");
        holder.join().unwrap();

        assert_eq!(transfer.status, "completed");
        assert_eq!(fs::read(&destination).unwrap(), b"contents");
        let planned: i64 = connection
            .query_row("SELECT COUNT(*) FROM planned_moves", [], |row| row.get(0))
            .unwrap();
        assert_eq!(planned, 0);
    }

    #[test]
    fn transfer_execution_copies_verifies_and_completes() {
        let database = TestDatabase::new("execute-transfer");
        let volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-execute-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let _ = fs::remove_dir_all(&volume.0);
        fs::create_dir_all(&volume.0).unwrap();

        let source = volume.0.join("source.mov");
        let destination = volume.0.join("Archive/film.mov");
        let contents = vec![0x73_u8; 1024 * 1024 + 41];
        fs::write(&source, &contents).unwrap();

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
                 ) VALUES ('UUID-A', 'source.mov', 'source.mov', '', 0, ?1)",
                params![contents.len() as i64],
            )
            .unwrap();

        let move_id = plan_move(
            &mut connection,
            "UUID-A",
            "source.mov",
            "drive:UUID-B",
            "Archive/film.mov",
        )
        .unwrap();

        let transfer = execute_transfer_paths(&connection, move_id, &source, &destination).unwrap();

        assert_eq!(transfer.status, "completed");
        assert_eq!(transfer.copied_bytes, contents.len() as i64);
        assert_eq!(transfer.error_message, None);
        assert!(transfer.started_at.is_some());
        assert!(transfer.completed_at.is_some());

        assert_eq!(fs::read(&source).unwrap(), contents);
        assert_eq!(fs::read(&destination).unwrap(), contents);
        assert!(source.exists());

        let history = list_transfer_records(&connection).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, "completed");

        let planned_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM planned_moves WHERE id = ?1",
                params![move_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            planned_count, 0,
            "a completed verified transfer must leave the active plan"
        );
    }

    #[test]
    fn transfer_execution_records_failure_and_preserves_existing_destination() {
        let database = TestDatabase::new("execute-transfer-failure");
        let volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-execute-failure-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let _ = fs::remove_dir_all(&volume.0);
        fs::create_dir_all(&volume.0).unwrap();

        let source = volume.0.join("source.mov");
        let destination = volume.0.join("film.mov");

        fs::write(&source, b"new source").unwrap();
        fs::write(&destination, b"existing destination").unwrap();

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
                 ) VALUES ('UUID-A', 'source.mov', 'source.mov', '', 0, 10)",
                [],
            )
            .unwrap();

        let move_id = plan_move(
            &mut connection,
            "UUID-A",
            "source.mov",
            "drive:UUID-B",
            "film.mov",
        )
        .unwrap();

        let error =
            execute_transfer_paths(&connection, move_id, &source, &destination).unwrap_err();

        assert!(error.contains("Destination already exists"), "{error}");

        assert_eq!(fs::read(&source).unwrap(), b"new source");
        assert_eq!(fs::read(&destination).unwrap(), b"existing destination");

        let history = list_transfer_records(&connection).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, "failed");

        let planned_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM planned_moves WHERE id = ?1",
                params![move_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            planned_count, 1,
            "a failed transfer must remain in the active plan"
        );

        assert!(history[0]
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("Destination already exists"));
        assert!(source.exists());
    }

    #[test]
    fn transfer_status_tracks_execution_lifecycle() {
        let database = TestDatabase::new("transfer-lifecycle");
        let connection = open_database(&database.0).unwrap();

        connection
            .execute(
                "INSERT INTO transfers (
                    source_drive_id,
                    source_relative_path,
                    destination_location_id,
                    destination_relative_path,
                    total_bytes,
                    status,
                    created_at
                 ) VALUES (
                    'UUID-A',
                    'film.mov',
                    'drive:UUID-B',
                    'film.mov',
                    100,
                    'pending',
                    1
                 )",
                [],
            )
            .unwrap();

        let id = connection.last_insert_rowid();

        update_transfer_status(&connection, id, "copying", Some(40), None).unwrap();

        let copying: (String, i64, Option<i64>, Option<i64>) = connection
            .query_row(
                "SELECT status, copied_bytes, started_at, completed_at
                 FROM transfers WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();

        assert_eq!(copying.0, "copying");
        assert_eq!(copying.1, 40);
        assert!(copying.2.is_some());
        assert!(copying.3.is_none());

        update_transfer_status(&connection, id, "verifying", Some(100), None).unwrap();
        update_transfer_status(&connection, id, "completed", Some(100), None).unwrap();

        let completed: (String, i64, Option<String>, Option<i64>) = connection
            .query_row(
                "SELECT status, copied_bytes, error_message, completed_at
                 FROM transfers WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();

        assert_eq!(completed.0, "completed");
        assert_eq!(completed.1, 100);
        assert_eq!(completed.2, None);
        assert!(completed.3.is_some());
    }

    #[test]
    fn failed_transfer_records_error_without_removing_snapshot() {
        let database = TestDatabase::new("transfer-failure");
        let connection = open_database(&database.0).unwrap();

        connection
            .execute(
                "INSERT INTO transfers (
                    source_drive_id,
                    source_relative_path,
                    destination_location_id,
                    destination_relative_path,
                    status,
                    created_at
                 ) VALUES (
                    'UUID-A',
                    'film.mov',
                    'drive:UUID-B',
                    'film.mov',
                    'pending',
                    1
                 )",
                [],
            )
            .unwrap();

        let id = connection.last_insert_rowid();

        update_transfer_status(&connection, id, "copying", None, None).unwrap();
        update_transfer_status(
            &connection,
            id,
            "failed",
            None,
            Some("Destination disconnected."),
        )
        .unwrap();

        let transfer = list_transfer_records(&connection).unwrap().remove(0);

        assert_eq!(transfer.id, id);
        assert_eq!(transfer.status, "failed");
        assert_eq!(
            transfer.error_message.as_deref(),
            Some("Destination disconnected.")
        );
        assert_eq!(transfer.source_relative_path, "film.mov");
        assert!(transfer.started_at.is_some());
        assert!(transfer.completed_at.is_some());
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

    // Two folders standing in for connected drives, with film.mov catalogued
    // on the source and planned to Archive/film.mov on the destination.
    struct TransferFixture {
        connection: Connection,
        drives: Vec<DriveInfo>,
        move_id: i64,
        source: PathBuf,
        destination: PathBuf,
        contents: Vec<u8>,
        source_volume: TestVolume,
        destination_volume: TestVolume,
        database: TestDatabase,
    }

    fn transfer_fixture(name: &str) -> TransferFixture {
        let contents: Vec<u8> = (0..256 * 1024 + 7)
            .map(|index| (index % 251) as u8)
            .collect();
        transfer_fixture_with(name, "film.mov", "Archive/film.mov", contents)
    }

    // As transfer_fixture, with the source's path, its planned destination
    // and its contents chosen by the test.
    fn transfer_fixture_with(
        name: &str,
        source_relative_path: &str,
        destination_relative_path: &str,
        contents: Vec<u8>,
    ) -> TransferFixture {
        let database = TestDatabase::new(name);
        let unique = format!("{name}-{}-{}", std::process::id(), now_unix());
        let source_volume =
            TestVolume(std::env::temp_dir().join(format!("media-mapper-{unique}-source")));
        let destination_volume =
            TestVolume(std::env::temp_dir().join(format!("media-mapper-{unique}-destination")));
        let _ = fs::remove_dir_all(&source_volume.0);
        let _ = fs::remove_dir_all(&destination_volume.0);
        fs::create_dir_all(&source_volume.0).unwrap();
        fs::create_dir_all(&destination_volume.0).unwrap();

        let mut connection = open_database(&database.0).unwrap();
        insert_drive(&connection, "UUID-A", "Source");
        insert_drive(&connection, "UUID-B", "Backup");
        sync_drive_locations(&connection).unwrap();
        let source = catalogue_test_file(
            &connection,
            &source_volume.0,
            source_relative_path,
            &contents,
        );
        let move_id = plan_move(
            &mut connection,
            "UUID-A",
            source_relative_path,
            "drive:UUID-B",
            destination_relative_path,
        )
        .unwrap();

        let drives = vec![
            test_drive("UUID-A", "Source", &source_volume.0, 1_000_000_000),
            test_drive("UUID-B", "Backup", &destination_volume.0, 1_000_000_000),
        ];
        let destination = destination_volume.0.join(destination_relative_path);

        TransferFixture {
            connection,
            drives,
            move_id,
            source,
            destination,
            contents,
            source_volume: source_volume,
            destination_volume,
            database,
        }
    }

    // Writes a file on the test source drive `UUID-A` and catalogues it, with
    // any folders above it, as a scan would. Returns its path.
    fn catalogue_test_file(
        connection: &Connection,
        volume: &Path,
        relative_path: &str,
        contents: &[u8],
    ) -> PathBuf {
        let path = volume.join(relative_path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        let modified_at = system_time_unix(fs::metadata(&path).unwrap().modified());

        let mut folder = Path::new(relative_path).parent();
        while let Some(current) = folder.filter(|folder| !folder.as_os_str().is_empty()) {
            let parent = current
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned());
            connection
                .execute(
                    "INSERT OR IGNORE INTO files (
                        drive_id, relative_path, name, parent_path, is_directory
                     ) VALUES ('UUID-A', ?1, ?2, ?3, 1)",
                    params![
                        current.to_string_lossy(),
                        current.file_name().unwrap().to_string_lossy(),
                        parent.unwrap_or_default()
                    ],
                )
                .unwrap();
            folder = current.parent();
        }

        let relative = Path::new(relative_path);
        connection
            .execute(
                "INSERT INTO files (
                    drive_id, relative_path, name, parent_path, is_directory, size_bytes, modified_at
                 ) VALUES ('UUID-A', ?1, ?2, ?3, 0, ?4, ?5)",
                params![
                    relative_path,
                    relative.file_name().unwrap().to_string_lossy(),
                    relative
                        .parent()
                        .map(|parent| parent.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    contents.len() as i64,
                    modified_at
                ],
            )
            .unwrap();
        path
    }

    // Leaves the database and disk as a process killed mid-transfer would: a
    // record stuck in `status` and a temporary file holding the first `bytes`.
    fn simulate_interrupted_transfer(
        fixture: &TransferFixture,
        status: &str,
        bytes: usize,
    ) -> (TransferRecord, PathBuf) {
        let connection = &fixture.connection;
        let transfer = create_transfer_record(connection, fixture.move_id).unwrap();
        update_transfer_status(connection, transfer.id, "copying", None, None).unwrap();
        if status == "verifying" {
            update_transfer_status(
                connection,
                transfer.id,
                "verifying",
                Some(bytes as i64),
                None,
            )
            .unwrap();
        }
        let temporary =
            transfer_temporary_path(&fixture.destination, transfer.id, transfer.created_at)
                .unwrap();
        fs::create_dir_all(temporary.parent().unwrap()).unwrap();
        fs::write(&temporary, &fixture.contents[..bytes]).unwrap();
        (transfer, temporary)
    }

    fn transfer_status(connection: &Connection, id: i64) -> (String, Option<String>) {
        connection
            .query_row(
                "SELECT status, error_message FROM transfers WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    }

    fn folder_names(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn transfer_temporary_file_is_unique_to_its_record() {
        let destination = Path::new("/Volumes/Backup/Archive/film.mov");
        let first = transfer_temporary_path(destination, 7, 1_700_000_000).unwrap();
        let second = transfer_temporary_path(destination, 8, 1_700_000_000).unwrap();

        assert_eq!(
            first,
            Path::new("/Volumes/Backup/Archive/.mediamapper-transfer-7-1700000000.partial")
        );
        assert_ne!(first, second);
        assert!(transfer_temporary_path(
            Path::new("/Volumes/Backup/.mediamapper-transfer-7-1.partial"),
            7,
            1
        )
        .is_err());
    }

    #[test]
    fn interrupted_copy_is_recovered_at_startup_and_retry_succeeds() {
        let fixture = transfer_fixture("recover-copying");
        let (transfer, temporary) = simulate_interrupted_transfer(&fixture, "copying", 100_000);
        let archive = fixture.destination_volume.0.join("Archive");

        // Hidden files that recovery must never touch: another record's name
        // with no such record, the old process-based name, and a user file.
        let unrelated = [
            ".mediamapper-transfer-999-1.partial",
            ".mediamapper-4242-film.mov.partial",
            "notes.partial",
        ];
        for name in unrelated {
            fs::write(archive.join(name), name).unwrap();
        }

        // Launch: records first, without drives.
        let lock = try_lock_transfers(&fixture.connection).unwrap().unwrap();
        let recovery = recover_interrupted_transfers(&fixture.connection, None, None).unwrap();
        assert_eq!(
            recovery,
            TransferRecovery {
                interrupted: 1,
                completed: 0,
                temporary_files_removed: 0
            }
        );
        assert_eq!(
            transfer_status(&fixture.connection, transfer.id),
            (
                "failed".to_string(),
                Some(INTERRUPTED_TRANSFER_MESSAGE.to_string())
            )
        );
        assert!(temporary.exists());

        // Then temporary files, once drives are listed.
        let recovery =
            recover_interrupted_transfers(&fixture.connection, Some(&fixture.drives), None)
                .unwrap();
        assert_eq!(
            recovery,
            TransferRecovery {
                interrupted: 0,
                completed: 0,
                temporary_files_removed: 1
            }
        );
        assert!(!temporary.exists());
        assert!(!fixture.destination.exists());
        for name in unrelated {
            assert_eq!(fs::read(archive.join(name)).unwrap(), name.as_bytes());
        }
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);

        // Recovery is idempotent.
        let recovery =
            recover_interrupted_transfers(&fixture.connection, Some(&fixture.drives), None)
                .unwrap();
        assert_eq!(recovery, TransferRecovery::default());
        drop(lock);

        // The plan is still waiting, and a retry copies and verifies it.
        let retry = execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
            .unwrap();
        assert_eq!(retry.status, "completed");
        assert_eq!(fs::read(&fixture.destination).unwrap(), fixture.contents);
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);

        let mut expected: Vec<String> = unrelated.iter().map(|name| name.to_string()).collect();
        expected.push("film.mov".to_string());
        expected.sort();
        assert_eq!(
            folder_names(&archive),
            expected,
            "no partial of this transfer remains"
        );

        let history = list_transfer_records(&fixture.connection).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].status, "completed");
        assert_eq!(history[1].status, "failed");
    }

    #[test]
    fn interrupted_verification_is_never_marked_complete() {
        let fixture = transfer_fixture("recover-verifying");
        // Every byte was copied, but verification never finished.
        let (transfer, temporary) =
            simulate_interrupted_transfer(&fixture, "verifying", fixture.contents.len());

        let recovery =
            recover_interrupted_transfers(&fixture.connection, Some(&fixture.drives), None)
                .unwrap();
        assert_eq!(
            recovery,
            TransferRecovery {
                interrupted: 1,
                completed: 0,
                temporary_files_removed: 1
            }
        );
        assert_eq!(
            transfer_status(&fixture.connection, transfer.id),
            (
                "failed".to_string(),
                Some(INTERRUPTED_TRANSFER_MESSAGE.to_string())
            )
        );
        assert!(!temporary.exists());
        assert!(
            !fixture.destination.exists(),
            "an unverified copy is never finalised"
        );

        let retry = execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
            .unwrap();
        assert_eq!(retry.status, "completed");
        assert_eq!(fs::read(&fixture.destination).unwrap(), fixture.contents);
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
        assert_eq!(
            folder_names(fixture.destination.parent().unwrap()),
            vec!["film.mov"]
        );
    }

    #[test]
    fn retry_cleans_up_a_stale_partial_that_startup_could_not_reach() {
        let fixture = transfer_fixture("recover-on-retry");
        let (transfer, temporary) = simulate_interrupted_transfer(&fixture, "copying", 4_096);

        // The destination was offline at launch, so only the record was
        // resolved. Here even that step was missed; the retry must do both.
        let retry = execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
            .unwrap();

        assert_eq!(retry.status, "completed");
        assert_eq!(
            transfer_status(&fixture.connection, transfer.id),
            (
                "failed".to_string(),
                Some(INTERRUPTED_TRANSFER_MESSAGE.to_string())
            )
        );
        assert!(!temporary.exists());
        assert_eq!(fs::read(&fixture.destination).unwrap(), fixture.contents);
        assert_eq!(
            folder_names(fixture.destination.parent().unwrap()),
            vec!["film.mov"]
        );
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
    }

    // The state a crash leaves between the final rename and recording
    // `completed`: the record says `verifying`, the temporary file is gone and
    // the copy is at the destination.
    fn simulate_crash_after_rename(fixture: &TransferFixture) -> TransferRecord {
        let (transfer, temporary) =
            simulate_interrupted_transfer(fixture, "verifying", fixture.contents.len());
        fs::rename(&temporary, &fixture.destination).unwrap();
        transfer
    }

    fn planned_move_exists(connection: &Connection, id: i64) -> bool {
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM planned_moves WHERE id = ?1)",
                params![id],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn copy_finalised_before_a_crash_is_confirmed_and_completed() {
        let fixture = transfer_fixture("recover-finalised");
        let transfer = simulate_crash_after_rename(&fixture);

        // Launch, before drives are listed: the record cannot be judged yet,
        // so it is left alone rather than called failed.
        let recovery = recover_interrupted_transfers(&fixture.connection, None, None).unwrap();
        assert_eq!(recovery, TransferRecovery::default());
        assert_eq!(
            transfer_status(&fixture.connection, transfer.id),
            ("verifying".to_string(), None)
        );

        // With drives, the copy is compared with the source and completed.
        let recovery =
            recover_interrupted_transfers(&fixture.connection, Some(&fixture.drives), None)
                .unwrap();
        assert_eq!(
            recovery,
            TransferRecovery {
                interrupted: 0,
                completed: 1,
                temporary_files_removed: 0
            }
        );
        let history = list_transfer_records(&fixture.connection).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, "completed");
        assert_eq!(history[0].error_message, None);
        assert_eq!(history[0].copied_bytes, fixture.contents.len() as i64);
        assert!(history[0].completed_at.is_some());
        assert!(!planned_move_exists(&fixture.connection, fixture.move_id));
        assert_eq!(fs::read(&fixture.destination).unwrap(), fixture.contents);
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
        assert_eq!(
            folder_names(fixture.destination.parent().unwrap()),
            vec!["film.mov"]
        );

        let recovery =
            recover_interrupted_transfers(&fixture.connection, Some(&fixture.drives), None)
                .unwrap();
        assert_eq!(recovery, TransferRecovery::default());
    }

    #[test]
    fn retry_reports_a_finalised_copy_instead_of_copying_again() {
        let fixture = transfer_fixture("retry-finalised");
        let transfer = simulate_crash_after_rename(&fixture);

        // No launch recovery ran; the retry itself finds the finished copy.
        let result =
            execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
                .unwrap();

        assert_eq!(result.id, transfer.id);
        assert_eq!(result.status, "completed");
        assert_eq!(list_transfer_records(&fixture.connection).unwrap().len(), 1);
        assert!(!planned_move_exists(&fixture.connection, fixture.move_id));
        assert_eq!(fs::read(&fixture.destination).unwrap(), fixture.contents);
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
    }

    #[test]
    fn finalised_copy_waits_until_its_source_can_confirm_it() {
        let fixture = transfer_fixture("finalised-source-offline");
        let transfer = simulate_crash_after_rename(&fixture);
        let destination_only = vec![fixture.drives[1].clone()];

        let recovery =
            recover_interrupted_transfers(&fixture.connection, Some(&destination_only), None)
                .unwrap();
        assert_eq!(recovery, TransferRecovery::default());
        assert_eq!(
            transfer_status(&fixture.connection, transfer.id).0,
            "verifying"
        );
        assert!(planned_move_exists(&fixture.connection, fixture.move_id));

        let recovery =
            recover_interrupted_transfers(&fixture.connection, Some(&fixture.drives), None)
                .unwrap();
        assert_eq!(recovery.completed, 1);
        assert_eq!(
            transfer_status(&fixture.connection, transfer.id).0,
            "completed"
        );
    }

    #[test]
    fn finalised_copy_is_not_completed_once_its_source_is_gone() {
        let fixture = transfer_fixture("finalised-source-gone");
        let transfer = simulate_crash_after_rename(&fixture);
        fs::remove_file(&fixture.source).unwrap();

        let recovery =
            recover_interrupted_transfers(&fixture.connection, Some(&fixture.drives), None)
                .unwrap();

        assert_eq!(recovery.interrupted, 1);
        assert_eq!(
            transfer_status(&fixture.connection, transfer.id),
            (
                "failed".to_string(),
                Some(UNCONFIRMED_DESTINATION_MESSAGE.to_string())
            )
        );
        assert_eq!(fs::read(&fixture.destination).unwrap(), fixture.contents);
        assert!(planned_move_exists(&fixture.connection, fixture.move_id));
    }

    #[test]
    fn recovery_never_removes_a_destination_or_anything_but_a_file() {
        let fixture = transfer_fixture("recover-keeps-destination");
        let finalised = simulate_crash_after_rename(&fixture);
        // Something else then replaced the file at the destination with
        // different bytes of the same length.
        let replaced: Vec<u8> = fixture.contents.iter().map(|byte| !byte).collect();
        fs::remove_file(&fixture.destination).unwrap();
        fs::write(&fixture.destination, &replaced).unwrap();

        // Another interrupted attempt whose temporary path is now a folder.
        let (odd, odd_temporary) = simulate_interrupted_transfer(&fixture, "copying", 10);
        fs::remove_file(&odd_temporary).unwrap();
        fs::create_dir(&odd_temporary).unwrap();
        fs::write(odd_temporary.join("keep.txt"), b"keep").unwrap();

        let recovery =
            recover_interrupted_transfers(&fixture.connection, Some(&fixture.drives), None)
                .unwrap();

        assert_eq!(
            recovery,
            TransferRecovery {
                interrupted: 2,
                completed: 0,
                temporary_files_removed: 0
            }
        );
        assert_eq!(
            transfer_status(&fixture.connection, finalised.id),
            (
                "failed".to_string(),
                Some(MISMATCHED_DESTINATION_MESSAGE.to_string())
            )
        );
        assert_eq!(transfer_status(&fixture.connection, odd.id).0, "failed");
        assert_eq!(fs::read(&fixture.destination).unwrap(), replaced);
        assert_eq!(fs::read(odd_temporary.join("keep.txt")).unwrap(), b"keep");
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);

        // The retry refuses to overwrite the file already at the destination.
        let error = execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
            .unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(fs::read(&fixture.destination).unwrap(), replaced);
    }

    #[test]
    fn failed_copy_never_deletes_a_file_already_at_its_temporary_path() {
        let fixture = transfer_fixture("temporary-collision");
        let temporary = transfer_temporary_path(&fixture.destination, 1, 1).unwrap();
        fs::create_dir_all(temporary.parent().unwrap()).unwrap();
        fs::write(&temporary, b"not ours").unwrap();

        let error = copy_file_verified_with_stage(
            &fixture.source,
            &fixture.destination,
            &temporary,
            |_| Ok(()),
        )
        .unwrap_err();

        assert!(
            error.contains("Unable to create temporary destination file"),
            "{error}"
        );
        assert_eq!(fs::read(&temporary).unwrap(), b"not ours");
        assert!(!fixture.destination.exists());
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
    }

    #[test]
    fn transfer_lock_rejects_a_second_transfer() {
        let fixture = transfer_fixture("transfer-lock");
        let other = open_database(&fixture.database.0).unwrap();

        let held = try_lock_transfers(&other).unwrap().expect("lock is free");
        assert!(try_lock_transfers(&fixture.connection).unwrap().is_none());

        let error = execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
            .unwrap_err();
        assert!(
            error.contains("Another transfer or a drive scan is running"),
            "{error}"
        );
        assert!(list_transfer_records(&fixture.connection)
            .unwrap()
            .is_empty());
        assert!(!fixture.destination.exists());
        assert!(!fixture.destination.parent().unwrap().exists());

        drop(held);
        assert!(try_lock_transfers(&fixture.connection).unwrap().is_some());
        let transfer =
            execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
                .unwrap();
        assert_eq!(transfer.status, "completed");
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
    }

    #[test]
    fn concurrent_executions_of_one_move_complete_it_once() {
        let fixture = transfer_fixture("transfer-concurrent");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let results: Vec<Result<TransferRecord, String>> = (0..2)
            .map(|_| {
                let database = fixture.database.0.clone();
                let drives = fixture.drives.clone();
                let barrier = barrier.clone();
                let move_id = fixture.move_id;
                std::thread::spawn(move || {
                    let connection = open_database(&database).unwrap();
                    barrier.wait();
                    execute_planned_transfer(&connection, move_id, &drives)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        assert_eq!(
            results.iter().filter(|result| result.is_ok()).count(),
            1,
            "{results:?}"
        );
        let completed: i64 = fixture
            .connection
            .query_row(
                "SELECT COUNT(*) FROM transfers WHERE status = 'completed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(completed, 1);
        assert_eq!(fs::read(&fixture.destination).unwrap(), fixture.contents);
        assert_eq!(
            folder_names(fixture.destination.parent().unwrap()),
            vec!["film.mov"]
        );
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
    }

    fn scan_times(connection: &Connection, drive_id: &str) -> Option<(Option<i64>, Option<i64>)> {
        catalogued_drives(connection)
            .unwrap()
            .into_iter()
            .find(|drive| drive.persistent_identifier == drive_id)
            .map(|drive| (drive.last_scanned_at, drive.last_connected_at))
    }

    #[test]
    fn existing_database_migrates_keeping_catalogue_and_scan_times() {
        let database = TestDatabase::new("freshness-migration");
        Connection::open(&database.0)
            .unwrap()
            .execute_batch(&format!(
                "{LEGACY_SCHEMA}{LEGACY_LOCATIONS_SCHEMA}
                 UPDATE drives SET last_scanned_at = 1000, last_seen_at = 900
                 WHERE persistent_identifier = 'UUID-A';"
            ))
            .unwrap();

        let connection = open_database(&database.0).expect("old database must migrate");

        let files: Vec<String> = connection
            .prepare("SELECT relative_path FROM files WHERE drive_id = 'UUID-A'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(files, ["film.mp4"]);
        assert_eq!(location_count(&connection), 2);

        let drives = catalogued_drives(&connection).unwrap();
        let source = drives
            .iter()
            .find(|drive| drive.persistent_identifier == "UUID-A")
            .unwrap();
        assert_eq!(source.last_scanned_at, Some(1000));
        assert_eq!(source.last_connected_at, Some(900));
        assert_eq!(scan_times(&connection, "UUID-B"), Some((None, Some(0))));
    }

    #[test]
    fn connection_is_recorded_for_known_drives_without_counting_as_a_scan() {
        let database = TestDatabase::new("record-connection");
        let connection = open_database(&database.0).unwrap();
        insert_drive(&connection, "UUID-SCANNED", "Films");
        insert_drive(&connection, "UUID-NEVER", "Old Backup");
        connection
            .execute(
                "UPDATE drives SET last_scanned_at = 500
                 WHERE persistent_identifier = 'UUID-SCANNED'",
                [],
            )
            .unwrap();

        let drive_ids = ["UUID-SCANNED", "UUID-NEVER", "UUID-UNKNOWN"].map(String::from);
        record_drive_connections(&connection, &drive_ids, 2000).unwrap();

        assert_eq!(
            scan_times(&connection, "UUID-SCANNED"),
            Some((Some(500), Some(2000)))
        );
        // Never scanned stays never scanned, however often it connects.
        assert_eq!(
            scan_times(&connection, "UUID-NEVER"),
            Some((None, Some(2000)))
        );
        // A drive with no catalogue is not added by connecting it.
        assert_eq!(scan_times(&connection, "UUID-UNKNOWN"), None);

        // An older sighting never moves the time backwards.
        record_drive_connections(&connection, &drive_ids[..1], 1500).unwrap();
        assert_eq!(
            scan_times(&connection, "UUID-SCANNED"),
            Some((Some(500), Some(2000)))
        );
    }

    #[cfg(unix)]
    #[test]
    fn only_a_completed_scan_changes_the_scan_time() {
        use std::os::unix::fs::PermissionsExt;

        let database = TestDatabase::new("scan-time");
        let volume = TestVolume(std::env::temp_dir().join(format!(
            "media-mapper-scan-time-{}-{}",
            std::process::id(),
            now_unix()
        )));
        let _ = fs::remove_dir_all(&volume.0);
        fs::create_dir_all(volume.0.join("Video")).unwrap();
        fs::write(volume.0.join("Video/film.mp4"), b"film").unwrap();
        let drive = test_drive("UUID-SCAN-TIME", "Films", &volume.0, 1_000);
        let scan =
            |drive: &DriveInfo| scan_drive_job(database.0.clone(), drive.clone(), |_, _, _| {})();
        let cancel = || {
            cancelled_scans()
                .lock()
                .unwrap()
                .insert("UUID-SCAN-TIME".to_string());
        };

        // A first scan that is cancelled leaves the drive never scanned.
        cancel();
        let error = scan(&drive).unwrap_err();
        assert_eq!(error, "Scan cancelled.");
        let connection = open_database(&database.0).unwrap();
        assert_eq!(scan_times(&connection, "UUID-SCAN-TIME"), None);

        let result = scan(&drive).unwrap();
        let (scanned_at, _) = scan_times(&connection, "UUID-SCAN-TIME").unwrap();
        assert_eq!(scanned_at, Some(result.scanned_at));

        // Mark the successful scan as older, so a later write would show.
        connection
            .execute(
                "UPDATE drives SET last_scanned_at = 100
                 WHERE persistent_identifier = 'UUID-SCAN-TIME'",
                [],
            )
            .unwrap();
        fs::write(volume.0.join("Video/new.mp4"), b"new").unwrap();

        // A cancelled rescan keeps the previous scan time and catalogue.
        cancel();
        scan(&drive).unwrap_err();
        let (scanned_at, _) = scan_times(&connection, "UUID-SCAN-TIME").unwrap();
        assert_eq!(scanned_at, Some(100));

        // So does a rescan that fails part way, here on an unreadable drive.
        fs::set_permissions(&volume.0, fs::Permissions::from_mode(0o000)).unwrap();
        let result = scan(&drive);
        fs::set_permissions(&volume.0, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.unwrap_err().contains("top folder"));
        let (scanned_at, _) = scan_times(&connection, "UUID-SCAN-TIME").unwrap();
        assert_eq!(scanned_at, Some(100));
        let file_count: i64 = connection
            .query_row(
                "SELECT file_count FROM drives WHERE persistent_identifier = 'UUID-SCAN-TIME'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(file_count, 1, "the previous catalogue must be kept");

        // A drive that is no longer mounted cannot be scanned at all.
        let missing = test_drive(
            "UUID-SCAN-TIME",
            "Films",
            &volume.0.join("not-mounted"),
            1_000,
        );
        scan(&missing).unwrap_err();
        let (scanned_at, _) = scan_times(&connection, "UUID-SCAN-TIME").unwrap();
        assert_eq!(scanned_at, Some(100));
    }

    #[test]
    fn drives_are_listed_most_recently_scanned_first_then_by_name() {
        let database = TestDatabase::new("drive-order");
        let connection = open_database(&database.0).unwrap();
        for (id, name, scanned_at) in [
            ("UUID-1", "beta", Some(100)),
            ("UUID-2", "Echo", None),
            ("UUID-3", "gamma", Some(300)),
            ("UUID-4", "Alpha", Some(100)),
            ("UUID-5", "delta", None),
        ] {
            insert_drive(&connection, id, name);
            connection
                .execute(
                    "UPDATE drives SET last_scanned_at = ?1 WHERE persistent_identifier = ?2",
                    params![scanned_at, id],
                )
                .unwrap();
        }

        let names: Vec<String> = catalogued_drives(&connection)
            .unwrap()
            .into_iter()
            .map(|drive| drive.name)
            .collect();
        assert_eq!(names, ["gamma", "Alpha", "beta", "delta", "Echo"]);
    }

    #[test]
    fn transfer_refuses_a_catalogued_source_that_is_gone() {
        let fixture = transfer_fixture("transfer-source-gone");
        fs::remove_file(&fixture.source).unwrap();

        let error = execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
            .unwrap_err();

        assert!(error.contains("not currently present"), "{error}");
        assert!(list_transfer_records(&fixture.connection)
            .unwrap()
            .is_empty());
        assert!(!fixture.destination.exists());
        assert!(planned_move_exists(&fixture.connection, fixture.move_id));
    }

    #[cfg(unix)]
    #[test]
    fn transfer_refuses_a_catalogued_file_replaced_by_a_link() {
        let fixture = transfer_fixture("transfer-source-link");
        let elsewhere = fixture.destination_volume.0.join("elsewhere.mov");
        fs::write(&elsewhere, b"not the catalogued file").unwrap();
        fs::remove_file(&fixture.source).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &fixture.source).unwrap();
        // Give the catalogue the link's own size and no date, so only its
        // type tells it apart.
        let link_size = fs::symlink_metadata(&fixture.source).unwrap().len() as i64;
        fixture
            .connection
            .execute(
                "UPDATE files SET size_bytes = ?1, modified_at = NULL
                 WHERE drive_id = 'UUID-A' AND relative_path = 'film.mov'",
                params![link_size],
            )
            .unwrap();

        let error = execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
            .unwrap_err();

        assert!(error.contains("changed type"), "{error}");
        assert!(list_transfer_records(&fixture.connection)
            .unwrap()
            .is_empty());
        assert!(!fixture.destination.exists());
    }

    #[test]
    fn transfer_refuses_a_destination_that_appeared_after_planning() {
        let fixture = transfer_fixture("transfer-destination-appeared");
        fs::create_dir_all(fixture.destination.parent().unwrap()).unwrap();
        fs::write(&fixture.destination, b"someone else's file").unwrap();

        let error = execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
            .unwrap_err();

        assert!(error.contains("already exists"), "{error}");
        assert_eq!(
            fs::read(&fixture.destination).unwrap(),
            b"someone else's file"
        );
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
        assert!(list_transfer_records(&fixture.connection)
            .unwrap()
            .is_empty());
        assert!(planned_move_exists(&fixture.connection, fixture.move_id));
    }

    #[test]
    fn transfer_never_replaces_a_destination_that_appears_during_the_copy() {
        let fixture = transfer_fixture("transfer-destination-races");
        let destination = fixture.destination.clone();

        // Validation has passed by the time the copy reports progress.
        let error = execute_planned_transfer_reporting(
            &fixture.connection,
            fixture.move_id,
            &fixture.drives,
            &|_| {
                if !destination.exists() {
                    fs::write(&destination, b"arrived mid-copy").unwrap();
                }
            },
            &|| false,
        )
        .unwrap_err();

        assert!(error.contains("already exists"), "{error}");
        assert_eq!(fs::read(&fixture.destination).unwrap(), b"arrived mid-copy");
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
        assert_eq!(
            folder_names(fixture.destination.parent().unwrap()),
            vec!["film.mov"],
            "no temporary file may be left behind"
        );
        let history = list_transfer_records(&fixture.connection).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, "failed");
        assert!(planned_move_exists(&fixture.connection, fixture.move_id));
    }

    // ---- Safety and recovery matrix (SAFETY-MATRIX.md) ----
    //
    // Every case checks the same promises: the source is unchanged, nothing
    // is at the destination unless the transfer completed and matches the
    // source, no temporary file of ours is left behind, and the plan item
    // remains unless the move completed.

    // The transfer temporary files left in a folder.
    fn partial_files(folder: &Path) -> Vec<String> {
        match fs::read_dir(folder) {
            Ok(entries) => entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with(".mediamapper-transfer-"))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn assert_nothing_copied(fixture: &TransferFixture) {
        assert!(
            fs::symlink_metadata(&fixture.destination).is_err(),
            "nothing may be at the destination"
        );
        assert!(partial_files(fixture.destination.parent().unwrap()).is_empty());
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
        assert!(planned_move_exists(&fixture.connection, fixture.move_id));
    }

    fn assert_copied_and_verified(fixture: &TransferFixture, transfer: &TransferRecord) {
        assert_eq!(transfer.status, "completed");
        assert_eq!(transfer.copied_bytes, fixture.contents.len() as i64);
        assert_eq!(fs::read(&fixture.destination).unwrap(), fixture.contents);
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
        assert!(partial_files(fixture.destination.parent().unwrap()).is_empty());
        assert!(!planned_move_exists(&fixture.connection, fixture.move_id));
    }

    #[test]
    fn zero_byte_file_is_copied_and_verified() {
        let fixture =
            transfer_fixture_with("matrix-zero-byte", "empty.txt", "Archive/empty.txt", vec![]);

        let transfer =
            execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
                .unwrap();

        assert_copied_and_verified(&fixture, &transfer);
    }

    #[test]
    fn names_with_spaces_and_unicode_are_copied_exactly() {
        // Composed (NFC) and decomposed (NFD) accents, spaces and non-Latin
        // script, in both folder and file names.
        let cases = [
            ("Été 2024/Café clip.mov", "Archive/Été 2024/Café clip.mov"),
            (
                "E\u{301}te\u{301}/cafe\u{301}.mov",
                "Archive/E\u{301}te\u{301}/cafe\u{301}.mov",
            ),
            ("写真/家族 旅行.jpg", "Archive/写真/家族 旅行.jpg"),
        ];
        for (index, (source, destination)) in cases.iter().enumerate() {
            let fixture = transfer_fixture_with(
                &format!("matrix-unicode-{index}"),
                source,
                destination,
                b"named carefully".to_vec(),
            );

            let transfer =
                execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
                    .unwrap();

            assert_copied_and_verified(&fixture, &transfer);
            assert_eq!(transfer.destination_relative_path, *destination);
        }
    }

    #[test]
    fn deeply_nested_new_folders_are_created_for_the_copy() {
        let fixture = transfer_fixture_with(
            "matrix-nested",
            "film.mov",
            "Archive/2024/Summer/Day 1/Camera A/film.mov",
            b"nested".to_vec(),
        );
        assert!(!fixture.destination_volume.0.join("Archive").exists());

        let transfer =
            execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
                .unwrap();

        assert_copied_and_verified(&fixture, &transfer);
    }

    #[test]
    fn large_file_is_copied_and_verified_across_many_chunks() {
        // Several 1 MB chunks plus a partial one.
        let contents: Vec<u8> = (0..(7 * 1024 * 1024 + 123) as u32)
            .map(|index| (index.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        let fixture =
            transfer_fixture_with("matrix-large", "large.mov", "Archive/large.mov", contents);

        let transfer =
            execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
                .unwrap();

        assert_copied_and_verified(&fixture, &transfer);
    }

    // Adds a second planned file from the fixture's source drive to its
    // destination drive. Returns the move id, source and destination.
    fn plan_second_file(
        fixture: &mut TransferFixture,
        name: &str,
        contents: &[u8],
    ) -> (i64, PathBuf, PathBuf) {
        let source = catalogue_test_file(
            &fixture.connection,
            &fixture.source_volume.0,
            name,
            contents,
        );
        let destination_relative = format!("Archive/{name}");
        let move_id = plan_move(
            &mut fixture.connection,
            "UUID-A",
            name,
            "drive:UUID-B",
            &destination_relative,
        )
        .unwrap();
        (
            move_id,
            source,
            fixture.destination_volume.0.join(destination_relative),
        )
    }

    #[test]
    fn several_planned_files_copy_one_after_another() {
        let mut fixture = transfer_fixture("matrix-multi");
        let (second_id, second_source, second_destination) =
            plan_second_file(&mut fixture, "second.mov", b"second file");

        let first = execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
            .unwrap();
        let second =
            execute_planned_transfer(&fixture.connection, second_id, &fixture.drives).unwrap();

        assert_copied_and_verified(&fixture, &first);
        assert_eq!(second.status, "completed");
        assert_eq!(fs::read(&second_destination).unwrap(), b"second file");
        assert_eq!(fs::read(&second_source).unwrap(), b"second file");
        assert!(!planned_move_exists(&fixture.connection, second_id));
        assert_eq!(
            folder_names(fixture.destination.parent().unwrap()),
            vec!["film.mov", "second.mov"]
        );
    }

    #[test]
    fn destination_space_must_cover_the_file_exactly() {
        let mut fixture = transfer_fixture("matrix-capacity");
        let size = fixture.contents.len() as u64;

        fixture.drives[1].available_bytes = Some(size - 1);
        let error = execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
            .unwrap_err();
        assert!(error.contains("enough free space"), "{error}");
        assert_nothing_copied(&fixture);
        assert!(list_transfer_records(&fixture.connection)
            .unwrap()
            .is_empty());

        fixture.drives[1].available_bytes = Some(size);
        let transfer =
            execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
                .unwrap();
        assert_copied_and_verified(&fixture, &transfer);
    }

    #[test]
    fn cancel_before_the_first_chunk_copies_nothing() {
        let fixture = transfer_fixture("matrix-cancel-first");

        let error = execute_planned_transfer_reporting(
            &fixture.connection,
            fixture.move_id,
            &fixture.drives,
            &|_| {},
            &|| true,
        )
        .unwrap_err();

        assert_eq!(error, TRANSFER_CANCELLED);
        assert_nothing_copied(&fixture);
        // Any record made says cancelled, never failed or completed.
        for transfer in list_transfer_records(&fixture.connection).unwrap() {
            assert!(is_cancelled_record(&transfer), "{transfer:?}");
        }
    }

    fn is_cancelled_record(transfer: &TransferRecord) -> bool {
        transfer.status == "failed" && transfer.error_message.as_deref() == Some(TRANSFER_CANCELLED)
    }

    #[test]
    fn cancel_during_verification_leaves_no_copy_and_keeps_the_plan() {
        let fixture = progress_fixture("matrix-cancel-verifying");
        let verifying = std::cell::Cell::new(false);

        let error = execute_transfer_paths_reporting(
            &fixture.connection,
            fixture.move_id,
            &fixture.source,
            &fixture.destination,
            &|progress| {
                if progress.stage == TransferStage::Verifying {
                    verifying.set(true);
                }
            },
            &|| verifying.get(),
        )
        .unwrap_err();

        assert_eq!(error, TRANSFER_CANCELLED);
        assert!(verifying.get(), "the cancel arrived during verification");
        assert!(!fixture.destination.exists());
        assert!(partial_files(fixture.destination.parent().unwrap()).is_empty());
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
        assert!(planned_move_exists(&fixture.connection, fixture.move_id));
        let history = list_transfer_records(&fixture.connection).unwrap();
        assert_eq!(history.len(), 1);
        assert!(is_cancelled_record(&history[0]));
        let _ = (&fixture.database, &fixture.volume);
    }

    #[test]
    fn cancel_after_some_files_keeps_completed_copies_and_remaining_plans() {
        let mut fixture = transfer_fixture("matrix-cancel-later");
        let (second_id, second_source, second_destination) =
            plan_second_file(&mut fixture, "second.mov", b"second file");

        let first = execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
            .unwrap();
        let error = execute_planned_transfer_reporting(
            &fixture.connection,
            second_id,
            &fixture.drives,
            &|_| {},
            &|| true,
        )
        .unwrap_err();

        assert_eq!(error, TRANSFER_CANCELLED);
        assert_copied_and_verified(&fixture, &first);
        assert!(!second_destination.exists());
        assert_eq!(fs::read(&second_source).unwrap(), b"second file");
        assert!(planned_move_exists(&fixture.connection, second_id));
        assert!(partial_files(fixture.destination.parent().unwrap()).is_empty());

        let history = list_transfer_records(&fixture.connection).unwrap();
        assert_eq!(
            history
                .iter()
                .filter(|transfer| transfer.status == "completed")
                .count(),
            1
        );
        assert!(history
            .iter()
            .filter(|transfer| transfer.status != "completed")
            .all(is_cancelled_record));
    }

    // Runs the progress fixture's transfer, calling `change` with the source
    // at the first progress report of `stage`.
    fn transfer_changing_source(
        fixture: &ProgressFixture,
        stage: TransferStage,
        change: &dyn Fn(&Path),
    ) -> String {
        let changed = std::cell::Cell::new(false);
        execute_transfer_paths_reporting(
            &fixture.connection,
            fixture.move_id,
            &fixture.source,
            &fixture.destination,
            &|progress| {
                if progress.stage == stage && !changed.get() {
                    changed.set(true);
                    change(&fixture.source);
                }
            },
            &|| false,
        )
        .unwrap_err()
    }

    fn assert_failed_without_copy(fixture: &ProgressFixture) {
        assert!(!fixture.destination.exists());
        assert!(partial_files(fixture.destination.parent().unwrap()).is_empty());
        assert!(planned_move_exists(&fixture.connection, fixture.move_id));
        let history = list_transfer_records(&fixture.connection).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, "failed");
        let _ = (&fixture.database, &fixture.volume);
    }

    #[test]
    fn source_changed_during_the_copy_fails_verification() {
        let fixture = progress_fixture("matrix-source-changed-copying");
        let changed: Vec<u8> = fixture.contents.iter().map(|byte| !byte).collect();

        let error = transfer_changing_source(&fixture, TransferStage::Copying, &|source| {
            fs::write(source, &changed).unwrap()
        });

        assert!(
            error.contains("failed byte-for-byte verification"),
            "{error}"
        );
        assert_failed_without_copy(&fixture);
    }

    #[test]
    fn source_changed_during_verification_fails_verification() {
        let fixture = progress_fixture("matrix-source-changed-verifying");
        let changed: Vec<u8> = fixture.contents.iter().map(|byte| !byte).collect();

        let error = transfer_changing_source(&fixture, TransferStage::Verifying, &|source| {
            fs::write(source, &changed).unwrap()
        });

        assert!(
            error.contains("failed byte-for-byte verification"),
            "{error}"
        );
        assert_failed_without_copy(&fixture);
    }

    #[test]
    fn source_deleted_during_the_copy_is_never_completed() {
        let fixture = progress_fixture("matrix-source-deleted");

        // The open file can still be read to the end, but the copy can no
        // longer be confirmed against the source.
        transfer_changing_source(&fixture, TransferStage::Copying, &|source| {
            fs::remove_file(source).unwrap()
        });

        assert_failed_without_copy(&fixture);
    }

    #[cfg(unix)]
    #[test]
    fn read_only_destination_folder_fails_without_leaving_anything() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = transfer_fixture("matrix-read-only");
        let archive = fixture.destination.parent().unwrap().to_path_buf();
        fs::create_dir_all(&archive).unwrap();
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o555)).unwrap();

        let result =
            execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives);
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o755)).unwrap();

        let error = result.unwrap_err();
        assert!(
            error.contains("Unable to create temporary destination file"),
            "{error}"
        );
        assert_nothing_copied(&fixture);
        let history = list_transfer_records(&fixture.connection).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, "failed");
    }

    #[cfg(unix)]
    #[test]
    fn folder_or_link_at_the_temporary_path_is_never_touched() {
        let fixture = transfer_fixture("matrix-temporary-occupied");
        let archive = fixture.destination.parent().unwrap().to_path_buf();
        fs::create_dir_all(&archive).unwrap();

        // A folder holding someone's file.
        let folder = transfer_temporary_path(&fixture.destination, 1, 1).unwrap();
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("keep.txt"), b"keep").unwrap();
        let error =
            copy_file_verified_with_stage(&fixture.source, &fixture.destination, &folder, |_| {
                Ok(())
            })
            .unwrap_err();
        assert!(
            error.contains("Unable to create temporary destination file"),
            "{error}"
        );
        assert_eq!(fs::read(folder.join("keep.txt")).unwrap(), b"keep");

        // A link to a file elsewhere: neither the link nor its target change.
        let target = fixture.destination_volume.0.join("target.txt");
        fs::write(&target, b"target").unwrap();
        let link = transfer_temporary_path(&fixture.destination, 2, 2).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error =
            copy_file_verified_with_stage(&fixture.source, &fixture.destination, &link, |_| Ok(()))
                .unwrap_err();
        assert!(
            error.contains("Unable to create temporary destination file"),
            "{error}"
        );
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"target");

        assert!(!fixture.destination.exists());
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_link_at_the_destination_is_never_replaced() {
        let fixture = transfer_fixture("matrix-dangling-destination");
        let archive = fixture.destination.parent().unwrap().to_path_buf();
        fs::create_dir_all(&archive).unwrap();
        let missing = fixture.destination_volume.0.join("missing.mov");
        std::os::unix::fs::symlink(&missing, &fixture.destination).unwrap();

        // Final validation refuses it before any record is made.
        let error = execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
            .unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        assert!(list_transfer_records(&fixture.connection)
            .unwrap()
            .is_empty());

        // So does the copy itself, if it is ever reached.
        let error = copy_file_verified(&fixture.source, &fixture.destination).unwrap_err();
        assert!(error.contains("already exists"), "{error}");

        assert_eq!(fs::read_link(&fixture.destination).unwrap(), missing);
        assert!(!missing.exists());
        assert!(partial_files(&archive).is_empty());
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
    }

    #[test]
    fn planned_folder_is_refused_before_any_record_is_made() {
        let mut fixture = transfer_fixture_with(
            "matrix-folder-move",
            "Films/film.mov",
            "Archive/film.mov",
            b"inside a folder".to_vec(),
        );
        // Plan the folder itself instead of the file inside it.
        fixture
            .connection
            .execute("DELETE FROM planned_moves", [])
            .unwrap();
        let folder_move = plan_move(
            &mut fixture.connection,
            "UUID-A",
            "Films",
            "drive:UUID-B",
            "Archive/Films",
        )
        .unwrap();

        let error = execute_planned_transfer(&fixture.connection, folder_move, &fixture.drives)
            .unwrap_err();

        assert_eq!(error, FOLDER_TRANSFER_UNSUPPORTED);
        assert!(list_transfer_records(&fixture.connection)
            .unwrap()
            .is_empty());
        assert!(!fixture.destination_volume.0.join("Archive").exists());
        assert!(planned_move_exists(&fixture.connection, folder_move));
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
    }

    // Validation refuses a source that is a link, but the source could be
    // swapped for one after validation. The copy must not follow it.
    #[cfg(unix)]
    #[test]
    fn copy_never_follows_a_source_replaced_by_a_link() {
        let fixture = progress_fixture("matrix-source-link-race");
        let elsewhere = fixture.volume.0.join("elsewhere.mov");
        fs::write(&elsewhere, b"not the planned file").unwrap();
        fs::remove_file(&fixture.source).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &fixture.source).unwrap();

        let error = execute_transfer_paths(
            &fixture.connection,
            fixture.move_id,
            &fixture.source,
            &fixture.destination,
        )
        .unwrap_err();

        assert_eq!(error, SOURCE_NOT_A_FILE);
        assert!(!fixture.destination.exists());
        assert!(partial_files(fixture.destination.parent().unwrap()).is_empty());
        assert_eq!(fs::read(&elsewhere).unwrap(), b"not the planned file");
        assert!(fs::symlink_metadata(&fixture.source)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(planned_move_exists(&fixture.connection, fixture.move_id));
        let _ = &fixture.database;
    }

    // A named pipe would block an ordinary open until something wrote to it.
    #[cfg(unix)]
    #[test]
    fn copy_refuses_a_source_that_is_not_a_regular_file() {
        let fixture = progress_fixture("matrix-source-fifo");
        fs::remove_file(&fixture.source).unwrap();
        let made = std::process::Command::new("/usr/bin/mkfifo")
            .arg(&fixture.source)
            .status()
            .unwrap();
        assert!(made.success());

        let error = execute_transfer_paths(
            &fixture.connection,
            fixture.move_id,
            &fixture.source,
            &fixture.destination,
        )
        .unwrap_err();

        assert_eq!(error, SOURCE_NOT_A_FILE);
        assert!(!fixture.destination.exists());
        assert!(partial_files(fixture.destination.parent().unwrap()).is_empty());
        let _ = &fixture.database;
    }

    #[test]
    fn copy_finalised_but_not_recorded_is_reported_truthfully() {
        let fixture = transfer_fixture("matrix-bookkeeping-fails");
        // The database refuses to record the completion, as it would if it
        // stayed locked past the busy timeout or the disk filled.
        fixture
            .connection
            .execute_batch(
                "CREATE TEMP TRIGGER refuse_completion
                 BEFORE UPDATE OF status ON transfers
                 WHEN NEW.status = 'completed'
                 BEGIN SELECT RAISE(ABORT, 'database is locked'); END;",
            )
            .unwrap();

        let error = execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
            .unwrap_err();

        // The copy is in place and verified, so the error must not suggest
        // nothing was copied, and must never claim it completed.
        assert_eq!(error, COPIED_NOT_RECORDED);
        assert_eq!(fs::read(&fixture.destination).unwrap(), fixture.contents);
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
        let history = list_transfer_records(&fixture.connection).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, "verifying");
        assert!(planned_move_exists(&fixture.connection, fixture.move_id));

        // Once the database recovers, the next retry confirms the copy
        // against the source and completes it without copying again.
        fixture
            .connection
            .execute_batch("DROP TRIGGER refuse_completion;")
            .unwrap();
        let retry = execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
            .unwrap();
        assert_eq!(retry.id, history[0].id);
        assert_copied_and_verified(&fixture, &retry);
        assert_eq!(list_transfer_records(&fixture.connection).unwrap().len(), 1);
    }

    #[test]
    fn interrupted_pending_record_is_recovered_and_retried() {
        let fixture = transfer_fixture("matrix-recover-pending");
        // Killed after the record was written, before the copy began.
        let transfer = create_transfer_record(&fixture.connection, fixture.move_id).unwrap();

        let recovery = recover_interrupted_transfers(&fixture.connection, None, None).unwrap();
        assert_eq!(recovery.interrupted, 1);
        assert_eq!(
            transfer_status(&fixture.connection, transfer.id),
            (
                "failed".to_string(),
                Some(INTERRUPTED_TRANSFER_MESSAGE.to_string())
            )
        );
        assert_nothing_copied(&fixture);

        let retry = execute_planned_transfer(&fixture.connection, fixture.move_id, &fixture.drives)
            .unwrap();
        assert_copied_and_verified(&fixture, &retry);
    }

    // Each state a crash or force-quit can leave a transfer in, and the
    // status relaunch must then give it.
    #[derive(Clone, Copy, Debug)]
    enum Interruption {
        Pending,
        Copying,
        // Every byte copied, verification not finished.
        Verifying,
        // Verified and renamed into place, but `completed` not recorded.
        Finalised,
        // As Finalised, then something else changed the destination.
        FinalisedThenReplaced,
    }

    #[test]
    fn every_interrupted_state_is_recovered_truthfully_after_relaunch() {
        let cases = [
            (Interruption::Pending, "failed"),
            (Interruption::Copying, "failed"),
            (Interruption::Verifying, "failed"),
            (Interruption::Finalised, "completed"),
            (Interruption::FinalisedThenReplaced, "failed"),
        ];

        for (case, expected_status) in cases {
            let fixture = transfer_fixture(&format!("matrix-relaunch-{case:?}"));
            let replaced: Vec<u8> = fixture.contents.iter().map(|byte| !byte).collect();
            let transfer = match case {
                Interruption::Pending => {
                    create_transfer_record(&fixture.connection, fixture.move_id).unwrap()
                }
                Interruption::Copying => {
                    simulate_interrupted_transfer(&fixture, "copying", 1_000).0
                }
                Interruption::Verifying => {
                    simulate_interrupted_transfer(&fixture, "verifying", fixture.contents.len()).0
                }
                Interruption::Finalised => simulate_crash_after_rename(&fixture),
                Interruption::FinalisedThenReplaced => {
                    let transfer = simulate_crash_after_rename(&fixture);
                    fs::remove_file(&fixture.destination).unwrap();
                    fs::write(&fixture.destination, &replaced).unwrap();
                    transfer
                }
            };
            let temporary =
                transfer_temporary_path(&fixture.destination, transfer.id, transfer.created_at)
                    .unwrap();

            // Relaunch: records without drives, then with them.
            {
                let _lock = try_lock_transfers(&fixture.connection).unwrap().unwrap();
                recover_interrupted_transfers(&fixture.connection, None, None).unwrap();
                recover_interrupted_transfers(&fixture.connection, Some(&fixture.drives), None)
                    .unwrap();
            }

            let (status, _) = transfer_status(&fixture.connection, transfer.id);
            assert_eq!(status, expected_status, "{case:?}");
            assert!(!temporary.exists(), "{case:?}: partial removed");
            assert_eq!(
                fs::read(&fixture.source).unwrap(),
                fixture.contents,
                "{case:?}"
            );

            match case {
                Interruption::Finalised => {
                    assert_eq!(fs::read(&fixture.destination).unwrap(), fixture.contents);
                    assert!(!planned_move_exists(&fixture.connection, fixture.move_id));
                    // Nothing is left to retry, and nothing is copied again.
                    let error = execute_planned_transfer(
                        &fixture.connection,
                        fixture.move_id,
                        &fixture.drives,
                    )
                    .unwrap_err();
                    assert!(error.contains("no longer exists"), "{error}");
                    assert_eq!(list_transfer_records(&fixture.connection).unwrap().len(), 1);
                }
                Interruption::FinalisedThenReplaced => {
                    // Never trusted, never removed, never overwritten.
                    assert_eq!(fs::read(&fixture.destination).unwrap(), replaced);
                    assert!(planned_move_exists(&fixture.connection, fixture.move_id));
                    let error = execute_planned_transfer(
                        &fixture.connection,
                        fixture.move_id,
                        &fixture.drives,
                    )
                    .unwrap_err();
                    assert!(error.contains("already exists"), "{error}");
                    assert_eq!(fs::read(&fixture.destination).unwrap(), replaced);
                }
                _ => {
                    assert!(!fixture.destination.exists(), "{case:?}");
                    assert!(planned_move_exists(&fixture.connection, fixture.move_id));
                    let retry = execute_planned_transfer(
                        &fixture.connection,
                        fixture.move_id,
                        &fixture.drives,
                    )
                    .unwrap();
                    assert_copied_and_verified(&fixture, &retry);
                }
            }

            let completed = list_transfer_records(&fixture.connection)
                .unwrap()
                .into_iter()
                .filter(|record| record.status == "completed")
                .count();
            assert!(completed <= 1, "{case:?}: completed at most once");
            for record in list_transfer_records(&fixture.connection).unwrap() {
                assert!(
                    record.status == "completed" || record.status == "failed",
                    "{case:?}: {record:?}"
                );
            }
        }
    }

    #[test]
    fn scan_is_refused_while_a_transfer_is_running() {
        let fixture = transfer_fixture("matrix-scan-during-transfer");
        let other = open_database(&fixture.database.0).unwrap();
        let held = try_lock_transfers(&other).unwrap().expect("lock is free");

        let scan = scan_drive_job(
            fixture.database.0.clone(),
            fixture.drives[0].clone(),
            |_, _, _| {},
        );
        let error = scan().unwrap_err();

        assert!(error.contains("A transfer is running"), "{error}");
        drop(held);
        assert!(planned_move_exists(&fixture.connection, fixture.move_id));
    }

    // Detaches a disk image when the test ends, even if it fails.
    struct AttachedImage(PathBuf);

    impl Drop for AttachedImage {
        fn drop(&mut self) {
            let _ = std::process::Command::new("/usr/bin/hdiutil")
                .args(["detach", "-force"])
                .arg(&self.0)
                .output();
        }
    }

    // Fills a real, tiny volume. Opt in with `cargo test -- --ignored`: it
    // creates and mounts a disk image with hdiutil.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn copy_that_runs_out_of_space_fails_and_cleans_up() {
        let fixture = progress_fixture("matrix-out-of-space");
        let image = fixture.volume.0.join("full.dmg");
        let mount = fixture.volume.0.join("Full");
        let created = std::process::Command::new("/usr/bin/hdiutil")
            .args(["create", "-size", "3m", "-fs", "HFS+", "-volname", "MMFull"])
            .arg(&image)
            .output()
            .unwrap();
        assert!(created.status.success(), "{created:?}");
        fs::create_dir_all(&mount).unwrap();
        let attached = std::process::Command::new("/usr/bin/hdiutil")
            .args(["attach", "-nobrowse", "-mountpoint"])
            .arg(&mount)
            .arg(&image)
            .output()
            .unwrap();
        assert!(attached.status.success(), "{attached:?}");
        let _detach = AttachedImage(mount.clone());

        // The 5 MB source cannot fit on a 3 MB volume.
        let destination = mount.join("Archive/clip.mov");
        let error = execute_transfer_paths(
            &fixture.connection,
            fixture.move_id,
            &fixture.source,
            &destination,
        )
        .unwrap_err();

        assert!(!error.is_empty());
        assert!(!destination.exists());
        assert!(partial_files(destination.parent().unwrap()).is_empty());
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.contents);
        assert!(planned_move_exists(&fixture.connection, fixture.move_id));
        let history = list_transfer_records(&fixture.connection).unwrap();
        assert_eq!(history[0].status, "failed");
        let _ = &fixture.database;
    }
}
