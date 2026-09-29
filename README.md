# Media Mapper

A macOS desktop app for cataloguing external drives and planning where files should live.

- **Catalogue**: scan a connected drive once. Media Mapper stores file names, paths, sizes and dates, then lets you browse and search the catalogue after the drive is disconnected. Scanning never opens, renames, moves, copies or deletes files.
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

The catalogue is a SQLite database at `~/Library/Application Support/com.mediamapper.app/catalogue.sqlite3`.
