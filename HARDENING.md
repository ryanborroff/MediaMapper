# Hardening checklist

The living checklist for taking Media Mapper from a working app to a V1 release. It covers safety, recovery, release builds, migrations, accessibility, error copy and shipping. It does not cover new features.

Ground rules for this work:

- Planning stays virtual. Nothing touches files until the user starts a copy.
- Copies are verified byte for byte before they are finalised. Originals are never modified or deleted.
- A failed or interrupted copy is never shown as completed.
- An existing destination is never overwritten.
- Cleanup only removes the exact temporary file of a known transfer record.
- When a check can't be completed, the copy doesn't go ahead.
- No deletion, cloud, NAS, AI organisation or redesign.

Status key: `[ ]` not started · `[~]` in progress · `[x]` done

## Phase 1: Baseline (2026-10-01)

Taken at `main` 66426ad (*Preview planned organisation from Plan (#34)*), with a clean working tree.

| Check | Result |
| --- | --- |
| `npm install` | OK, 0 vulnerabilities. npm warns that the install script for `fsevents` (a Vite dependency) isn't covered by `allowScripts`. |
| `npm run build` (`tsc && vite build`) | OK, no warnings. JS bundle 287 kB (84 kB gzipped). |
| `cargo fmt --check` | OK |
| `cargo test` | 80 passed, 0 failed, 0 ignored. No compiler warnings. |
| CI (`.github/workflows/ci.yml`) | Runs on macos-latest for every PR and every push to `main`: `npm ci`, `npm run build`, `cargo fmt --check`, `cargo test --lib`. No clippy, no release build, no frontend tests. |

### Shape of the code

- The whole backend is one Rust file, `src-tauri/src/lib.rs` (8.9k lines, about 3.9k of them tests), which registers 23 Tauri commands. The frontend is a single React component file, `src/App.tsx` (3k lines).
- The catalogue is SQLite (rusqlite, bundled) in WAL mode, stored at `~/Library/Application Support/com.mediamapper.app/catalogue.sqlite3`.
- Drives are found by running `diskutil`. Files are opened and revealed in Finder by running `/usr/bin/open`, and free space on this Mac is read by running `df`.
- Transfers copy one file per command. The frontend loops over the ready file moves. Folder moves can be planned and previewed, but the frontend leaves them out of execution.
- The app isn't sandboxed and has no entitlements or Info.plist additions. Tauri capabilities are `core:default`, `core:window:allow-show` and `dialog:default`.

### The copy pipeline as implemented

1. An OS file lock beside the database (`catalogue.sqlite3.transfer-lock`) allows only one transfer or scan at a time, across threads and app instances.
2. Stale records are recovered, and the exact temporary files of earlier attempts at this destination are removed.
3. A finalised copy that was confirmed during recovery is reported as complete rather than copied again.
4. Live validation and preflight run again. Only issues that concern this move block it.
5. A `transfers` row is written as a snapshot of the source and destination: `pending`, then `copying`.
6. The file is copied to `.mediamapper-transfer-<id>-<created_at>.partial` beside the destination. The partial is created with `create_new`, and writes bypass the cache (`F_NOCACHE`). Every 500 ms the copy checks by device and inode that the temporary path still leads to the open file.
7. The data is synced (`fsync`). Dates, permissions and xattrs are copied, and the record moves to `verifying`.
8. The copy is compared byte for byte with the source, reading the copy uncached.
9. `renamex_np(RENAME_EXCL)` puts the copy in place without ever replacing anything.
10. In one transaction the record becomes `completed` and the planned move is deleted.

Recovery runs at launch and before every transfer:

- Records in `pending` or `copying` become `failed`.
- A record in `verifying` with its temporary file still present becomes `failed`.
- A record in `verifying` whose temporary file is gone has its destination compared with the source again. It completes only if they are identical, and stays undetermined while either drive is unavailable.

### Safety coverage that already exists (automated)

| Area | Tests |
| --- | --- |
| Verified copy | identical result with the source untouched; verification reads from the drive, not the cache; dates, permissions and tags kept; progress order |
| No overwrite | exclusive rename; existing destination refused; destination appearing after planning or during the copy |
| Validation gate | offline source or destination; changed source; source replaced by a symlink; missing source; existing destination; local destination folder; current capacity, including the 1 GB reserve for folders on this Mac |
| Destination loss | renamed or deleted during the copy; renamed during verification |
| Cancellation | cancel during the copy leaves no partial, keeps the plan and the source, and records "Copy cancelled." |
| Interruption and recovery | interrupted copy recovered at launch, then retried; interrupted verification never completed; copy finalised before a crash is confirmed (or waits for its source, or fails if the source is gone); retry doesn't copy a finalised file again; a stale partial cleaned up on retry; recovery never removes a destination, a folder or a link; an unrelated file at the partial path is never deleted |
| Concurrency | second transfer refused by the lock; two concurrent executions of one move complete it once; reads during a scan's write transaction; completion recorded despite brief database contention |
| Migration | planned moves from before locations existed (two variants); catalogue and scan times kept; each database migrated once; concurrent first opens |
| Show in Finder / open | only the exact completed file; local-folder destinations; catalogue paths can't escape their drive |

## Gaps found in Phase 1

Each gap is classed as **Blocker**, **Should fix before V1** or **Safe to defer**. All of these classes are provisional until Phase 2 tests confirm the behaviour.

### Safety and recovery

1. **Should fix before V1.** **A folder move reaches the copy engine if the backend is called directly.** Only the frontend filters folder moves out. `execute_planned_move` would create a record and a partial, fail with "Is a directory", then clean up. That's safe, but it's luck rather than a guarantee, and it leaves a misleading failed record. The backend should refuse a folder source before it creates a record.
2. **Should fix before V1.** **The source is opened following symlinks.** Validation rejects a source that is a link, but the copy then calls `File::open(source)`, so the source could be swapped for a link between the check and the open. Opening with `O_NOFOLLOW` and checking that the handle is a regular file closes that gap.
3. **Should fix before V1.** **"Nothing was copied" can be false.** If bookkeeping fails after the exclusive rename (for example the database stays busy for more than 30 s), the command returns an error and the window reports that nothing was copied, but a verified file is at the destination. Data is safe: the record stays `verifying` and recovery confirms it later. The message is still untruthful and needs a distinct outcome.
4. **Should fix before V1.** **A cancel can be lost at the boundary between files.** `execute_planned_move` clears the cancel flag when it starts, so a Cancel that arrives just as the next file's command starts is erased and that file copies in full. This is safe, because the file is verified and counted, but it doesn't honour the user's cancel.
5. **Safe to defer.** **The copy creates the destination's parent folders with `create_dir_all`, without checking that they sit on the expected volume.** For external drives, `/Volumes` is root-owned (755), so this can't recreate a missing mount point. For a folder on this Mac, a folder deleted between validation and the copy would be recreated. The fix is to confirm that the location root exists and to create only beneath it.
6. **Safe to defer.** **A dangling symlink at the destination passes the copy's early `destination.exists()` check.** The exclusive rename still refuses it, so it fails late but safely.
7. **Should fix before V1.** **Untested cases** that are deterministic and should be automated: zero-byte file; names with spaces and Unicode (NFC and NFD); deeply nested new folders; a large file spanning many chunks; cancel before the first chunk; cancel during verification; cancel between files; source modified during the copy; source deleted mid-copy; destination folder read-only; temporary path occupied by a folder or a link; an interrupted `pending` record; bookkeeping failure after the rename (gap 3); a scan refused while a transfer runs.

### Database

8. **Should fix before V1.** **There's no explicit schema version.** Migration infers state from `PRAGMA table_info` and checks every step, so each step is idempotent. Steps aren't atomic as a whole: only the parent-path backfill and the planned-moves rebuild run in transactions. An interrupted migration is re-run safely on the next open, but this needs to be proven by tests.
9. **Should fix before V1.** **Migrations are serialised only within a process.** A second app instance (`open -n`) could race an `ALTER TABLE`. The loser would fail its open and retry, but this hasn't been tested.
10. **Should fix before V1.** **No fixtures cover historical schemas end to end.** Survival of drive labels, local-folder locations and transfer history isn't covered.
11. **Should fix before V1.** **No backup is taken before a destructive migration step.** That's the table rebuild for planned moves.

### Release and shipping

12. **Blocker.** **The bundle identifier `com.mediamapper.app` ends in `.app`.** The Tauri CLI warns about this because it conflicts with the macOS bundle extension. The app data folder, and so the catalogue, is named after the identifier, so **changing it moves the database**. That has to be decided, with a migration path for existing catalogues, before the first public build fixes the identifier forever.
13. **Should fix before V1.** **No production build has been validated.** Nobody has checked the CSP, the dialog plugin, `diskutil` and `open` from a bundled app, or the removable-volume access prompt (TCC) in a release build. That's Phase 3.
14. **Should fix before V1.** No signing, notarisation, versioning or update strategy yet. That's Phase 8.

### Error copy and accessibility

15. **Should fix before V1.** **Many backend errors reach the window raw**, as `Unable to … : <rusqlite/io error>`. Some include full paths, such as `Destination already exists: /Volumes/…`, and some include location ids, such as `Destination location … has no drive identity.` That's Phase 6.
16. **Should fix before V1.** Accessibility hasn't been audited. The folder picker is `role="dialog"` without modal focus management, and transfer progress uses `aria-live`. That's Phase 5.

## Phase 2 test plan (awaiting approval)

The full matrix will live in `SAFETY-MATRIX.md`. For each case it records the test, the expected result, whether it's automated or manual, current coverage, the result and any follow-up.

Every automated test uses the existing fixtures (`transfer_fixture`, `progress_fixture`, `simulate_interrupted_transfer`) on temporary folders and never real drives. Each one asserts these invariants:

- The source bytes are unchanged.
- No final destination file exists unless the status is `completed`, and a `completed` destination is byte-identical to the source.
- No `.partial` file remains after a cancel or failure that the app itself observed.
- The plan item remains unless the move completed.
- History is truthful.

| Group | Cases | How |
| --- | --- | --- |
| Normal | single file; multi-file loop (sequential calls); 5+ MB multi-chunk; different simulated volumes; local-folder destination; nested new folders; spaces; Unicode NFC and NFD; zero-byte; exact-fit capacity; capacity one byte short | automated |
| Cancellation | before the first chunk; mid-copy (exists); during verification; between files (a cancel set before the next call must not be lost, gap 4); after some files are complete (earlier files stay valid, the rest stay planned) | automated. The between-files case first shows the race, then guards the fix. |
| Source failure | offline before execution (exists); removed after planning (exists); changed after planning (exists); replaced by a symlink before validation (exists) and between validation and open (gap 2); remounted at a new mount point (exists); modified during the copy, so verification fails; deleted mid-copy | automated, except that a physical disconnect during the copy or verification is **manual** |
| Destination failure | offline before execution (exists); folder renamed or deleted during the copy or verification (exists); folder made read-only; destination exists before execution (exists) or appears during the copy (exists); dangling symlink at the destination (gap 6); insufficient space at validation (exists) or changed since planning (exists) | automated. ENOSPC mid-copy is semi-automated with a small `hdiutil` disk image, opt-in and `#[ignore]` by default. A physical unplug is **manual**. |
| App interruption | killed in `pending`, in `copying` (exists), in `verifying` before the rename (exists), and after the rename but before bookkeeping (exists); relaunch; retry; stale partial (exists); unrelated file at the partial path (exists); folder or link at the partial path | simulated state is automated. A real quit or force-kill (`kill -9`) during the copy and verification is **manual**, with a script. |
| Concurrency | second execution while one runs (exists); same move twice (exists); scan refused while a transfer holds the lock; reads during a scan (exists); bookkeeping contention (exists) plus contention beyond the timeout (gap 3) | automated |
| Recovery invariants | for every interrupted state: correct status after relaunch, never a false `completed`, source intact, a mismatched destination never trusted, cleanup limited to `.mediamapper-transfer-<id>-<ts>.partial`, a retry that succeeds, a finalised copy not copied twice, plan and history consistent | automated, as a table-driven test across all the states |
| Backend refusal | folder move sent straight to `execute_planned_move` is refused before a record is created (gap 1) | automated |

The fixes for gaps 1–4 come as separate small commits, each test-first. Each test is shown failing before its fix where the gap is real. Manual cases get exact steps in `SAFETY-MATRIX.md`: a real external drive, unplugging it during the copy or verification, force-quitting, then relaunching and checking that history, the plan and the disk agree.

## Later phases

- [ ] Phase 3: release build validation, and `RELEASE-TESTING.md`
- [ ] Phase 4: database and migration hardening, including fixtures for historical schemas and a written strategy
- [ ] Phase 5: accessibility and keyboard QA
- [ ] Phase 6: error message audit
- [ ] Phase 7: duplicate awareness. Needs the design approved first.
- [ ] Phase 8: shipping audit
- [ ] Phase 9: `RELEASE-READINESS.md`
