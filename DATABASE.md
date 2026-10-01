# Catalogue database

A catalogue can hold hours of scanning, the user's plan and their transfer history. It is user data. This document covers where it lives, how its schema changes, and what protects it while it does.

## Where it lives

`~/Library/Application Support/com.mediamapper.app/catalogue.sqlite3`. It's a SQLite database in WAL mode, so `catalogue.sqlite3-wal` and `catalogue.sqlite3-shm` sit beside it while the app is open. `catalogue.sqlite3.transfer-lock` is the lock that allows only one transfer or scan at a time.

The folder is named after the bundle identifier. Changing the identifier moves the catalogue, so a change needs a step that finds the old catalogue. See HARDENING.md gap 12.

Development builds and release builds use the same catalogue (RELEASE-TESTING.md P3-9). Running a branch that changes the schema upgrades the developer's real catalogue.

## Tables

| Table | Holds |
|---|---|
| `drives` | One row per catalogued drive, keyed by volume UUID: name, capacity, last mount point, when it was last seen and scanned, and summary totals |
| `files` | The catalogue entries for each drive: path, name, parent folder, size, date, and whether a folder couldn't be read |
| `locations` | Where files can be planned to: every drive (`drive:<uuid>`), and folders on this Mac. Holds user labels. |
| `planned_moves` | The plan: source file and destination location and path |
| `transfers` | Transfer history: a snapshot of each copy attempt's source, destination and outcome |

## Schema versions

The schema version is SQLite's `PRAGMA user_version`. `SCHEMA_VERSION` in `src-tauri/src/lib.rs` is the version this build writes.

| Version | Build | What it is |
|---|---|---|
| 0 | Every build before `ed3f730` | No version stored. The schema could be any of the eight shapes below. |
| 1 | `ed3f730` | The schema as of `main` 66426ad, with a version |

The shapes that version 0 covers, each of which has a test fixture:

| Commit | Change |
|---|---|
| `6e7d838` | First catalogue: `drives`, `files` |
| `a66b9e3` | Drive summary totals (`048dfaa`) and parent folders on entries |
| `7b80a78` | `planned_moves`, pointing straight at a destination drive |
| `c77c108` | `locations`, without labels |
| `e46eacb` | Planned moves point at locations, including folders on this Mac |
| `5a6f81a` | Location labels |
| `759b5aa` | Unreadable folders |
| `66426ad` | `transfers` (added in `a180b23`, unchanged since) |

## How an upgrade runs

`open_database` upgrades a catalogue the first time each run of the app opens it:

1. **Read the version.** This takes no write lock, so a current catalogue opens without writing, even during a scan.
2. **Refuse a newer catalogue.** If the version is higher than this build's, nothing is changed and the user is told to use the newer version. Builds from before version 1 don't check this, so they still open later catalogues.
3. **Back up.** Before the first upgrade to each version, an existing catalogue is copied with `VACUUM INTO` to `catalogue-before-schema-<n>.sqlite3` in the same folder. The copy is written under a temporary name, then hard-linked into place, which never replaces a file. A backup that already exists is kept: it predates an earlier attempt at the same upgrade. If the backup can't be made, the catalogue isn't upgraded. Backups are kept and never removed by the app. Each is a compacted copy, so usually much smaller than the catalogue.
4. **Upgrade in one transaction.** `BEGIN IMMEDIATE` takes the write lock before anything is read. A second copy of the app waits until the first one finishes, then finds the catalogue already current. Every step of `migrate_schema` checks before it writes, then the version is stamped, all in that one transaction.
5. **Commit.** If any step fails, or the app is killed part way, SQLite rolls the whole transaction back. The catalogue is exactly as it was, and the next open tries again.

Drive locations are created at startup by `sync_drive_locations`, after the upgrade.

## Changing the schema

1. Add the step at the end of `migrate_schema`. It must check before it writes, as the existing steps do, and must not start its own transaction.
2. Prefer adding tables and columns. When a table has to be rebuilt, copy its rows by column name inside the same transaction, as the `planned_moves` rebuild does, and never drop data.
3. Increase `SCHEMA_VERSION`.
4. Add a fixture for the previous version to `historical_fixtures` in the tests, and say in `Holds` what it contains.
5. Add a row to the version table above.

Never rebuild a catalogue from scratch when it can be upgraded in place.

## Tests

In `src-tauri/src/lib.rs`, `mod tests`:

| Test | Checks |
|---|---|
| `every_historical_catalogue_upgrades_keeping_its_data` | Each of the eight historical shapes upgrades to exactly the current schema. Drives, capacity, mount points, scan and connection times, totals, every entry with its exact name and parent folder, unreadable markers, locations, labels, folders on this Mac, planned moves, transfer history (including a cancel and an unfinished record), foreign keys and integrity are all kept. Reopening changes nothing. |
| `a_failed_upgrade_leaves_the_catalogue_exactly_as_it_was` | An upgrade that fails at its last step keeps none of its earlier steps, and succeeds in full later |
| `two_app_instances_upgrading_at_once_both_succeed` | Six connections upgrading at once, without the in-app lock, all succeed |
| `a_catalogue_from_a_newer_version_is_left_untouched` | Refused, unchanged, no backup |
| `an_existing_catalogue_is_backed_up_once_before_its_upgrade` | The backup holds the old schema and data, leaves no temporary files, and never replaces an existing backup |
| `a_new_or_current_catalogue_is_not_backed_up` | No backup for a new or current catalogue |
| `migrates_planned_moves_*`, `concurrent_first_opens_*`, `schema_is_migrated_once_per_database`, `existing_database_migrates_keeping_catalogue_and_scan_times` | Earlier tests for the planned-move rebuild and once-per-run upgrades |

The first two new tests failed when the upgrade ran without its transaction, as it did before `ed3f730`.

## Known limitations

- **The catalogue never shrinks.** It isn't vacuumed, so space freed when a drive is rescanned or removed stays in the file. A copy of a real catalogue was 500 MB, and 99.7% of its pages were free. This is harmless but wasteful. An occasional `VACUUM` or `PRAGMA auto_vacuum = INCREMENTAL` would fix it. Safe to defer.
- **Interrupted backups.** If the app is killed while writing a backup, a `catalogue-before-schema-<n>.sqlite3.<pid>-<n>.partial` file is left beside the catalogue. It's never mistaken for a backup, and the next upgrade writes a new one. Nothing removes it.
- **The identifier is undecided.** See "Where it lives".
