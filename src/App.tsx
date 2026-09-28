import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
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

function formatBytes(bytes: number | null) {
  if (bytes === null) return "Unknown";
  if (bytes === 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB", "PB"];
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1000)), units.length - 1);
  return `${(bytes / 1000 ** exponent).toFixed(exponent >= 4 ? 2 : 1)} ${units[exponent]}`;
}

function formatDate(timestamp: number | null) {
  if (!timestamp) return "Never";
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(timestamp * 1000));
}

function App() {
  const [connected, setConnected] = useState<DriveInfo[]>([]);
  const [catalogued, setCatalogued] = useState<CataloguedDrive[]>([]);
  const [loading, setLoading] = useState(true);
  const [scanningId, setScanningId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

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

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const connectedIds = useMemo(
    () => new Set(connected.flatMap((drive) => drive.persistentIdentifier ? [drive.persistentIdentifier] : [])),
    [connected],
  );

  const scan = async (drive: DriveInfo) => {
    if (!drive.persistentIdentifier) {
      setError("This drive does not provide a stable volume identifier, so it cannot be catalogued safely.");
      return;
    }

    setScanningId(drive.persistentIdentifier);
    setError(null);
    try {
      await invoke("scan_drive", { persistentIdentifier: drive.persistentIdentifier });
      await refresh();
    } catch (cause) {
      setError(String(cause));
    } finally {
      setScanningId(null);
    }
  };

  const offline = catalogued.filter((drive) => !connectedIds.has(drive.persistentIdentifier));

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

      <section className="section-block">
        <div className="section-heading">
          <h2>Connected</h2>
          <span>{connected.length}</span>
        </div>

        {!loading && connected.length === 0 && (
          <div className="empty-state compact">
            <h3>No external drives detected</h3>
            <p>Connect a drive, then choose Refresh.</p>
          </div>
        )}

        <div className="drive-list">
          {connected.map((drive) => {
            const catalogue = drive.persistentIdentifier
              ? catalogued.find((item) => item.persistentIdentifier === drive.persistentIdentifier)
              : undefined;
            const used = drive.totalBytes !== null && drive.availableBytes !== null
              ? drive.totalBytes - drive.availableBytes
              : null;
            const usedPercent = used !== null && drive.totalBytes
              ? Math.max(0, Math.min(100, (used / drive.totalBytes) * 100))
              : null;
            const scanning = scanningId === drive.persistentIdentifier;

            return (
              <article className="drive-card" key={drive.persistentIdentifier ?? drive.mountPoint}>
                <div className="drive-title-row">
                  <div>
                    <span className="status-dot" aria-hidden="true" />
                    <span className="online-label">CONNECTED</span>
                    <h3>{drive.name}</h3>
                  </div>
                  <div className="drive-actions">
                    <span className="capacity">{formatBytes(drive.totalBytes)}</span>
                    <button
                      className="scan-button"
                      disabled={scanningId !== null || !drive.persistentIdentifier}
                      onClick={() => void scan(drive)}
                    >
                      {scanning ? "Scanning…" : catalogue ? "Rescan drive" : "Scan drive"}
                    </button>
                  </div>
                </div>

                {usedPercent !== null && (
                  <div className="capacity-bar" aria-label={`${usedPercent.toFixed(0)} percent used`}>
                    <span style={{ width: `${usedPercent}%` }} />
                  </div>
                )}

                <dl className="drive-details">
                  <div><dt>Available</dt><dd>{formatBytes(drive.availableBytes)}</dd></div>
                  <div><dt>Filesystem</dt><dd>{drive.filesystem ?? "Unknown"}</dd></div>
                  <div><dt>Mount point</dt><dd>{drive.mountPoint}</dd></div>
                  <div><dt>Last scanned</dt><dd>{formatDate(catalogue?.lastScannedAt ?? null)}</dd></div>
                  {catalogue && <div><dt>Files catalogued</dt><dd>{catalogue.fileCount.toLocaleString()}</dd></div>}
                  {catalogue && <div><dt>Catalogue size</dt><dd>{formatBytes(catalogue.cataloguedBytes)}</dd></div>}
                </dl>
              </article>
            );
          })}
        </div>
      </section>

      <section className="section-block">
        <div className="section-heading">
          <h2>Offline catalogue</h2>
          <span>{offline.length}</span>
        </div>

        {offline.length === 0 ? (
          <div className="empty-state compact">
            <h3>No offline drives yet</h3>
            <p>Scan a connected drive, then disconnect it. Its catalogue will remain here.</p>
          </div>
        ) : (
          <div className="drive-list">
            {offline.map((drive) => (
              <article className="drive-card offline" key={drive.persistentIdentifier}>
                <div className="drive-title-row">
                  <div>
                    <span className="status-dot offline-dot" aria-hidden="true" />
                    <span className="offline-label">OFFLINE</span>
                    <h3>{drive.name}</h3>
                  </div>
                  <span className="capacity">{formatBytes(drive.totalBytes)}</span>
                </div>
                <dl className="drive-details offline-details">
                  <div><dt>Files</dt><dd>{drive.fileCount.toLocaleString()}</dd></div>
                  <div><dt>Folders</dt><dd>{drive.directoryCount.toLocaleString()}</dd></div>
                  <div><dt>Catalogued</dt><dd>{formatBytes(drive.cataloguedBytes)}</dd></div>
                  <div><dt>Last scanned</dt><dd>{formatDate(drive.lastScannedAt)}</dd></div>
                  <div><dt>Last mount point</dt><dd>{drive.lastMountPoint ?? "Unknown"}</dd></div>
                  <div><dt>Volume ID</dt><dd>{drive.persistentIdentifier}</dd></div>
                </dl>
              </article>
            ))}
          </div>
        )}
      </section>

      <footer className="safety-note">
        Scanning reads names, paths, sizes and timestamps only. It does not open media contents, rename, move, copy or delete files.
      </footer>
    </main>
  );
}

export default App;
