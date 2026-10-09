# Release readiness

Where Media Mapper stands for V1, as of 2026-10-07 on `release-hardening` (`e7948de` plus this report). The supporting detail is in the documents linked from each section; this report doesn't repeat them.

**Verdict.** The app itself is ready. The copy pipeline, recovery, database upgrades, error wording, accessibility fixes and duplicate awareness are built, tested and checked in the release build. What stands between it and a public release is mostly not code:

- a Developer ID certificate;
- a short round of testing by hand with real drives and a Finder launch.

All remaining issues are classified in [section 11](#11-remaining-blockers).

## 1. Product architecture

Media Mapper is a macOS desktop app built with Tauri 2. The workflow is: browse what you have, plan what you want, preview the result, copy and verify, then see what happened.

| Part | What it is |
|---|---|
| Backend | Rust, in `src-tauri/src/lib.rs`: about 6,100 lines of code and 6,800 of tests. It exposes 26 Tauri commands. |
| Frontend | React 19 and TypeScript in `src/App.tsx` (3,600 lines), with `App.css`. Built with Vite. |
| Data | One SQLite catalogue in WAL mode, at `~/Library/Application Support/com.ryanborroff.mediamapper/catalogue.sqlite3`. Schema version 2. See [DATABASE.md](DATABASE.md). |
| macOS tools | `diskutil` (finding drives and their capacity), `df` (free space on this Mac), `open` (opening files, and Show in Finder) |
| Diagnostics | `~/Library/Logs/com.ryanborroff.mediamapper/media-mapper.log`, rolling over at 1 MB |
| Permissions | Not sandboxed. Tauri capabilities: `core:default`, `core:window:allow-show`, `dialog:default`. Privacy usage descriptions for removable volumes and the Documents, Desktop and Downloads folders. |

Planning is virtual: plans are rows in the catalogue, and nothing on any drive changes until the user chooses **Copy and verify**. Scanning and duplicate checks only read.

## 2. Safety guarantees

These hold in the code and are each covered by tests ([SAFETY-MATRIX.md](SAFETY-MATRIX.md)):

1. **Originals are never modified, moved or deleted.** Media Mapper has no code that deletes, renames or writes a source file. Deletion is outside the product.
2. **Nothing changes until an explicit copy.** Planning, previews, scanning and duplicate checks don't write to any drive.
3. **Copies are checked before they count.**
   - Each file is written to a temporary `.mediamapper-transfer-<id>-<created>.partial` beside its destination, with writes bypassing the cache, and synced to disk.
   - It is then compared byte for byte with the source, reading the copy back from the drive rather than from memory.
   - Only after that is it put in place, with an exclusive rename.
4. **An existing destination is never overwritten.**
   - The final rename (`renamex_np` with `RENAME_EXCL`) fails rather than replacing anything.
   - Anything at the destination path is refused before copying, including a broken link or a file that appears mid-copy.
5. **A failed, cancelled or interrupted copy is never shown as completed.**
   - Only a copy that is verified, in place and recorded is completed.
   - A crash after the rename is completed only after recovery compares the copy with the source again.
6. **Final checks run immediately before each file.** Drives must be connected, the source unchanged since its scan, the destination clear, and free space current, including a 1 GB reserve when copying to this Mac.
7. **The source can't be swapped.** Sources are opened without following links and must be regular files. A folder move can't reach the copy engine.
8. **One copy, scan or content check at a time,** across threads and app instances, using an OS file lock.
9. **Cleanup is narrow.** Only the exact temporary file of a known transfer record is ever removed. A folder, link or unrelated file at that path is left alone.
10. **The catalogue is protected while it upgrades.**
    - Upgrades are transactional and versioned.
    - The catalogue is backed up before each new schema version.
    - A catalogue from a newer version is refused, not changed.

## 3. Automated test coverage

| | |
|---|---|
| Rust tests | **139 pass**, plus 1 opt-in test that fills a real disk image (`cargo test -- --ignored`). There were 80 at the start of hardening. Repeated full runs pass. |
| Frontend | Type check and production build (`npm run build`). There are no frontend unit tests. |
| CI | GitHub Actions on macOS for every PR and every push to `main`: build, `cargo fmt --check`, `cargo test --lib` |
| Last run | 2026-10-07: build, formatting and all 139 tests pass |

What's covered:

- **Copying:**
  - **Normal copies:** zero-byte, large, Unicode and nested files, folders on this Mac, and exact-fit capacity.
  - **Cancellation:** at every stage, and between files.
  - **Source and destination failures:** changes, deletions, links, read-only folders, loss mid-copy and running out of space.
  - **Interruption and recovery:** from every state a crash can leave.
  - **Concurrency:** including the transfer lock while processes start.
- **Database:** upgrades from all nine historical schemas, a failed upgrade rolling back, two instances upgrading at once, newer catalogues, and backups.
- **Error wording:** the main error translations.
- **Duplicates:** probable grouping, content checks, identical grouping, stale checks, cancel, and read-only behaviour.

## 4. Manual test matrix

| What | Where | Status |
|---|---|---|
| Release smoke test R1–R21: scan, browse, plan, preview, copy, cancel, kill, recover, eject mid-copy, offline browsing, search | [RELEASE-TESTING.md](RELEASE-TESTING.md) | **Done** 2026-10-01 on the release build with disk images, except R10 (opening a file), which needs a run by hand |
| Duplicate check, end to end | HARDENING.md, Phase 7 | **Done** 2026-10-02 on the release build. Identical, renamed, same-size-but-different and small files all handled correctly. Files unchanged. |
| Hardened runtime (needed for notarisation) | [SHIPPING.md](SHIPPING.md) | **Done** 2026-10-07: launched, scanned and browsed with no entitlements |
| Physical drives unplugged mid-copy and mid-verification, quit and force-kill during a copy, read-only volume, remount at a new path (M1–M11) | SAFETY-MATRIX.md | M1, M3, M6 and M7 passed **with disk images**. **All still need a run with real drives.** |
| First launch from Finder: privacy prompts for drives and folders | RELEASE-TESTING.md section 5 | **Not run.** Needs a person at the Mac. |
| VoiceOver checklist in the app | [ACCESSIBILITY.md](ACCESSIBILITY.md) | **Not run.** Needs a person at the Mac. |

## 5. Recovery guarantees

After any interruption, the next launch, or the next copy attempt, resolves every unfinished transfer:

| Left in | Becomes | Disk |
|---|---|---|
| `pending` or `copying` | Failed: "Media Mapper stopped before this copy was verified, so the copy wasn't kept." | Its partial file is removed once its destination is connected |
| `verifying`, before the rename | Failed, as above | Partial removed |
| `verifying`, after the rename | Completed, **only** if the copy still matches the source byte for byte. Left waiting while either drive is offline. Failed if the source is gone or the copy differs. | A copy that doesn't match is never trusted, removed or overwritten |

- A retry then copies normally. A copy that was already finalised is never copied twice.
- Completing a copy and clearing its plan happen in one transaction.
- If recording a finished copy fails, the window says the file was copied, and recovery confirms it later.
- Proven on the release build:
  - force-kill and ⌘Q mid-copy;
  - ⌘Q during verification;
  - destination and source ejected mid-copy;
  - relaunching and retrying after each.

## 6. Known limitations

These are accepted for V1:

- **No folder copies.** Folder moves can be planned and previewed, but only files are copied. The window says so.
- **The destination isn't updated after a copy.** Its catalogue doesn't show copied files until it is rescanned.
- **Validation compares size and date, not contents.** A source rewritten after the final check, with its size and date unchanged, is copied as it now is.
- **Duplicates have limits:**
  - "Extra copies" doesn't detect APFS clones or hard links.
  - Files under 1 MB are only ever probable.
  - "Identical" means identical when checked.
- **No confirmation when quitting mid-copy.** Quitting is safe, because recovery handles it.
- **The catalogue never shrinks.** Space freed by rescans stays in the file.
- **Shared catalogue.** Development and release builds use the same catalogue.
- **Network drives aren't supported.**

## 7. Accessibility status

From Phase 5, without changing the design:

- **VoiceOver reaches every control.** Browse rows were rebuilt so their buttons aren't hidden.
- **Focus** is kept or returned whenever a control replaces itself or closes.
- **Escape** closes panels and the confirmation.
- **Announcements** cover only starts, results and cancels.
- **Repeated buttons** say which file or drive they act on.
- **Contrast** meets AA in light and dark appearances.
- **Status** is never shown by colour alone.

The Duplicates view follows the same rules: groups are buttons with `aria-expanded`, and the check announces its start and result, then returns focus.

- **Still needed:** the VoiceOver checklist, run by hand in the real app.
- **Deferred:** table semantics for the Browse file list, and announcing view changes.

## 8. Database and migration status

- **Schema version 2,** stored in `PRAGMA user_version`. Version 2 added `content_checks` and a size index.
- **Upgrades** run in one `BEGIN IMMEDIATE` transaction, after a `VACUUM INTO` backup that never replaces an existing file.
- **Failure or interruption keeps nothing.** A second app instance waits, then finds the catalogue current.
- **Newer catalogues** are refused and left unchanged.
- **Historical schemas:** fixtures for all nine upgrade with every field kept. A copy of the developer's real catalogue upgraded cleanly to version 1.
- **The transfers table has never changed.**
- **Rules for future schema changes** are in DATABASE.md.

## 9. Release-build status

- **The build works.** `npm run build:release` produces a working universal `Media Mapper.app` (13 MB) and a DMG (6.2 MB). The arm64-only build was 6.4 MB and 3.1 MB.
- **The release CSP, dialogs and macOS tools all work** in the bundled app.
- **The bundle is unsigned:** only the linker signs it, ad hoc, and `codesign --verify` fails.
- **Settled:** a universal build (`npm run build:release`), version 1.0.0 build 1, minimum macOS 27.0, category Utility, "© 2026 Kind Enough Studio", and the app icon.

## 10. Signing and notarisation status

**Not started, and blocked.** This Mac has Apple Development and Apple Distribution (App Store) certificates, but no **Developer ID Application** certificate, which signing outside the App Store needs. Only the team's (`SWVSGRAP86`) Account Holder can create one.

What's already in place:

- **Hardened runtime:** works without entitlements, so notarisation needs nothing extra.
- **Tools:** `notarytool` and `stapler` are installed.
- **Tauri** can sign, notarise and staple during the build, given the signing identity and an App Store Connect API key.
- **The DMG** needs notarising and stapling too.

The steps are in SHIPPING.md.

## 11. Remaining blockers

Every open issue, in the three classes. "You" marks a decision or action only you can take.

### Blocker

| Issue | Source |
|---|---|
| Get a Developer ID Application certificate, then sign, notarise and staple the app and DMG (you, then a build) | SHIPPING S2, P3-2 |

### Should fix before V1

| Issue | Source |
|---|---|
| Run the physical-drive cases M1–M11 | SAFETY-MATRIX F5 |
| Open the app from Finder and check the privacy prompts | RELEASE-TESTING section 5 |
| Run the VoiceOver checklist in the app | ACCESSIBILITY.md |
| Open a file from Browse in the release build (R10) | RELEASE-TESTING |

### Safe to defer

| Issue | Source |
|---|---|
| Automatic updates (D7). Recommended for 1.1, with `tauri-plugin-updater`. | SHIPPING S12 |
| Narrow `dialog:default` to `dialog:allow-open` | SHIPPING S13 |
| One launch without a window, seen once under an ad hoc hardened runtime and not reproduced; recheck on the first signed build | SHIPPING S14 |
| Plan readiness isn't rechecked when free space changes on a connected drive. Execution always rechecks. | P3-7 |
| No confirmation on quit during a copy | SAFETY-MATRIX F1 |
| A folder on this Mac deleted at the last moment could be recreated by the copy | SAFETY-MATRIX F2 |
| The catalogue never gives back free space | DATABASE.md |
| Development and release share one catalogue | P3-9 |
| Crash reports have no symbols; a panic ends the app, which recovery handles | P3-11 |
| Errors show in one banner that can be off-screen; a busy catalogue looks like an error | ERROR-MESSAGES.md |
| Log lines use Unix times | ERROR-MESSAGES.md |
| Browse file list table semantics; announcing view changes | ACCESSIBILITY.md |
| Remove the unreachable legacy dashboard code (`showLegacyDashboard`) | ACCESSIBILITY.md |
| Record SHA-256 during copy verification, so Media Mapper's own copies show as identical without being read again | DUPLICATES-DESIGN step 5 |

## 12. Recommended V1 scope

Ship what exists today, with nothing added:

- **Drives:** discovery, labels, scanning, automatic rescans of catalogued drives when they connect, and a persistent catalogue that stays browsable offline.
- **Browse:** browsing and search across drives, opening files, and Show in Finder.
- **Plan:** virtual planned moves to drives and to folders on this Mac, with previews, destination capacity and live readiness.
- **Copy:** copy and verify with cancellation, interruption recovery and destination-loss detection.
- **Transfers:** truthful history with Show in Finder.
- **Duplicates:** informational only. Probable, identical once checked, with an estimate before reading, progress and Cancel.

Then close the blockers and the "should fix" items in section 11. None of them needs new code; P3-6 was the last that did, and is fixed.

## 13. Explicitly deferred features

These are out of V1 by decision, not by oversight:

- Deleting files, or any automatic or one-click duplicate cleanup
- Copying whole folders (they can be planned and previewed)
- Automatic updates
- AI organisation, cloud sync, NAS or S3, Plex-style metadata, collaboration and mobile
- Network drives
- Mac App Store distribution and the App Sandbox
- Hashing during copy verification (duplicates step 5)
- Onboarding screens: the empty states guide first use instead
