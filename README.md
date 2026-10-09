# Tidy Drives

A macOS desktop app for cataloguing external drives and planning where files should live.

- **Catalogue**: scan a connected drive once. Tidy Drives stores file names, paths, sizes and dates, then lets you browse and search the catalogue after the drive is disconnected. A catalogued drive is rescanned automatically whenever it connects, so its catalogue keeps up. Scanning never opens, renames, moves, copies or deletes files.
- **Plan**: choose where files and folders should end up, on another drive or in a folder on this Mac. Plans are virtual; nothing moves.
- **Find**: search every drive at once, and list the largest files and probable duplicates.

Interface conventions are in [UI-GUIDELINES.md](UI-GUIDELINES.md).

## Development

Requires Node.js, Rust and the [Tauri prerequisites](https://tauri.app/start/prerequisites/). Drive discovery uses `diskutil`, so the app runs on macOS.

```bash
npm install
npm run tauri dev
```

Checks:

```bash
npm run build                          # TypeScript check and frontend build
cd src-tauri && cargo test --lib       # Rust tests
```

CI runs these, plus `cargo fmt --check`, on every pull request and push to `main` ([workflow](.github/workflows/ci.yml)).

Production builds (`npm run build:release`, universal) are checked with the smoke test in [RELEASE-TESTING.md](RELEASE-TESTING.md). Release status and what remains before V1 are in [RELEASE-READINESS.md](RELEASE-READINESS.md); distribution steps are in [SHIPPING.md](SHIPPING.md). Safety and recovery coverage is mapped in [SAFETY-MATRIX.md](SAFETY-MATRIX.md). Keyboard and VoiceOver status is in [ACCESSIBILITY.md](ACCESSIBILITY.md). Error wording is catalogued in [ERROR-MESSAGES.md](ERROR-MESSAGES.md), and diagnostics are logged to `~/Library/Logs/com.ryanborroff.tidydrives/tidy-drives.log`.

The catalogue is a SQLite database at `~/Library/Application Support/com.ryanborroff.tidydrives/catalogue.sqlite3`. How its schema is versioned and upgraded is in [DATABASE.md](DATABASE.md).
