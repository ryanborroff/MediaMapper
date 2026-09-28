import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

type DriveInfo = {
  name: string;
  mountPoint: string;
  filesystem: string | null;
  totalBytes: number | null;
  availableBytes: number | null;
  persistentIdentifier: string | null;
  deviceIdentifier: string | null;
};

type CataloguedDrive = {
  persistentIdentifier: string;
  name: string;
  filesystem: string | null;
  totalBytes: number | null;
  availableBytes: number | null;
  lastMountPoint: string | null;
  lastScannedAt: number | null;
  fileCount: number;
  directoryCount: number;
  cataloguedBytes: number;
};

type ScanProgress = {
  persistentIdentifier: string;
  fileCount: number;
  directoryCount: number;
  cataloguedBytes: number;
  skippedCount: number;
  currentPath: string;
};

type CatalogueEntry = {
  relativePath: string;
  name: string;
  isDirectory: boolean;
  sizeBytes: number | null;
  modifiedAt: number | null;
};

type LibrarySearchResult = CatalogueEntry & {
  driveId: string;
  driveName: string;
};

function formatBytes(bytes: number | null) {
  if (bytes === null) return "—";
  if (bytes === 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB", "PB"];
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1000)), units.length - 1);
  return `${(bytes / 1000 ** exponent).toFixed(exponent >= 4 ? 2 : 1)} ${units[exponent]}`;
}

function formatDate(timestamp: number | null) {
  if (!timestamp) return "Never";
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" })
    .format(new Date(timestamp * 1000));
}

function App() {
  const [connected, setConnected] = useState<DriveInfo[]>([]);
  const [catalogued, setCatalogued] = useState<CataloguedDrive[]>([]);
  const [loading, setLoading] = useState(true);
  const [scanningId, setScanningId] = useState<string | null>(null);
  const [scanProgress, setScanProgress] = useState<ScanProgress | null>(null);
  const [scanComplete, setScanComplete] = useState<ScanProgress | null>(null);
  const [cancellingId, setCancellingId] = useState<string | null>(null);
  const [scanCancelledId, setScanCancelledId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [browserDrive, setBrowserDrive] = useState<CataloguedDrive | null>(null);
  const [browserPath, setBrowserPath] = useState("");
  const [entries, setEntries] = useState<CatalogueEntry[]>([]);
  const [browserLoading, setBrowserLoading] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  const [searchResults, setSearchResults] = useState<CatalogueEntry[]>([]);
  const [searching, setSearching] = useState(false);
  const [libraryQuery, setLibraryQuery] = useState("");
  const [libraryResults, setLibraryResults] = useState<LibrarySearchResult[]>([]);
  const [librarySearching, setLibrarySearching] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [drives, catalogue] = await Promise.all([
        invoke<DriveInfo[]>("list_external_drives"),
        invoke<CataloguedDrive[]>("list_catalogued_drives"),
      ]);
      setConnected(drives);
      setCatalogued(catalogue);
    } catch (cause) {
      setError(String(cause));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { void refresh(); }, [refresh]);

  useEffect(() => {
    let dispose: (() => void) | undefined;
    void listen<ScanProgress>("scan-progress", (event) => setScanProgress(event.payload))
      .then((unlisten) => { dispose = unlisten; });
    return () => { dispose?.(); };
  }, []);

  const connectedIds = useMemo(
    () => new Set(connected.flatMap((drive) => drive.persistentIdentifier ? [drive.persistentIdentifier] : [])),
    [connected],
  );

  const openFolder = useCallback(async (drive: CataloguedDrive, path: string) => {
    setBrowserDrive(drive);
    setBrowserPath(path);
    setBrowserLoading(true);
    setError(null);
    try {
      setEntries(await invoke<CatalogueEntry[]>("list_catalogue_entries", {
        persistentIdentifier: drive.persistentIdentifier,
        parentPath: path,
      }));
    } catch (cause) {
      setError(String(cause));
      setEntries([]);
    } finally {
      setBrowserLoading(false);
    }
  }, []);

  const searchCatalogue = useCallback(async (drive: CataloguedDrive, query: string) => {
    setSearchQuery(query);
    const trimmed = query.trim();
    if (!trimmed) { setSearchResults([]); return; }
    setSearching(true); setError(null);
    try {
      setSearchResults(await invoke<CatalogueEntry[]>("search_catalogue", {
        persistentIdentifier: drive.persistentIdentifier, query: trimmed,
      }));
    } catch (cause) { setError(String(cause)); setSearchResults([]); }
    finally { setSearching(false); }
  }, []);

  const searchLibrary = useCallback(async (query: string) => {
    setLibraryQuery(query);
    const trimmed = query.trim();
    if (!trimmed) {
      setLibraryResults([]);
      return;
    }

    setLibrarySearching(true);
    setError(null);
    try {
      setLibraryResults(await invoke<LibrarySearchResult[]>("search_all_catalogues", { query: trimmed }));
    } catch (cause) {
      setError(String(cause));
      setLibraryResults([]);
    } finally {
      setLibrarySearching(false);
    }
  }, []);

  const containingFolder = (relativePath: string) => {
    const separator = relativePath.lastIndexOf("/");
    return separator === -1 ? "" : relativePath.slice(0, separator);
  };

  const openSearchResult = async (drive: CataloguedDrive, entry: CatalogueEntry) => {
    const destination = entry.isDirectory ? entry.relativePath : containingFolder(entry.relativePath);
    setSearchQuery(""); setSearchResults([]);
    await openFolder(drive, destination);
  };

  const openLibraryResult = async (result: LibrarySearchResult) => {
    const drive = catalogued.find((item) => item.persistentIdentifier === result.driveId);
    if (!drive) {
      setError("That drive catalogue is no longer available.");
      return;
    }
    setLibraryQuery("");
    setLibraryResults([]);
    await openSearchResult(drive, result);
  };

  const cancelScan = async (persistentIdentifier: string) => {
    setCancellingId(persistentIdentifier);
    try {
      await invoke("cancel_scan", { persistentIdentifier });
    } catch (cause) {
      setCancellingId(null);
      setError(String(cause));
    }
  };

  const scan = async (drive: DriveInfo) => {
    if (!drive.persistentIdentifier) {
      setError("This drive does not provide a stable volume identifier, so it cannot be catalogued safely.");
      return;
    }
    setScanningId(drive.persistentIdentifier);
    setCancellingId(null);
    setScanCancelledId(null);
    setScanComplete(null);
    setScanProgress({ persistentIdentifier: drive.persistentIdentifier, fileCount: 0, directoryCount: 0, cataloguedBytes: 0, skippedCount: 0, currentPath: "" });
    setError(null);
    try {
      const result = await invoke<ScanProgress>("scan_drive", { persistentIdentifier: drive.persistentIdentifier });
      setScanComplete({ ...result, currentPath: "" });
      await refresh();

      // A successful rescan replaces the SQLite snapshot. If this drive is
      // currently open, reload the visible folder/search from that new snapshot
      // so the browser cannot keep showing stale in-memory entries.
      if (browserDrive?.persistentIdentifier === drive.persistentIdentifier) {
        if (searchQuery.trim()) {
          await searchCatalogue(browserDrive, searchQuery);
        } else {
          await openFolder(browserDrive, browserPath);
        }
      }

      window.setTimeout(() => setScanComplete(null), 4000);
    } catch (cause) {
      const message = String(cause);
      if (message.includes("Scan cancelled.")) {
        setScanCancelledId(drive.persistentIdentifier);
        window.setTimeout(() => setScanCancelledId(null), 4000);
      } else {
        setError(message);
      }
    } finally {
      setScanningId(null);
      setCancellingId(null);
      setScanProgress(null);
    }
  };

  const catalogueFor = (id: string | null) =>
    id ? catalogued.find((item) => item.persistentIdentifier === id) : undefined;

  const pathParts = browserPath ? browserPath.split("/") : [];
  const offline = catalogued.filter((drive) => !connectedIds.has(drive.persistentIdentifier));

  if (browserDrive) {
    const liveDrive = catalogued.find((drive) => drive.persistentIdentifier === browserDrive.persistentIdentifier) ?? browserDrive;
    const online = connectedIds.has(liveDrive.persistentIdentifier);

    return (
      <main className="app-shell browser-shell">
        <header className="browser-header">
          <button className="back-button" onClick={() => { setBrowserDrive(null); setBrowserPath(""); setEntries([]); setSearchQuery(""); setSearchResults([]); }}>
            Back to drives
          </button>
          <div className="browser-title-row">
            <div>
              <p className="eyebrow">{online ? "CONNECTED CATALOGUE" : "OFFLINE CATALOGUE"}</p>
              <h1>{liveDrive.name}</h1>
            </div>
            <span className={online ? "browser-status online-label" : "browser-status offline-label"}>
              {online ? "CONNECTED" : "OFFLINE"}
            </span>
          </div>
          <nav className="breadcrumbs" aria-label="Folder path">
            <button onClick={() => void openFolder(liveDrive, "")}>{liveDrive.name}</button>
            {pathParts.map((part, index) => {
              const path = pathParts.slice(0, index + 1).join("/");
              return (
                <span key={path}>
                  <span className="crumb-separator">/</span>
                  <button onClick={() => void openFolder(liveDrive, path)}>{part}</button>
                </span>
              );
            })}
          </nav>
          <div className="catalogue-search">
            <input type="search" value={searchQuery} placeholder={`Search ${liveDrive.name}`}
              aria-label={`Search ${liveDrive.name} catalogue`}
              onChange={(event) => void searchCatalogue(liveDrive, event.target.value)} />
            {searchQuery.trim() && <span className="search-summary">
              {searching ? "Searching…" : `${searchResults.length}${searchResults.length === 200 ? "+" : ""} result${searchResults.length === 1 ? "" : "s"}`}
            </span>}
          </div>
        </header>

        {error && <div className="notice error">{error}</div>}

        {searchQuery.trim() && <section className="search-results" aria-live="polite">
          {searching ? <div className="browser-message">Searching catalogue…</div>
          : searchResults.length === 0 ? <div className="browser-message">No matching files or folders.</div>
          : searchResults.map((entry) => <button className="search-result" key={entry.relativePath}
              onClick={() => void openSearchResult(liveDrive, entry)}>
              <span className="search-result-main"><strong>{entry.name}</strong><span>{entry.relativePath}</span></span>
              <span>{entry.isDirectory ? "Folder" : formatBytes(entry.sizeBytes)}</span>
              <span>{entry.modifiedAt ? formatDate(entry.modifiedAt) : "—"}</span>
            </button>)}
        </section>}

        {!searchQuery.trim() && <section className="file-browser">
          <div className="file-browser-head">
            <span>Name</span><span>Modified</span><span>Size</span>
          </div>
          {browserLoading ? (
            <div className="browser-message">Loading catalogue…</div>
          ) : entries.length === 0 ? (
            <div className="browser-message">This folder is empty in the catalogue.</div>
          ) : (
            entries.map((entry) => (
              <button
                className={`file-row ${entry.isDirectory ? "folder-row" : ""}`}
                key={entry.relativePath}
                disabled={!entry.isDirectory}
                onClick={() => entry.isDirectory && void openFolder(liveDrive, entry.relativePath)}
              >
                <span className="file-name">
                  <span className="file-kind" aria-hidden="true">{entry.isDirectory ? "▸" : ""}</span>
                  {entry.name}
                </span>
                <span>{entry.modifiedAt ? formatDate(entry.modifiedAt) : "—"}</span>
                <span>{entry.isDirectory ? "Folder" : formatBytes(entry.sizeBytes)}</span>
              </button>
            ))
          )}
        </section>}

        <footer className="safety-note">
          This view comes from Media Mapper's local catalogue. Search and browsing do not read the drive.
        </footer>
      </main>
    );
  }

  return (
    <main className="app-shell">
      <header className="app-header">
        <div>
          <p className="eyebrow">MEDIA MAPPER</p>
          <h1>Drive catalogue</h1>
          <p className="intro">Catalogue external drives once, then keep browsing their metadata after they are disconnected.</p>
        </div>
        <button className="refresh-button" onClick={() => void refresh()} disabled={loading || scanningId !== null}>
          {loading ? "Checking…" : "Refresh"}
        </button>
      </header>

      {error && <div className="notice error">{error}</div>}

      {catalogued.length > 0 && <section className="section-block">
        <div className="section-heading"><h2>Search all drives</h2><span>{catalogued.length}</span></div>
        <div className="catalogue-search">
          <input
            type="search"
            value={libraryQuery}
            placeholder="Search every catalogued drive"
            aria-label="Search all catalogued drives"
            onChange={(event) => void searchLibrary(event.target.value)}
          />
          {libraryQuery.trim() && <span className="search-summary">
            {librarySearching ? "Searching…" : `${libraryResults.length}${libraryResults.length === 200 ? "+" : ""} result${libraryResults.length === 1 ? "" : "s"}`}
          </span>}
        </div>

        {libraryQuery.trim() && <div className="search-results" aria-live="polite">
          {librarySearching ? <div className="browser-message">Searching all catalogues…</div>
          : libraryResults.length === 0 ? <div className="browser-message">No matching files or folders.</div>
          : libraryResults.map((result) => <button
              className="search-result"
              key={`${result.driveId}:${result.relativePath}`}
              onClick={() => void openLibraryResult(result)}
            >
              <span className="search-result-main">
                <strong>{result.name}</strong>
                <span>{result.driveName} / {result.relativePath}</span>
              </span>
              <span>{connectedIds.has(result.driveId) ? "Connected" : "Offline"}</span>
              <span>{result.isDirectory ? "Folder" : formatBytes(result.sizeBytes)}</span>
            </button>)}
        </div>}
      </section>}

      <section className="section-block">
        <div className="section-heading"><h2>Connected</h2><span>{connected.length}</span></div>
        {!loading && connected.length === 0 && (
          <div className="empty-state compact"><h3>No external drives detected</h3><p>Connect a drive, then choose Refresh.</p></div>
        )}
        <div className="drive-list">
          {connected.map((drive) => {
            const catalogue = catalogueFor(drive.persistentIdentifier);
            const scanning = scanningId === drive.persistentIdentifier;
            return (
              <article className="drive-card" key={drive.persistentIdentifier ?? drive.mountPoint}>
                <div className="drive-title-row">
                  <div><span className="status-dot" /><span className="online-label">CONNECTED</span><h3>{drive.name}</h3></div>
                  <div className="drive-actions">
                    <span className="capacity">{formatBytes(drive.totalBytes)}</span>
                    <div className="button-row">
                      {catalogue && <button className="browse-button" onClick={() => void openFolder(catalogue, "")}>Browse catalogue</button>}
                      <button className="scan-button" disabled={scanningId !== null || !drive.persistentIdentifier} onClick={() => void scan(drive)}>
                        {scanning ? "Scanning…" : catalogue ? "Rescan drive" : "Scan drive"}
                      </button>
                    </div>
                  </div>
                </div>
                <dl className="drive-details offline-details">
                  <div><dt>Available</dt><dd>{formatBytes(drive.availableBytes)}</dd></div>
                  <div><dt>Filesystem</dt><dd>{drive.filesystem ?? "Unknown"}</dd></div>
                  <div><dt>Mount point</dt><dd>{drive.mountPoint}</dd></div>
                  <div><dt>Last scanned</dt><dd>{formatDate(catalogue?.lastScannedAt ?? null)}</dd></div>
                  {catalogue && <div><dt>Files catalogued</dt><dd>{catalogue.fileCount.toLocaleString()}</dd></div>}
                  {catalogue && <div><dt>Folders</dt><dd>{catalogue.directoryCount.toLocaleString()}</dd></div>}
                </dl>
                <div className="scan-status-slot" aria-live="polite">
                  {scanning && scanProgress?.persistentIdentifier === drive.persistentIdentifier ? (
                    <div className="scan-progress">
                      <div className="scan-progress-row">
                        <strong>{cancellingId === drive.persistentIdentifier ? "Cancelling scan…" : "Scanning catalogue…"}</strong>
                        <div className="scan-progress-actions">
                          <span>{(scanProgress.fileCount + scanProgress.directoryCount).toLocaleString()} items</span>
                          <button className="cancel-scan-button" disabled={cancellingId === drive.persistentIdentifier} onClick={() => void cancelScan(drive.persistentIdentifier!)}>
                            {cancellingId === drive.persistentIdentifier ? "Cancelling…" : "Cancel scan"}
                          </button>
                        </div>
                      </div>
                      <div className="scan-progress-stats"><span>{scanProgress.fileCount.toLocaleString()} files</span><span>{scanProgress.directoryCount.toLocaleString()} folders</span><span>{formatBytes(scanProgress.cataloguedBytes)}</span>{scanProgress.skippedCount > 0 && <span>{scanProgress.skippedCount.toLocaleString()} skipped</span>}</div>
                      <div className="scan-progress-path">{scanProgress.currentPath || "Starting scan…"}</div>
                    </div>
                  ) : scanComplete?.persistentIdentifier === drive.persistentIdentifier ? (
                    <div className="scan-complete">Scan complete · {scanComplete.fileCount.toLocaleString()} files · {scanComplete.directoryCount.toLocaleString()} folders · {formatBytes(scanComplete.cataloguedBytes)}{scanComplete.skippedCount > 0 ? ` · ${scanComplete.skippedCount.toLocaleString()} skipped` : ""}</div>
                  ) : scanCancelledId === drive.persistentIdentifier ? (
                    <div className="scan-cancelled">Scan cancelled.</div>
                  ) : null}
                </div>
              </article>
            );
          })}
        </div>
      </section>

      <section className="section-block">
        <div className="section-heading"><h2>Offline catalogue</h2><span>{offline.length}</span></div>
        {offline.length === 0 ? (
          <div className="empty-state compact"><h3>No offline drives yet</h3><p>Scan a connected drive, then disconnect it. Its catalogue will remain here.</p></div>
        ) : (
          <div className="drive-list">
            {offline.map((drive) => (
              <article className="drive-card offline" key={drive.persistentIdentifier}>
                <div className="drive-title-row">
                  <div><span className="status-dot offline-dot" /><span className="offline-label">OFFLINE</span><h3>{drive.name}</h3></div>
                  <div className="drive-actions">
                    <span className="capacity">{formatBytes(drive.totalBytes)}</span>
                    <button className="browse-button" onClick={() => void openFolder(drive, "")}>Browse catalogue</button>
                  </div>
                </div>
                <dl className="drive-details offline-details">
                  <div><dt>Files</dt><dd>{drive.fileCount.toLocaleString()}</dd></div>
                  <div><dt>Folders</dt><dd>{drive.directoryCount.toLocaleString()}</dd></div>
                  <div><dt>Catalogued</dt><dd>{formatBytes(drive.cataloguedBytes)}</dd></div>
                  <div><dt>Last scanned</dt><dd>{formatDate(drive.lastScannedAt)}</dd></div>
                </dl>
              </article>
            ))}
          </div>
        )}
      </section>

      <footer className="safety-note">Scanning reads names, paths, sizes and timestamps only. It does not open media contents, rename, move, copy or delete files.</footer>
    </main>
  );
}

export default App;
