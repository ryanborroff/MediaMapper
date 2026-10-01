# Release testing

A repeatable smoke test for a production build of Media Mapper, run outside `tauri dev`. It uses disk images as stand-in drives and a separate home folder. That way it never touches your real catalogue or your real drives.

Run it before every release candidate, and after any change to Tauri configuration, capabilities, the copy engine or the database.

## 1. Build

```bash
npm ci
npm run tauri build
```

This runs `npm run build` (type check and Vite) first, then builds the Rust app in release mode, which takes about 3–4 minutes from cold.

What it produces:

- `src-tauri/target/release/bundle/macos/Media Mapper.app`
- `src-tauri/target/release/bundle/dmg/Media Mapper_<version>_<arch>.dmg`

Expected output: one warning, that the bundle identifier `com.mediamapper.app` ends with `.app`. See [HARDENING.md](HARDENING.md) gap 12. Nothing else should warn or fail.

## 2. Inspect the bundle

```bash
APP="src-tauri/target/release/bundle/macos/Media Mapper.app"
DMG=$(ls src-tauri/target/release/bundle/dmg/*.dmg)
plutil -p "$APP/Contents/Info.plist"
file "$APP/Contents/MacOS/media-mapper"
codesign -dv --verbose=2 "$APP"
codesign --verify --deep --strict "$APP"; echo "verify exit $?"
hdiutil verify "$DMG"
```

| Check | Expected now (unsigned build) | Expected once Phase 8 is done |
|---|---|---|
| `CFBundleIdentifier` | `com.mediamapper.app` | A decided, permanent identifier |
| `CFBundleShortVersionString` / `CFBundleVersion` | Both `0.1.0` | The version, plus a separate, increasing build number |
| `LSMinimumSystemVersion` | `10.13`, which is lower than Tauri 2 supports | `10.15` or later |
| Architecture | `arm64` only | Decided: arm64 only, or universal |
| Signature | `adhoc,linker-signed`, so `codesign --verify` **fails** ("code has no resources but signature indicates they must be present") | Developer ID, hardened runtime, notarised and stapled |
| DMG | Checksum VALID; holds `Media Mapper.app` and an `Applications` link | Same, signed |
| Usage descriptions (`NS…UsageDescription`) | None | See the [permissions](#5-permissions-and-first-launch-from-finder) section |

The unsigned build runs on the Mac that built it. Copied to another Mac it is quarantined, and Gatekeeper reports it as damaged.

## 3. Set up stand-in drives and an isolated home

Disk images mount under `/Volumes`. `diskutil` reports them as `Device Location: External`, so Media Mapper treats them as external drives. Unmounting one with `diskutil eject force` behaves like unplugging a drive mid-copy.

```bash
T=$(mktemp -d /tmp/mm-release-XXXX)
mkdir -p "$T/home" "$T/local-dest"
hdiutil create -size 400m  -fs APFS -volname MMSmokeSource "$T/source.dmg"
hdiutil create -size 400m  -fs APFS -volname MMSmokeBackup "$T/backup.dmg"
hdiutil create -size 2600m -fs APFS -volname MMSmokeLarge  "$T/large.dmg"
for image in source backup large; do diskutil image attach --mountOptions nobrowse "$T/$image.dmg"; done
mkdir -p "/Volumes/MMSmokeSource/Films/Été 2024"
printf 'smoke test clip\n' > "/Volumes/MMSmokeSource/Films/Été 2024/Café clip.mov"
dd if=/dev/urandom of=/Volumes/MMSmokeSource/Films/big.bin bs=1m count=300
dd if=/dev/urandom of=/Volumes/MMSmokeLarge/huge.bin bs=1m count=2048
shasum -a 256 /Volumes/MMSmokeSource/Films/big.bin /Volumes/MMSmokeLarge/huge.bin > "$T/sources.sha"
```

Launch the release app with the separate home folder. The catalogue then lives at `$T/home/Library/Application Support/com.mediamapper.app/catalogue.sqlite3`, and your real one is never opened:

```bash
HOME="$T/home" "$APP/Contents/MacOS/media-mapper"
```

Started this way, the app inherits Terminal's privacy permissions, so macOS asks for nothing. Section 5 covers first launch from Finder.

To read the test catalogue:

```bash
sqlite3 "$T/home/Library/Application Support/com.mediamapper.app/catalogue.sqlite3" "SELECT id, status, copied_bytes, error_message FROM transfers"
```

Use plain `sqlite3`, not `-readonly`. The database is in WAL mode, and a read-only open fails when the app has no connection open.

To catch a partial file the moment it appears, enable `setopt NULL_GLOB` first, then use `f=(/Volumes/MMSmokeBackup/.mediamapper-transfer-*)`. The interruption steps below rely on this.

## 4. Smoke test

Work through the table in order. "History" means the **Transfers** view.

| # | Area | Steps | Expected |
|---|---|---|---|
| R1 | Launch | Launch as above. | The window opens on **Drives**, styled correctly in light and dark mode. The catalogue file is created under `$T/home`. |
| R2 | Drive detection | **Refresh**. | The three MMSmoke images are listed under Connected with free space. |
| R3 | Scanning | **Scan** each image. | Source shows "2 files · 2 folders · 314.6 MB catalogued". The others show their contents. |
| R4 | Browsing | **Browse** › MMSmokeSource › Films › Été 2024. | `Café clip.mov` is listed with the exact accented name. |
| R5 | Planning | **Add to plan** on `Café clip.mov`, location MMSmokeBackup, **Add to plan**. | Plan shows 1 item, "Ready to transfer". |
| R6 | Preview organisation | Plan › **Preview organisation** (shown for drive destinations). | MMSmokeBackup shows the planned file as "Not on the drive yet". Nothing has been written to the drive. |
| R7 | Copy and verify | **Continue** › **Copy and verify**. | "1 file copied and verified. Originals remain untouched." `cmp` of source and copy succeeds, and the copy keeps the source's modification date. |
| R8 | History | **View transfers**. | Café clip.mov, "Copied and verified", From and To paths. |
| R9 | Show in Finder | **Show in Finder** on that entry. | Finder selects `MMSmokeBackup/Café clip.mov`. |
| R10 | Open file | Browse to a catalogued file you can open, such as a `.txt` you add and rescan, and click its name. | It opens in its default app. A file type no app can open gives "No app on this Mac can open this type of file." |
| R11 | Folder on this Mac | Add to plan › Location › **Choose a folder on this Mac…**, pick `$T/local-dest`, then copy. | The file is copied and verified into that folder. |
| R12 | Cancel | Plan `huge.bin` to `$T/local-dest`, **Copy and verify**, then **Cancel** at once. | "Transfer cancelled. No files were copied. Planned transfers are unchanged." History shows Cancelled. No partial file. `shasum -c "$T/sources.sha"` passes. |
| R13 | Force-kill mid-copy | Plan `big.bin` to MMSmokeBackup and copy. As soon as `.mediamapper-transfer-*.partial` appears on MMSmokeBackup, run `pkill -9 -x media-mapper`. Relaunch. | History shows Failed, "Interrupted before verification completed." The partial is gone. The plan still has 1 item. |
| R14 | Quit mid-copy | Copy again and press ⌘Q as soon as the partial appears. Relaunch. | As R13. Never "Copied and verified". |
| R15 | Retry | **Copy and verify** again. | Completes. `cmp` passes. The plan is empty. |
| R16 | Destination unplugged | Re-plan a large file to MMSmokeBackup after freeing space, copy, then run `diskutil eject force /Volumes/MMSmokeBackup` once the partial appears. | "Transfer stopped. Nothing was copied. The destination … is no longer available." Failed in history. The plan is kept. Reattach and retry: the leftover partial is removed and the copy completes. |
| R17 | Source unplugged | As R16, but eject `/Volumes/MMSmokeSource`. | The transfer stops and is recorded as failed. No partial on the destination. The plan is kept. After reattaching, `shasum -c` passes. |
| R18 | Capacity | With MMSmokeBackup nearly full, plan a file larger than its free space. | Plan shows "Needs attention: … does not currently have enough free space". **Continue** is disabled. |
| R19 | Persistence | Quit and relaunch. | Drives, scan times, labels, history and the remaining plan are all still there. |
| R20 | Offline browsing | Eject all images and relaunch. | All three drives are listed under Offline. Browsing MMSmokeSource › Films still lists its contents. |
| R21 | Search | Browse › **All files**, search `clip`. | `Café clip.mov` on MMSmokeSource, marked "Drive offline". |

Teardown:

```bash
for v in MMSmokeSource MMSmokeBackup MMSmokeLarge; do diskutil eject "/Volumes/$v"; done
rm -rf "$T"
```

## 5. Permissions and first launch from Finder

These can't be automated. They need a person at the Mac, because macOS shows privacy prompts only to an app launched on its own, from Finder or the Dock. An app started from Terminal uses Terminal's permissions instead.

Before you start, quit Media Mapper and back up the real catalogue. A Finder launch can't be pointed at another home folder:

```bash
cp ~/Library/Application\ Support/com.mediamapper.app/catalogue.sqlite3 ~/Desktop/catalogue-backup.sqlite3
```

1. Copy `Media Mapper.app` to `/Applications` and open it from Finder.
2. Scan a real external drive. **Expected:** macOS asks whether Media Mapper may access files on a removable volume. Allow it. The scan completes. Decline instead and the scan should report it couldn't read the drive, not crash.
3. Add a folder inside **Documents**, **Desktop** or **Downloads** as a destination, and copy to it. Note any prompt. The release has no usage-description text, so prompts show only the system wording.
4. Quit and relaunch, then copy to that folder again. Note whether macOS asks again.
5. Record the results in the table below.

Ad-hoc builds get a new code identity on every build, so macOS may ask again after each rebuild. Signed builds keep their permissions.

## Results

### 2026-10-01: `release-hardening` at `270391a`, macOS 27.0.1, Apple silicon

Run with disk images and a separate home folder, driving the bundled app through the macOS accessibility interface.

| # | Result | Notes |
|---|---|---|
| Build | Pass | 3 m 27 s. App 6.3 MB, DMG 3.0 MB. Only the identifier warning. |
| Bundle | As listed in section 2 | `codesign --verify` fails. arm64 only. Minimum system 10.13. No usage descriptions. |
| R1–R3 | Pass | The production frontend renders under the release CSP. Disk images are detected as external drives. |
| R4–R5 | Pass | The planning panel works by keyboard. VoiceOver can't reach a row's **Add to plan** and **Open** buttons (Phase 5). |
| R6 | Pass | `big.bin` shown as planned on MMSmokeBackup, "Not on the drive yet". Nothing was written. Not offered for a folder on this Mac, by design. |
| R7–R9 | Pass | The copy is byte-identical and its date is kept. Finder selected the copied file. |
| R10 | Not run | Avoided opening apps on the tester's screen. Run by hand. |
| R11 | Pass | The native folder picker worked. The file was copied into the folder on this Mac. |
| R12 | Pass | Cancel via the new run id. Recorded as "Copy cancelled.", no partial, source hash unchanged. |
| R13 | Pass | Killed in `copying` with a 3 MB partial. Relaunch recorded Failed and removed the partial. The plan was kept. |
| R14 | Pass | ⌘Q landed during **verification**: the app quit at once with the record in `verifying`. Relaunch recorded Failed and removed the 300 MB partial. Never completed. |
| R15 | Pass | |
| R16 | Pass | The leftover 74 MB partial was removed on retry, and the retry completed. The message calls the drive a "folder" (Phase 6). |
| R17 | Pass, with an error-copy issue | The window showed "Nothing was copied. Unable to copy file: Input/output error (os error 5)" (Phase 6). |
| R18 | Pass, with an issue | Blocked correctly, but the same screen said "Space after transfer 103.1 MB". See finding P3-6. |
| R19–R21 | Pass | The window said "1 files" and "1 results" (Phase 6). |
| Section 5 | Not run | Needs a person at the Mac. |

## Phase 3 findings

| # | Finding | Class |
|---|---|---|
| P3-1 | The bundle identifier ends in `.app`, and the catalogue folder is named after it. This is gap 12 in [HARDENING.md](HARDENING.md). | Blocker |
| P3-2 | The bundle signature is invalid (linker ad-hoc only). Fine on the building Mac. On any other Mac it's reported as damaged. To fix in Phase 8 with Developer ID signing and notarisation. | Blocker for distribution (Phase 8) |
| P3-3 | arm64-only build: Intel Macs can't run it. Universal or arm64-only is a decision for Phase 8. | Should fix before V1 (decision) |
| P3-4 | No `NSRemovableVolumesUsageDescription` or Documents, Desktop and Downloads usage strings. Development never shows these prompts, so they are untested: see section 5. | Should fix before V1 (Phase 8) |
| P3-5 | Diagnostics go to stderr (`eprintln!`), which is lost when the app is launched from Finder. Recovery and database errors leave no trace on a user's Mac. | Fixed in Phase 6: `~/Library/Logs/com.mediamapper.app/media-mapper.log` |
| P3-6 | Plan's "Space after transfer" uses the free space stored at the last scan, while readiness uses current free space. The two can contradict each other on one screen, and the planned-data figure is the truthful one to show. | Should fix before V1 |
| P3-7 | Plan readiness isn't rechecked when free space changes on a drive that stays connected. It refreshes on connect, disconnect and user actions. Execution always rechecks, so this is never unsafe. | Safe to defer |
| P3-8 | `LSMinimumSystemVersion` is 10.13, lower than Tauri 2 supports (10.15). | Should fix before V1 (Phase 8) |
| P3-9 | Development and release builds share one catalogue (same identifier and path). A development branch with schema changes migrates the developer's real catalogue. This section's separate home folder avoids that for testing. | Safe to defer |
| P3-10 | A destination drive's catalogue doesn't show copied files until it is rescanned. | Known limitation |
| P3-11 | `panic = "abort"` and `strip = true` in release. A panic ends the app at once, which recovery handles like a force-kill, and crash reports have no symbols. Revisit with crash reporting in Phase 8. | Safe to defer |
