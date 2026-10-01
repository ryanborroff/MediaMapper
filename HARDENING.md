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

8. *Fixed in Phase 4.* **Should fix before V1.** **There's no explicit schema version.** Migration infers state from `PRAGMA table_info` and checks every step, so each step is idempotent. Steps aren't atomic as a whole: only the parent-path backfill and the planned-moves rebuild run in transactions. An interrupted migration is re-run safely on the next open, but this needs to be proven by tests.
9. *Fixed in Phase 4.* **Should fix before V1.** **Migrations are serialised only within a process.** A second app instance (`open -n`) could race an `ALTER TABLE`. The loser would fail its open and retry, but this hasn't been tested.
10. *Fixed in Phase 4.* **Should fix before V1.** **No fixtures cover historical schemas end to end.** Survival of drive labels, local-folder locations and transfer history isn't covered.
11. *Fixed in Phase 4.* **Should fix before V1.** **No backup is taken before a destructive migration step.** That's the table rebuild for planned moves.

### Release and shipping

12. **Blocker.** **The bundle identifier `com.mediamapper.app` ends in `.app`.** The Tauri CLI warns about this because it conflicts with the macOS bundle extension. The app data folder, and so the catalogue, is named after the identifier, so **changing it moves the database**. That has to be decided, with a migration path for existing catalogues, before the first public build fixes the identifier forever.
13. **Should fix before V1.** **No production build has been validated.** Nobody has checked the CSP, the dialog plugin, `diskutil` and `open` from a bundled app, or the removable-volume access prompt (TCC) in a release build. That's Phase 3.
14. **Should fix before V1.** No signing, notarisation, versioning or update strategy yet. That's Phase 8.

### Error copy and accessibility

15. **Should fix before V1.** **Many backend errors reach the window raw**, as `Unable to … : <rusqlite/io error>`. Some include full paths, such as `Destination already exists: /Volumes/…`, and some include location ids, such as `Destination location … has no drive identity.` That's Phase 6.
16. **Should fix before V1.** Accessibility hasn't been audited. The folder picker is `role="dialog"` without modal focus management, and transfer progress uses `aria-live`. That's Phase 5.

## Phase 2: Safety and recovery matrix (done 2026-10-01)

The full matrix is in [SAFETY-MATRIX.md](SAFETY-MATRIX.md). It covers 63 cases with expected results, the tests covering them, results and follow-ups, plus exact steps for the 11 manual cases that need real drives.

- [x] 26 new automated tests. Rust tests went from 80 to 105, plus 1 opt-in disk-image test. All pass.
- [x] Gap 1 fixed (`08bc9e7`): folder moves are refused by the backend before any record is made.
- [x] Gap 2 fixed (`1b6db0d`): transfer sources are opened without following links, and must be regular files. Before this, a source swapped for a link after validation had the link's target copied and completed.
- [x] Gap 3 fixed (`de3f8de`): a verified copy whose completion couldn't be saved is reported as copied, not as "Nothing was copied".
- [x] Gap 4 fixed (`9379763`): a cancel is tied to its copy run, so it can't be cleared as the next file starts. A file whose run is already cancelled makes no record.
- [x] Gap 6 fixed (`ff51ef9`): a dangling link at the destination is refused before copying.
- [ ] Gap 5 deferred. See SAFETY-MATRIX F2.
- [ ] Manual cases M1–M11 need a session with real drives. See SAFETY-MATRIX F5.

## Phase 3: Release build validation (done 2026-10-01)

The repeatable procedure and its results are in [RELEASE-TESTING.md](RELEASE-TESTING.md). `npm run tauri build` produces a working unsigned `.app` and `.dmg`.

The bundled app was driven end to end on disk images, with a separate home folder so the real catalogue was never opened. These all passed:

- drive detection and scanning
- browsing, including offline
- search
- planning, preview and capacity checks
- copying and verifying to a drive and to a folder on this Mac
- history and Show in Finder
- cancel
- force-kill and ⌘Q mid-copy, then recovery and retry
- destination and source ejected mid-copy
- persistence across relaunches

Still to do:

- [ ] Section 5 of RELEASE-TESTING.md (first launch from Finder, macOS privacy prompts) needs a person at the Mac.
- [ ] Opening a file (R10) needs a run by hand.

Findings P3-1 to P3-11 are listed in RELEASE-TESTING.md. They are:

- **Blockers:** the identifier, and the invalid bundle signature for distribution.
- **Should fix before V1:**
  - choosing arm64 or universal
  - privacy usage descriptions
  - stderr-only logging
  - the contradictory "Space after transfer"
  - the minimum macOS version
- **Phase 5:** VoiceOver can't reach the row buttons in Browse.
- **Phase 6:** raw OS errors, "folder" for a drive, "1 files" and "1 results".

## Phase 4: Database and migrations (done 2026-10-01)

The strategy, the version history and the tests are in [DATABASE.md](DATABASE.md).

What the audit found:

- **The version was inferred from the schema.** It was never stored.
- **Upgrades weren't atomic** (gap 8). Each step committed on its own. If the app was interrupted between adding the parent-folder column and filling it in, the next open skipped the fill. Every entry then sat at the top of its drive, permanently. The summary totals had the same flaw. Only the developer's catalogue went through those steps, but the pattern would have repeated with every future migration.
- **Two app instances could race an upgrade** (gap 9). Upgrades were serialised only within one process.
- **No fixtures for historical schemas** (gap 10), and **no backup** (gap 11).
- **The transfers table has never changed.** History written by any build reads correctly.

What changed (`ed3f730`):

- [x] The schema has a version in `PRAGMA user_version`, now 1. A current catalogue opens without writing, as before.
- [x] An upgrade runs in one `BEGIN IMMEDIATE` transaction. A failure or crash keeps nothing. A second instance waits and then finds the catalogue current. Gaps 8 and 9 are fixed.
- [x] A catalogue from a newer version is refused and left untouched.
- [x] Before the first upgrade to each version, an existing catalogue is copied to `catalogue-before-schema-<n>.sqlite3`. The copy never replaces a file, and the upgrade doesn't run without it. Gap 11 is fixed.
- [x] Fixtures for all eight on-disk shapes, from `6e7d838` to `66426ad`, are each upgraded and checked field by field. Gap 10 is fixed.
- [x] 6 new tests. Rust tests went from 105 to 111, plus 1 opt-in test, and all pass. The atomicity and two-instance tests failed against the old, non-transactional upgrade.
- [x] A copy of the developer's real catalogue upgraded cleanly, taking 0.03 s. Every count was kept: 1 drive, 1,166 entries, 4 locations, 1 label, 2 planned moves and 28 transfers. Scan times were kept, and the integrity check passed. The real catalogue was only read, to take that copy.

Still open:

- [ ] Gap 12 (the identifier, and so where the catalogue lives) is still a **Blocker** that needs a decision. It belongs with Phase 8. The fix will need a step that finds the catalogue in the old folder.
- [ ] **Safe to defer.** The catalogue never gives back free space. The real copy was 500 MB with 99.7% free pages.

## Later phases

- [x] Phase 1: baseline
- [x] Phase 2: safety and recovery matrix, except the manual runs
- [x] Phase 3: release build validation, and `RELEASE-TESTING.md` (except the Finder-launch permission checks)
- [x] Phase 4: database and migration hardening, and `DATABASE.md`
- [ ] Phase 5: accessibility and keyboard QA
- [ ] Phase 6: error message audit
- [ ] Phase 7: duplicate awareness. Needs the design approved first.
- [ ] Phase 8: shipping audit
- [ ] Phase 9: `RELEASE-READINESS.md`
