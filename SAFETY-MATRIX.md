# Safety and recovery test matrix

This matrix lists every safety and recovery case for copying planned files: what must happen, and how that is checked. Automated tests live in `src-tauri/src/lib.rs` (`mod tests`), and the names below are their function names. The newer cases are grouped under "Safety and recovery matrix".

Every automated case runs against temporary folders standing in for drives, never real drives. Each checks the same promises:

- **Source unchanged.** The source bytes are identical afterwards.
- **No false completion.** A file is at the destination only when the transfer is `completed`, and a completed copy is byte-identical to the source.
- **No stray temporaries.** No `.mediamapper-transfer-<id>-<created>.partial` is left after a failure or cancel that the app saw.
- **Plan kept.** The planned move remains unless the move completed.
- **Truthful history.** The transfer history matches what happened.

Run the suite with `cargo test --manifest-path src-tauri/Cargo.toml`. The out-of-space test mounts a disk image, so it is opt-in: `cargo test --manifest-path src-tauri/Cargo.toml -- --ignored`.

**Columns**

- **Coverage:** **Existing** = covered before this phase. **New** = added in this phase. **Fixed** = a new test that first failed and now guards a fix. **Manual** = see the manual procedures below.
- **Result:** **Pass** = passing on `release-hardening`. **Not run** = needs hardware, not yet performed.

## Normal execution

| # | Test | Expected result | How | Coverage | Result | Follow-up |
|---|---|---|---|---|---|---|
| N1 | Single file | Copied, verified, completed; plan cleared | Auto: `transfer_execution_copies_verifies_and_completes` | Existing | Pass | |
| N2 | Several files in a row | Each copied and completed independently | Auto: `several_planned_files_copy_one_after_another` | New | Pass | |
| N3 | Large file (7 MB, many 1 MB chunks) | Copied and verified across chunks | Auto: `large_file_is_copied_and_verified_across_many_chunks` | New | Pass | Multi-GB copies are in manual cases M1–M8 |
| N4 | Source and destination on different drives | Paths resolve through each drive's current mount point | Auto: `transfer_paths_use_current_external_drive_mount_points`, fixtures use two volumes | Existing | Pass | |
| N5 | Destination is a folder on this Mac | Copied into the chosen folder | Auto: `file_is_copied_into_a_folder_on_this_mac` | New | Pass | |
| N6 | Nested new destination folders | Missing folders are created; copy completes | Auto: `deeply_nested_new_folders_are_created_for_the_copy` | New | Pass | |
| N7 | Names with spaces | Exact name kept | Auto: `names_with_spaces_and_unicode_are_copied_exactly` | New | Pass | |
| N8 | Unicode names (composed, decomposed, CJK) | Exact name kept | Auto: same test as N7 | New | Pass | |
| N9 | Zero-byte file | Copied and completed | Auto: `zero_byte_file_is_copied_and_verified` | New | Pass | |
| N10 | Enough free space | Allowed | Auto: `destination_space_must_cover_the_file_exactly` | New | Pass | |
| N11 | Space exactly at the limit | Exact fit allowed; one byte short refused before any record | Auto: same test as N10; `copies_into_a_folder_on_this_mac_check_its_free_space` (keeps 1 GB free on this Mac) | New / Existing | Pass | |
| N12 | Dates, permissions and Finder tags | Carried over to the copy | Auto: `verified_copy_keeps_dates_permissions_and_finder_tags` | Existing | Pass | |
| N13 | Verification reads the drive, not the cache | The copy is read back uncached | Auto: `verified_copy_is_checked_against_the_drive_not_memory` | Existing | Pass | |

## Cancellation

| # | Test | Expected result | How | Coverage | Result | Follow-up |
|---|---|---|---|---|---|---|
| C1 | Cancel before the first file starts | Nothing copied, no record, no folder created | Auto: `cancel_requested_before_a_file_starts_copies_and_records_nothing` | Fixed | Pass | |
| C2 | Cancel before the first chunk completes | No copy, no partial; any record says cancelled | Auto: `cancel_before_the_first_chunk_copies_nothing` | New | Pass | |
| C3 | Cancel during the copy | No copy, no partial, plan kept, recorded as "Copy cancelled." | Auto: `cancelled_transfer_leaves_no_copy_and_keeps_the_plan` | Existing | Pass | |
| C4 | Cancel during verification | Same as C3 | Auto: `cancel_during_verification_leaves_no_copy_and_keeps_the_plan` | New | Pass | |
| C5 | Cancel between files | Cancel stays in force for the rest of the run, and never for a later run | Auto: `a_cancel_applies_to_its_whole_run_and_only_to_it` together with C1 | Fixed | Pass | Previously the next file's command cleared the cancel |
| C6 | Cancel after some files completed | Completed copies stay valid and completed; the rest stay planned | Auto: `cancel_after_some_files_keeps_completed_copies_and_remaining_plans` | New | Pass | |
| C7 | Originals untouched on cancel | Source bytes unchanged | Auto: asserted in C1–C6 | New | Pass | |

## Source failure

| # | Test | Expected result | How | Coverage | Result | Follow-up |
|---|---|---|---|---|---|---|
| S1 | Source drive disconnected before execution | Refused before any record | Auto: `live_validation_reports_offline_source_and_destination`, `planned_transfer_executes_only_after_final_live_validation` | Existing | Pass | |
| S2 | Source disconnected during the copy | Copy fails; partial removed; plan kept; source drive unharmed | Manual: M1 | Manual | Pass (disk image, release build; RELEASE-TESTING R17) | Error copy is raw (Phase 6). Repeat with a physical drive. |
| S3 | Source disconnected during verification | As S2; never completed | Manual: M2 | Manual | Not run | |
| S4 | Source removed after planning | Refused before any record | Auto: `transfer_refuses_a_catalogued_source_that_is_gone` | Existing | Pass | |
| S5 | Source deleted during the copy | Never completed; partial removed; plan kept | Auto: `source_deleted_during_the_copy_is_never_completed` | New | Pass | |
| S6 | Source changed after planning | Refused: rescan needed | Auto: `live_validation_detects_changed_source_file`, `planned_transfer_refuses_changed_source_before_creating_record` | Existing | Pass | |
| S7 | Source changed during the copy | Verification fails; nothing finalised | Auto: `source_changed_during_the_copy_fails_verification` | New | Pass | |
| S8 | Source changed during verification | Verification fails; nothing finalised | Auto: `source_changed_during_verification_fails_verification` | New | Pass | |
| S9 | Source replaced by a symlink before validation | Refused as "changed type" | Auto: `transfer_refuses_a_catalogued_file_replaced_by_a_link` | Existing | Pass | |
| S10 | Source replaced by a symlink after validation | Link never followed; nothing copied | Auto: `copy_never_follows_a_source_replaced_by_a_link` | Fixed | Pass | Previously the link's target was copied and completed |
| S11 | Source is a pipe or other non-file | Refused without blocking | Auto: `copy_refuses_a_source_that_is_not_a_regular_file` | New | Pass | |
| S12 | Source drive remounted at a different mount point | Current mount point used, found by volume UUID | Auto: `transfer_paths_use_current_external_drive_mount_points`; Manual: M11 | Existing / Manual | Pass / Not run | |
| S13 | Planned folder sent straight to the backend | Refused before any record or folder | Auto: `planned_folder_is_refused_before_any_record_is_made` | Fixed | Pass | Previously recorded a raw "Is a directory" failure |

## Destination failure

| # | Test | Expected result | How | Coverage | Result | Follow-up |
|---|---|---|---|---|---|---|
| D1 | Destination disconnected before execution | Refused before any record | Auto: `planned_transfer_refuses_offline_destination_before_creating_record` | Existing | Pass | |
| D2 | Destination disconnected during the copy | Stops promptly; never completed; plan kept | Auto (folder deleted or renamed): `destination_deleted_during_copy_stops_the_copy`, `destination_renamed_during_copy_stops_the_copy_promptly`; Manual: M3 | Existing / Manual | Pass / Pass (disk image, release build; RELEASE-TESTING R16) | Repeat with a physical drive |
| D3 | Destination disconnected during verification | Never completed | Auto: `destination_renamed_during_verification_is_never_completed`; Manual: M4 | Existing / Manual | Pass / Not run | |
| D4 | Destination folder disappears | As D2 | Auto: `destination_deleted_during_copy_stops_the_copy` | Existing | Pass | |
| D5 | Destination folder on this Mac removed after planning | Refused before any record | Auto: `live_validation_checks_local_destination_folder` | Existing | Pass | F2 |
| D6 | Destination folder read-only | Fails cleanly; nothing left behind; plan kept | Auto: `read_only_destination_folder_fails_without_leaving_anything`; Manual: M10 (read-only volume) | New / Manual | Pass / Not run | |
| D7 | Destination file exists before execution | Refused; existing file untouched | Auto: `transfer_refuses_a_destination_that_appeared_after_planning`, `verified_copy_refuses_existing_destination` | Existing | Pass | |
| D8 | Destination file appears during the copy | Never replaced; partial removed | Auto: `transfer_never_replaces_a_destination_that_appears_during_the_copy`, `exclusive_rename_never_replaces_existing_destination` | Existing | Pass | |
| D9 | Dangling link at the destination | Refused before copying; link untouched | Auto: `dangling_link_at_the_destination_is_never_replaced` | Fixed | Pass | Previously refused only at the final rename, after a full copy |
| D10 | Destination folder renamed or replaced during the transfer | Stops; partial removed from wherever it moved | Auto: `destination_renamed_during_copy_stops_the_copy_promptly` | Existing | Pass | |
| D11 | Not enough free space | Refused at validation | Auto: `preflight_reports_insufficient_capacity_and_missing_sources`, `destination_space_must_cover_the_file_exactly` | Existing / New | Pass | |
| D12 | Free space changes between planning and execution | Current free space checked immediately before each copy | Auto: `live_validation_checks_source_disk_and_current_capacity` | Existing | Pass | |
| D13 | Volume fills up during the copy | Copy fails; partial removed; plan kept | Auto (opt-in, real disk image): `copy_that_runs_out_of_space_fails_and_cleans_up` | New | Pass (run 2026-10-01) | Not part of CI |

## Application interruption

| # | Test | Expected result | How | Coverage | Result | Follow-up |
|---|---|---|---|---|---|---|
| A1 | Quit (⌘Q) during the copy | On relaunch: failed ("Interrupted…"), partial removed, plan kept | Manual: M5. Auto (simulated state): `interrupted_copy_is_recovered_at_startup_and_retry_succeeds` | Manual / Existing | Not run / Pass | F1 |
| A2 | Force-kill during the copy | As A1 | Manual: M6. Auto: same test as A1 | Manual / Existing | Pass (release build; RELEASE-TESTING R13) / Pass | |
| A3 | Quit during verification | As A1; never completed | Manual: M7. Auto: `interrupted_verification_is_never_marked_complete` | Manual / Existing | Pass (release build; RELEASE-TESTING R14) / Pass | ⌘Q quits at once, without asking (F1) |
| A4 | Force-kill during verification | As A3 | Manual: M8. Auto: same test as A3 | Manual / Existing | Not run / Pass | |
| A5 | Killed after the record was written, before the copy began | Failed on relaunch; retry succeeds | Auto: `interrupted_pending_record_is_recovered_and_retried` | New | Pass | |
| A6 | Relaunch after the copy completed but before bookkeeping finished | Copy compared with the source again, then completed | Auto: `copy_finalised_before_a_crash_is_confirmed_and_completed` | Existing | Pass | |
| A7 | As A6, but the source is offline or gone | Waits while offline; fails, never completes, once it is gone | Auto: `finalised_copy_waits_until_its_source_can_confirm_it`, `finalised_copy_is_not_completed_once_its_source_is_gone` | Existing | Pass | |
| A8 | Bookkeeping fails without a crash | Error says the file was copied; record stays `verifying`; retry confirms it without copying again | Auto: `copy_finalised_but_not_recorded_is_reported_truthfully` | Fixed | Pass | Previously the window said "Nothing was copied" |
| A9 | Retry an interrupted transfer | Retry succeeds; history keeps both attempts | Auto: `interrupted_copy_is_recovered_at_startup_and_retry_succeeds` | Existing | Pass | |
| A10 | Stale partial exists | Removed on retry (exact path only) | Auto: `retry_cleans_up_a_stale_partial_that_startup_could_not_reach` | Existing | Pass | |
| A11 | Unrelated file at the expected partial path | Never adopted or deleted | Auto: `failed_copy_never_deletes_a_file_already_at_its_temporary_path` | Existing | Pass | |
| A12 | Folder or link at the expected partial path | Never touched; link target unchanged | Auto: `folder_or_link_at_the_temporary_path_is_never_touched`, `recovery_never_removes_a_destination_or_anything_but_a_file` | New / Existing | Pass | |

## Concurrency

| # | Test | Expected result | How | Coverage | Result | Follow-up |
|---|---|---|---|---|---|---|
| K1 | Second execution while a transfer runs | Refused before any record | Auto: `transfer_lock_rejects_a_second_transfer` | Existing | Pass | |
| K2 | The same planned move executed twice at once | Completed exactly once | Auto: `concurrent_executions_of_one_move_complete_it_once` | Existing | Pass | |
| K3 | Reading the database while a scan runs | Reads succeed during the scan's write transaction | Auto: `read_connection_opens_while_scan_holds_write_transaction` | Existing | Pass | |
| K4 | Scan while a transfer runs | Scan refused | Auto: `scan_is_refused_while_a_transfer_is_running` | New | Pass | |
| K5 | Database contention while completing | Brief contention waited out; prolonged failure reported truthfully (A8) | Auto: `transfer_records_its_completion_despite_brief_database_contention`, `copy_finalised_but_not_recorded_is_reported_truthfully` | Existing / Fixed | Pass | |

## Recovery invariants

`every_interrupted_state_is_recovered_truthfully_after_relaunch` runs a relaunch for each state a crash can leave:

- `pending`
- `copying` with a partial file
- `verifying` with a full partial file
- finalised but not recorded
- finalised and then replaced by someone else

For each state it checks the following:

| Invariant | Result |
|---|---|
| Correct status after relaunch, and never left `pending`, `copying` or `verifying` | Pass |
| No false `completed` (only a copy re-verified against the source completes) | Pass |
| Source unchanged | Pass |
| A destination that no longer matches is never trusted, removed or overwritten | Pass |
| Only the record's exact partial file is cleaned up | Pass |
| A retry of a recoverable state copies and completes | Pass |
| A finalised copy is not copied again | Pass |
| Plan and history agree, with at most one `completed` per move | Pass |

## Manual procedures

Use a **scratch** external drive whose contents don't matter, plus a second drive or a folder on this Mac as the destination. Make a test file large enough to give you time to act, for example 4 GB of random data:

```bash
dd if=/dev/urandom of="/Volumes/<Source>/mm-test/big.bin" bs=1m count=4096
```

Scan the source drive in Media Mapper, then plan `mm-test/big.bin` to the destination.

**Checks after every manual case**

1. Relaunch Media Mapper if it isn't running, and reconnect any drive you unplugged.
2. Open **Transfers**. The entry must not say "Copied and verified" unless the copy really finished.
3. Open **Plan**. The move must still be planned unless it completed.
4. List what is left in the destination folder:
   ```bash
   ls -la "<destination folder>"
   ```
   No `.mediamapper-transfer-*.partial` may remain once the destination drive is connected and the app has opened.
5. If the file is at the destination:
   ```bash
   cmp "<source>" "<destination>"
   ```
   They must be identical, and the transfer must say completed.
6. Confirm the source file is still present and unchanged:
   ```bash
   shasum -a 256 "<source>"
   ```
   Compare against a hash taken before the test.
7. Optionally, read the raw records. Use plain `sqlite3`: `-readonly` fails on this WAL database when the app has no connection open.
   ```bash
   sqlite3 ~/Library/Application\ Support/com.mediamapper.app/catalogue.sqlite3 "SELECT id, status, error_message FROM transfers ORDER BY id DESC LIMIT 5"
   ```

**Cases**

| # | Steps | Expected |
|---|---|---|
| M1 | Copy planned files. While it shows **Copying**, unplug the **source** drive. | The run stops with an error. History shows Failed. Once the destination is checked, no partial remains. The plan is kept. After reconnecting, the source is intact. |
| M2 | As M1, but unplug the source while it shows **Verifying**. | As M1, never Copied and verified. |
| M3 | Copy planned files. While it shows **Copying**, unplug the **destination** drive. | The run stops with "The destination … is no longer available". The plan is kept. After reconnecting, relaunch: no partial remains on the destination. |
| M4 | As M3, during **Verifying**. | As M3. Nothing named `big.bin` at the destination. |
| M5 | Copy planned files. During **Copying**, quit with ⌘Q. Relaunch. | History shows Failed ("Interrupted before verification completed."). The partial is removed. Plan kept. Copy again: succeeds. |
| M6 | As M5, but force-kill instead of quitting: `pkill -9 -f "Media Mapper"`. | As M5. |
| M7 | As M5, quitting during **Verifying**. | As M5. Never Copied and verified. |
| M8 | As M6, killing during **Verifying**. | As M5. |
| M9 | Start a copy, then try **Scan** on any drive. | The scan is refused while copying. |
| M10 | Make a read-only destination with `hdiutil create -size 50m -fs APFS -volname MMReadOnly ro.dmg`, then `hdiutil attach -readonly ro.dmg`. Scan it, plan a small file to it, then copy. | Fails cleanly. Nothing is created. The plan is kept. |
| M11 | Plan a move. Eject the source, connect another volume with the same name first (it takes `/Volumes/<Name>`), then reconnect the source (it mounts as `/Volumes/<Name> 1`). Copy. | Media Mapper copies from the correct drive, matched by volume UUID, never from the impostor. |

## Follow-ups

| # | Item | Class |
|---|---|---|
| F1 | Quitting during a copy isn't confirmed. Quitting is safe, since recovery handles it, but a desktop app usually asks before abandoning work in progress. | Safe to defer |
| F2 | The copy creates the destination's parent folders with `create_dir_all`. If a folder on this Mac is deleted between the final validation and the copy, that folder would be recreated. For external drives, `/Volumes` is root-owned, so a missing mount point can't be recreated. The fix is to create folders only beneath an existing location root. | Safe to defer |
| F3 | Source and destination I/O failures show raw OS errors, such as `Unable to copy file: Input/output error (os error 5)`. | Phase 6 |
| F4 | Verification compares against the source as it is when read. A source rewritten after the final validation and before the copy is opened, without changing its size or date, is copied as it now is. Validation compares size and date with the catalogue, not contents. | Known limitation |
| F5 | M1, M3, M6 and M7 passed on the release build using disk images ([RELEASE-TESTING.md](RELEASE-TESTING.md)). M1–M11 still need a run with physical drives. | Before V1 |
