import { useCallback, useEffect, useState } from "react";
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

function formatBytes(bytes: number | null) {
  if (bytes === null) return "Unknown";
  if (bytes === 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB", "PB"];
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1000)), units.length - 1);
  return `${(bytes / 1000 ** exponent).toFixed(exponent >= 4 ? 2 : 1)} ${units[exponent]}`;
}

function App() {
  const [drives, setDrives] = useState<DriveInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setDrives(await invoke<DriveInfo[]>("list_external_drives"));
    } catch (cause) {
      setError(String(cause));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  return (
    <main className="app-shell">
      <header className="app-header">
        <div>
          <p className="eyebrow">MEDIA MAPPER</p>
          <h1>External drives</h1>
          <p className="intro">A read-only view of the external storage currently connected to this Mac.</p>
        </div>
        <button className="refresh-button" onClick={() => void refresh()} disabled={loading}>
          {loading ? "Checking…" : "Refresh"}
        </button>
      </header>

      {error && <div className="notice error">Could not read connected drives: {error}</div>}

      {!error && !loading && drives.length === 0 && (
        <section className="empty-state">
          <h2>No external drives detected</h2>
          <p>Connect an external drive, then choose Refresh. Media Mapper does not modify the drive.</p>
        </section>
      )}

      <section className="drive-list" aria-live="polite">
        {drives.map((drive) => {
          const used = drive.totalBytes !== null && drive.availableBytes !== null
            ? drive.totalBytes - drive.availableBytes
            : null;
          const usedPercent = used !== null && drive.totalBytes
            ? Math.max(0, Math.min(100, (used / drive.totalBytes) * 100))
            : null;

          return (
            <article className="drive-card" key={drive.persistentIdentifier ?? drive.mountPoint}>
              <div className="drive-title-row">
                <div>
                  <span className="status-dot" aria-hidden="true" />
                  <span className="online-label">CONNECTED</span>
                  <h2>{drive.name}</h2>
                </div>
                <span className="capacity">{formatBytes(drive.totalBytes)}</span>
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
                <div><dt>Volume ID</dt><dd>{drive.persistentIdentifier ?? "Not provided by macOS"}</dd></div>
                <div><dt>Device</dt><dd>{drive.deviceIdentifier ?? "Unknown"}</dd></div>
              </dl>
            </article>
          );
        })}
      </section>

      <footer className="safety-note">Drive discovery is read-only. Media Mapper cannot move, rename or delete files in this build.</footer>
    </main>
  );
}

export default App;
