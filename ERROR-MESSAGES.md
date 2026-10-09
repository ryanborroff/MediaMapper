# Error messages

What Tidy Drives says when something goes wrong, and how internal failures become those words. Every message says what happened and what to do next. Messages don't show SQLite or macOS error text, internal ids or full paths. Serious failures aren't made to sound harmless, a cancel isn't called a failure, and waiting for a drive isn't treated as an error.

## How it works

Internal failures are built as `Unable to <step>: <detail>`, where the detail comes from SQLite or macOS. Every command passes its error through `present_error` (`src-tauri/src/lib.rs`):

- **A message already written for the window** passes through unchanged.
- **An internal failure** is written to the diagnostics log, and the window is shown a plain message instead. It is chosen from:
  1. the catalogue's state: busy, damaged, or out of space;
  2. the macOS error number, together with whether the failing step read the **source** or wrote the **destination**;
  3. otherwise, what the user was doing: "Tidy Drives couldn't *open this folder*. Try again. If it keeps happening, quit and reopen Tidy Drives."

Transfer history keeps the recorded error, so the log and the record agree. It's shown with the same wording as above, through `present_transfer_error`.

**Diagnostics log:** `~/Library/Logs/com.ryanborroff.tidydrives/tidy-drives.log`, plus stderr. Each line is a Unix time and the full technical detail. At 1 MB the log moves to `media-mapper.log.1`, replacing the previous one. Before Phase 6, diagnostics went only to stderr, which is lost when the app is opened from Finder (RELEASE-TESTING P3-5).

## Messages by category

| Category | When | Message |
|---|---|---|
| Drive unavailable | Scanning a drive that was just unplugged | That drive is no longer connected. Reconnect it, then scan again. |
| | Unplugged as the scan starts | The drive was disconnected. Reconnect it, then scan again. |
| | Plan: the source drive is offline | The drive that holds *file* isn't connected. (Plan also says "Waiting for *drive*") |
| | Plan: the destination drive is offline | The destination "*name*" isn't connected. Connect it to copy. |
| | No permanent identity | This drive doesn't report a permanent identity, so Tidy Drives can't recognise it reliably and won't catalogue it. |
| Source unavailable | Source drive offline at copy time | The source drive isn't connected. Connect it, then copy again. |
| | Source unplugged or failing mid-copy (EIO, ENXIO, ENODEV) | The source drive stopped responding or was disconnected during the copy. Reconnect it, then copy again. |
| | Source file gone (ENOENT) | The source file is no longer there. Rescan the source drive to update the catalogue. |
| | Plan: gone from disk or catalogue | *file* is in the catalogue but no longer on the source drive. Rescan the drive, or remove it from the plan. / *file* is no longer in the catalogue. Rescan its drive, or remove it from the plan. |
| Destination unavailable | Destination drive or folder lost mid-copy | "*name*" became unavailable during the copy. Reconnect it, then copy again. |
| | Destination failing mid-copy (EIO and similar) | The destination stopped responding or was disconnected during the copy. Reconnect it, then copy again. |
| | Plan: the destination folder is gone | The destination folder "*name*" is no longer available. Check that it still exists, or choose another destination. |
| | Plan: the location was removed | The destination for *file* is no longer in Tidy Drives. Remove it from the plan, then add it again. |
| File changed | Plan: changed since the scan | *file* has changed since it was catalogued. Rescan the source drive before copying. |
| | Plan: the type changed | *file* has changed type since it was catalogued. Rescan the source drive before copying. |
| | Copy: no longer a regular file | The source is no longer a file, so it was not copied. Rescan the source drive to update the catalogue. |
| Destination exists | Before copying | There's already a file at the destination. Tidy Drives never replaces existing files, so nothing was copied. Remove it from the plan, or choose another destination. |
| | Appeared during the copy | A file appeared at the destination during the copy. Tidy Drives never replaces existing files, so the copy was discarded and that file was left as it is. |
| | Plan | *file* already exists at the planned destination. Tidy Drives never replaces existing files, so remove it from the plan or choose another destination. |
| Insufficient space | Destination full mid-copy (ENOSPC) | The destination ran out of space during the copy. Free some space there, then copy again. |
| | This Mac full when saving the catalogue | This Mac is out of space, so Tidy Drives couldn't save its catalogue. Free some space, then try again. |
| | Plan | *name* doesn't currently have enough free space… / didn't have enough free space for the planned files at its last scan. Free some space there, or plan fewer files. |
| Permission | Source not readable (EACCES, EPERM) | macOS didn't let Tidy Drives read the source file. Check its permissions in Finder, then copy again. |
| | Destination not writable | macOS didn't let Tidy Drives add files to the destination. Check that you can add files there in Finder, then copy again. |
| | Read-only destination (EROFS) | The destination is read-only, so nothing can be copied to it. Choose a destination you can add files to. |
| | Anything else | macOS didn't let Tidy Drives *do this*. Check the permissions in Finder, or in System Settings › Privacy & Security › Files & Folders, then try again. |
| Catalogue | Busy, for example during a scan | Tidy Drives is still finishing another task, such as a scan. Try again in a moment. |
| | Damaged | The Tidy Drives catalogue is damaged and can't be read. The files on your drives aren't affected. Quit Tidy Drives, and keep its catalogue folder (Library › Application Support › com.ryanborroff.tidydrives) before trying anything else. |
| | From a newer version | This catalogue was updated by a newer version of Tidy Drives, so this version can't use it. Open it with the newer version of Tidy Drives. |
| | Backup failed before an upgrade | Tidy Drives couldn't back up your catalogue before updating it, so it was left unchanged. Check that this Mac has free space, then reopen Tidy Drives. |
| Scan | A transfer is running | A transfer is running. Wait for it to finish, then scan the drive. |
| | Folders couldn't be read (shown as a note, not an error) | Couldn't read 3 folders, including *folder*. Their contents are missing from the catalogue. |
| Copy | Any other failure | Tidy Drives couldn't copy the file. The original is untouched. Try again. If it keeps happening, quit and reopen Tidy Drives. The window adds "Nothing was copied.", or how many files were copied first. |
| Verification | The copy doesn't match | The copy didn't match the original when it was checked, so it was discarded. The original is untouched. Copy again. If it happens again, the source or destination drive may be failing. |
| Recovery | History: the app stopped mid-copy | Tidy Drives stopped before this copy was verified, so the copy wasn't kept. |
| | Copied but not recorded | The file was copied and verified, but Tidy Drives couldn't save that it finished. It will check the copy again the next time it opens. |
| File opening | No app for this type | No app on this Mac can open this type of file. |
| | Drive offline | Connect *drive* to open this file. |
| | Not on disk | The file isn't where the catalogue says. It may have been moved or deleted since the last scan. Rescan the drive to update the catalogue. |
| | A link out of the drive | That file points outside its drive, so it was not opened. |
| Show in Finder | Destination offline | Connect the destination drive to show this file in Finder. |
| | Moved since | Finder couldn't show the file. It may have been moved or renamed since it was copied. |
| Cancellation, not a failure | Copy | History: **Cancelled**. Window: "Transfer cancelled. No files were copied. Planned transfers are unchanged." |
| | Scan | Scan cancelled. |

Planning refusals follow the same pattern. Examples: "Another planned file is already going to that destination. Choose a different folder." and "A folder can't be planned inside itself. Choose a different destination."

## Wording the window depends on

The window recognises a few backend messages by their wording: "Copy cancelled.", the copied-but-not-recorded message, "The destination became unavailable during the copy." and "Scan cancelled.". `messages_written_for_the_window_pass_through` pins them, so changing one fails a test.

## Tests

| Test | Checks |
|---|---|
| `internal_errors_become_plain_messages` | 18 internal failures map to the right message, with the source and destination told apart. No message contains "os error", "SQLite", "database", "Unable to", `/Volumes`, `/Users`, `drive:`, "UUID" or "panicked". |
| `messages_written_for_the_window_pass_through` | User messages are unchanged, and the wording the window depends on is pinned |
| `transfer_history_shows_failures_in_plain_words` | History rewrites old raw errors and the interrupted-copy record |
| `plan_issues_name_files_not_paths_or_ids` | Plan issues name the file, not its path, and contain no internal ids |
| `the_diagnostics_log_appends_and_rolls_over` | The log appends and rolls over at 1 MB |
| `copy_that_runs_out_of_space_fails_and_cleans_up` (opt-in, `--ignored`) | A real full disk image gives "The destination ran out of space during the copy…" |

## Remaining

| Issue | Class |
|---|---|
| The window shows errors in one banner at the top. Errors from Plan and Browse actions can be off-screen in a long view. The plan panel and drag-and-drop already show theirs in place. | Safe to defer |
| A busy catalogue appears in the red error banner, although it only means "wait". It's rare, because commands wait 2 s for the lock first. | Safe to defer |
| The log records Unix times, not dates | Safe to defer |
