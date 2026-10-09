# Shipping Tidy Drives

This audit covers what it takes to distribute Tidy Drives as a proper macOS app outside the Mac App Store: its identity, signing and notarisation, permissions, updates and first run. It was done on 2026-10-07 on `release-hardening`, using macOS 27.0.1, Xcode with `notarytool` 1.1.3, tauri-cli 2.12.0 and Rust 1.98.1.

The release build itself works; see [RELEASE-TESTING.md](RELEASE-TESTING.md). What stops a public release is identity and signing, not code.

## Decisions needed

| # | Decision | Recommendation | Why it matters |
|---|---|---|---|
| D1 | **Bundle identifier** | *Decided: `com.ryanborroff.tidydrives`* (renamed with the product, before any public build). | It can never change after the first public build. The catalogue's folder is named after it. |
| D2 | **App icon** | *Decided: three labelled external drives, stacked* (`design/app-icon.svg`). Every size is made with `npx tauri icon design/app-icon.png`. | It's the first thing anyone sees of the app |
| D3 | **Apple silicon only, or universal** | *Decided: universal.* `npm run build:release` builds it. | An arm64-only build can't run on Intel Macs |
| D4 | **Minimum macOS** | *Decided: macOS 27.0,* the version that can be tested. Lower it only after testing on an older version. | Anyone on an older macOS can't open it |
| D5 | **Product name in the window** | *Decided: "Tidy Drives"* everywhere, including the sidebar | |
| D6 | **Version and build number** | *Decided:* V1 is `1.0.0`, build `1`. Raise `bundle.macOS.bundleVersion` by one for every build anyone else receives. | |
| D7 | **Updates** | No automatic updater in V1. See [Updates](#updates). | An updater is a security-critical channel and needs its own keys and hosting |

## Application identity

| Item | Now | Needed |
|---|---|---|
| Product name | `Tidy Drives` everywhere | Done (D5) |
| Bundle identifier | `com.ryanborroff.tidydrives` (earlier `com.ryanborroff.mediamapper`, then `com.mediamapper.app`) | Done, with existing catalogues brought across (below) |
| Version | `1.0.0` in `tauri.conf.json`, `package.json` and `Cargo.toml` | Done |
| Build number | `bundle.macOS.bundleVersion` = `1` | Done (D6); raise it for every build |
| Icon | Tidy Drives' own, from `design/app-icon.svg` | Done (D2) |
| Metadata | `bundle.category` is `Utility`; `bundle.copyright` is "© 2026 Kind Enough Studio" | Done |
| Minimum macOS | `bundle.macOS.minimumSystemVersion` = `27.0` | Done (D4) |

### Changing the identifier without losing catalogues

The catalogue, its pre-upgrade backups and the log all live in folders named after the identifier. Builds from when the app was called Media Mapper used `com.ryanborroff.mediamapper`, and before that `com.mediamapper.app`; the identifier is now `com.ryanborroff.tidydrives`.

The first time the app needs its catalogue and the new folder holds none, it brings the newest old one across (`bring_legacy_catalogues_across`, details in [DATABASE.md](DATABASE.md)):

1. it copies the old catalogue with `VACUUM INTO`, as the schema backups already are;
2. it runs `PRAGMA integrity_check` on the copy;
3. only then does it link the copy into place, never replacing an existing catalogue;
4. it leaves the old folder untouched.

The old log isn't moved; a new one starts in `~/Library/Logs/com.ryanborroff.tidydrives/`. The identifier is permanent from the first build anyone else installs.

## Distribution

### Release build

`npm run tauri build` produces `Tidy Drives.app` and `Tidy Drives_<version>_aarch64.dmg`. The DMG holds the app and an Applications link. Its checksum is valid.

### Code signing

**Blocker:** this Mac has no certificate that can sign for distribution outside the App Store.

`security find-identity -v -p codesigning` lists only these:

- `Apple Development: Ryan Borroff`, for development builds
- `Apple Distribution: Ryan Borroff (SWVSGRAP86)`, for the App Store

Direct distribution needs a **Developer ID Application** certificate from team `SWVSGRAP86`. Only the team's Account Holder can create one: in Xcode › Settings › Accounts › Manage Certificates, choose **+ › Developer ID Application**, or create it at developer.apple.com.

Today the bundle is signed only by the linker, ad hoc, and `codesign --verify --deep --strict` fails. It runs on the Mac that built it. On any other Mac, Gatekeeper reports it as damaged.

### Hardened runtime and entitlements

Notarisation requires the hardened runtime, and Tauri enables it by default when it signs. To check that Tidy Drives works under it, a copy of the release app was signed ad hoc with `--options runtime` and **no entitlements**, then run with a separate home folder and a test disk image. These all worked:

- the window
- drive detection, which runs `diskutil`
- scanning
- the database
- browsing

Five fresh launches in a row opened normally.

One earlier launch, the very first of that newly signed copy, started without showing its window. It couldn't be reproduced. Check for it on the first Developer ID build.

- **Entitlements:** none needed. Tidy Drives doesn't use the App Sandbox, JIT, unsigned libraries or Apple Events. WebKit runs its JavaScript engine in its own Apple-signed processes.
- **App Sandbox:** not recommended for V1. It would need security-scoped bookmarks for every drive and folder, and it restricts running `diskutil`, `df` and `open`. It is only required for the Mac App Store, which is out of scope.

### Notarisation and stapling

`xcrun notarytool` and `stapler` are installed. Tauri signs, notarises and staples the app during `tauri build` when these are set:

- `APPLE_SIGNING_IDENTITY="Developer ID Application: Ryan Borroff (SWVSGRAP86)"`
- **Either** an App Store Connect API key: `APPLE_API_ISSUER`, `APPLE_API_KEY` and `APPLE_API_KEY_PATH`
- **Or** an Apple ID: `APPLE_ID`, an app-specific `APPLE_PASSWORD` and `APPLE_TEAM_ID`

Prefer the API key, and keep it out of the repository.

Then notarise and staple the DMG as well, so it opens without a network check:

```bash
xcrun notarytool submit "Tidy Drives_1.0.0_universal.dmg" --keychain-profile <profile> --wait
xcrun stapler staple "Tidy Drives_1.0.0_universal.dmg"
spctl -a -vv -t open --context context:primary-signature "Tidy Drives_1.0.0_universal.dmg"
```

**Gatekeeper once signed and notarised:** downloaded from the web, the app opens after the standard "downloaded from the internet" confirmation. It is never reported as damaged or unverified.

### Distribution format

A DMG is the right format: a signed, notarised and stapled DMG holding the app and an Applications link. A ZIP works too, but a DMG is the familiar install for a Mac utility.

The release build is universal. Once per Mac, add the Intel target, then build:

```bash
rustup target add x86_64-apple-darwin
npm run build:release
```

It produces `src-tauri/target/universal-apple-darwin/release/bundle/macos/Tidy Drives.app` and `.../bundle/dmg/Tidy Drives_<version>_universal.dmg`.

## Permissions

| Access | When | Status |
|---|---|---|
| Removable volumes (TCC) | Scanning a drive, and copying from or to one | **Usage text added** (`761706c`) |
| Documents, Desktop, Downloads (TCC) | Copying into a folder chosen there | **Usage text added** (`761706c`) |
| Network volumes | Excluded from drive detection | No string needed. A network folder picked as a destination hasn't been tested. |
| Full Disk Access | Not needed | — |
| Tauri capabilities | `core:default`, `core:window:allow-show`, `dialog:default` | Fine. `dialog:default` also allows message and save dialogs, which aren't used; it could be narrowed to `dialog:allow-open`. Safe to defer. |
| CSP | Release CSP verified in Phase 3 | Fine |

The prompts themselves can only be seen when the app is opened from Finder, never when it is started from Terminal. That is still a manual step: RELEASE-TESTING.md section 5.

Ad hoc builds get a new code identity with every build, so macOS asks again after each rebuild. Developer ID builds keep their permissions across updates.

## Updates

| Option | What it takes | Fit |
|---|---|---|
| **No updater** (recommended for V1) | Publish signed DMGs on GitHub Releases or a website. The About window or website says what's new. | Nothing new to secure. Users update by downloading. |
| `tauri-plugin-updater` | A separate updater signing key, a `latest.json` and signed bundles on GitHub Releases or another HTTPS host, and update checks in the app | The natural choice for 1.1, once releases are routine |
| Sparkle | Native Objective-C framework | Not integrated with Tauri. More work than the plugin. |

Rules any updater must follow, whenever one is added:

- **Never during a copy or content check.** Never install while a copy or content check runs, and never restart the app under one. Recovery would cope, but it should never have to.
- **No downgrades.** Releases must only move forward. A catalogue upgraded by a newer version is refused by older ones (DATABASE.md).
- **Keep the updater key safe.** Keep it separate from the Developer ID certificate. Losing it means users can't update in place.

## First run

The flow is Drives → connect a drive → **Scan** → **Browse**, and it is explained where it happens:

- **Drives** with nothing connected: "No external drives detected. Connect a drive and it will appear here."
- **A connected drive** that isn't scanned: "Not catalogued yet · Never scanned", with **Scan**.
- **Browse** before any scan: "No catalogued drives. Scan a drive first, then its files will appear here."
- **Plan** when empty: "Browse your catalogued files and choose Add to plan to add files here." This named a "Plan move" button that doesn't exist; fixed in `ab858ee`.
- **Transfers** when empty: "No transfers yet. Copied and verified files will appear here."

**Recommendation:** no onboarding screens. The empty states already lead to the next step. One optional addition would be a sentence on the empty Drives view saying that scanning only reads names, sizes and dates. Safe to defer.

## Release checklist

1. **Identity:** the identifier, version, build number, minimum macOS, copyright, category and icon are already set. Raise the build number for every build anyone else receives.
2. **Certificate:** create the Developer ID Application certificate and an App Store Connect API key.
3. **Build:**
   ```bash
   npm run build:release
   ```
   Run it with the signing and notarisation variables set.
4. **DMG:** notarise and staple it, then check it with `spctl`.
5. **Smoke test:** follow RELEASE-TESTING.md, including section 5 opened from Finder. Repeat it on a second Mac, or a clean user account, after downloading the DMG from the web.
6. **Publish:** tag the release, publish the DMG and write the release notes.

## Findings

| # | Finding | Class |
|---|---|---|
| S1 | The bundle identifier ends in `.app` and must be chosen before any public build (D1) | Fixed: `com.ryanborroff.tidydrives` |
| S2 | No Developer ID Application certificate, so no signing or notarisation is possible yet | Blocker |
| S3 | The icon is Tauri's default (D2) | Fixed |
| S4 | arm64-only build (D3) | Fixed: universal |
| S5 | Minimum macOS declared as an untested 10.13 (D4) | Fixed: 27.0 |
| S6 | Build number repeats the version (D6) | Fixed: 1.0.0, build 1 |
| S7 | The product name was spelled two ways, "MediaMapper" and "Media Mapper" (D5) | Fixed: now "Tidy Drives" everywhere |
| S8 | No copyright or category metadata | Fixed |
| S9 | Privacy prompts not yet seen from a Finder launch (RELEASE-TESTING section 5) | Should fix before V1 |
| S10 | Privacy usage descriptions | Fixed (`761706c`) |
| S11 | Plan's empty state named a button that doesn't exist | Fixed (`ab858ee`) |
| S12 | Automatic updates (D7) | Safe to defer |
| S13 | `dialog:default` is broader than needed | Safe to defer |
| S14 | One launch without a window, seen once under an ad hoc hardened runtime and not reproduced | Safe to defer; recheck on the first signed build |
