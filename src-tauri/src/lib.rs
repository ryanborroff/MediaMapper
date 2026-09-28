use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
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

#[cfg(target_os = "macos")]
fn external_drives() -> Result<Vec<DriveInfo>, String> {
    use std::collections::HashMap;
    use std::fs;
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
    let entries = fs::read_dir(volumes).map_err(|error| format!("Unable to read /Volumes: {error}"))?;
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
        if info.get("Protocol").is_some_and(|protocol| protocol.eq_ignore_ascii_case("Network")) {
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![list_external_drives])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
