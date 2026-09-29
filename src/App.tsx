import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
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

type LargestFile = {
  driveId: string;
  driveName: string;
  relativePath: string;
  name: string;
  sizeBytes: number;
  modifiedAt: number | null;
};

type DuplicateFile = {
  driveId: string;
  driveName: string;
  relativePath: string;
  name: string;
  sizeBytes: number;
  modifiedAt: number | null;
};

type DuplicateGroup = {
  name: string;
  sizeBytes: number;
  copies: number;
  potentialWastedBytes: number;
  files: DuplicateFile[];
};


type Location = {
  id: string;
  kind: "external_drive" | "local_folder";
  displayName: string;
  userLabel: string | null;
  driveId: string | null;
  localPath: string | null;
};

type PlannedMove = {
  id: number;
  sourceDriveId: string;
  sourceDriveName: string;
  sourceRelativePath: string;
  sourceName: string;
  sourceSizeBytes: number | null;
  sourceIsDirectory: boolean;
  destinationLocationId: string;
  destinationLocationName: string;
  destinationRelativePath: string;
  createdAt: number;
};

type PlannedFolderEntry = {
  moveId: number;
  sourceDriveId: string;
  sourceDriveName: string;
  sourceRelativePath: string;
  name: string;
  isDirectory: boolean;
  sizeBytes: number | null;
  destinationRelativePath: string;
  // A folder that exists only in the plan, created by moves planned into it.
  isNewFolder: boolean;
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

// Searches wait for a pause in typing before querying, so each keystroke does
// not start its own full catalogue search. Short enough to feel immediate.
const SEARCH_DEBOUNCE_MS = 200;

// Cleans a typed folder path: trims each folder name and drops empty and "."
// parts, so " ./Video//Archive/ " becomes "Video/Archive". ".." is left in
// place for the backend to reject with an explanation.
function normaliseFolderInput(folder: string) {
  return folder
    .split("/")
    .map((part) => part.trim())
    .filter((part) => part !== "" && part !== ".")
    .join("/");
}

function waitForTypingPause() {
  return new Promise<void>((resolve) => window.setTimeout(resolve, SEARCH_DEBOUNCE_MS));
}

// Dot-files are system metadata (.Trashes, .Spotlight-V100, …) and are hidden
// unless the user asks to see them.
function isHiddenName(name: string) {
  return name.startsWith(".");
}

function isHiddenPath(relativePath: string) {
  return relativePath.split("/").some(isHiddenName);
}

function CapacitySummary({ totalBytes, availableBytes, atLastScan }: {
  totalBytes: number | null;
  availableBytes: number | null;
  atLastScan?: boolean;
}) {
  if (totalBytes === null || availableBytes === null || totalBytes <= 0) return null;
  const usedPercent = Math.min(100, Math.max(0, ((totalBytes - availableBytes) / totalBytes) * 100));
  return (
    <div className="capacity-summary">
      <div className="capacity-text">
        <strong>{formatBytes(availableBytes)} free</strong> of {formatBytes(totalBytes)}
        {atLastScan && <span> at last scan</span>}
      </div>
      <div className="capacity-bar" aria-hidden="true"><span style={{ width: `${usedPercent}%` }} /></div>
    </div>
  );
}

function parentFolder(relativePath: string) {
  const separator = relativePath.lastIndexOf("/");
  return separator === -1 ? "" : relativePath.slice(0, separator);
}

// Paths are shown as "Location / Folder / Subfolder" (UI guidelines §10).
function formatLocationPath(locationName: string, relativePath: string) {
  return [locationName, ...relativePath.split("/").filter(Boolean)].join(" / ");
}

// The single representation of a planned move, used wherever a plan is shown,
// so plans always read the same way and never look like a real move.
function PlannedPath({ direction, locationName, folder }: {
  direction: "to" | "from";
  locationName: string;
  folder: string;
}) {
  const path = formatLocationPath(locationName, folder);
  return (
    <span className="planned-path" title={path}>
      {direction === "to" ? "Planned → " : "Planned from "}{path}
    </span>
  );
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
  const [showHiddenItems, setShowHiddenItems] = useState(false);
  const visibleLibraryResults = showHiddenItems
    ? libraryResults
    : libraryResults.filter((result) => !isHiddenPath(result.relativePath));
  const [librarySearching, setLibrarySearching] = useState(false);
  const [largestFiles, setLargestFiles] = useState<LargestFile[]>([]);
  const [largestFilesLoading, setLargestFilesLoading] = useState(false);
  const [showLargestFiles, setShowLargestFiles] = useState(false);
  const [duplicateGroups, setDuplicateGroups] = useState<DuplicateGroup[]>([]);
  const [duplicatesLoading, setDuplicatesLoading] = useState(false);
  const [showDuplicates, setShowDuplicates] = useState(false);
  const [expandedDuplicate, setExpandedDuplicate] = useState<string | null>(null);
  const [plannedMoves, setPlannedMoves] = useState<PlannedMove[]>([]);
  const [plannedFolderEntries, setPlannedFolderEntries] = useState<PlannedFolderEntry[]>([]);
  const [locations, setLocations] = useState<Location[]>([]);
  const [editingDriveLabelId, setEditingDriveLabelId] = useState<string | null>(null);
  const [driveLabelDraft, setDriveLabelDraft] = useState("");
  const [savingDriveLabel, setSavingDriveLabel] = useState(false);
  const [selectedPlanEntry, setSelectedPlanEntry] = useState<CatalogueEntry | null>(null);
  const [planDestinationLocationId, setPlanDestinationLocationId] = useState("");
  const [planDestinationFolder, setPlanDestinationFolder] = useState("");
  const [savingPlan, setSavingPlan] = useState(false);
  const [draggedEntry, setDraggedEntry] = useState<CatalogueEntry | null>(null);
  const [dragOverFolderPath, setDragOverFolderPath] = useState<string | null>(null);

  const loadLocations = useCallback(async () => {
    try {
      setLocations(await invoke<Location[]>("list_locations"));
    } catch (cause) {
      setError(String(cause));
    }
  }, []);

  const loadPlannedMoves = useCallback(async () => {
    try {
      setPlannedMoves(await invoke<PlannedMove[]>("list_planned_moves"));
    } catch (cause) {
      setError(String(cause));
    }
  }, []);

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
    void loadPlannedMoves();
    void loadLocations();
  }, [refresh, loadPlannedMoves, loadLocations]);

  // Keep physical drive availability current while the app is open.
  // This only asks macOS which external drives are mounted. It does not
  // scan, catalogue, or read file contents.
  // The next check is scheduled only after the previous one finishes, so slow
  // `diskutil` calls cannot pile up.
  useEffect(() => {
    let active = true;
    let timeout: number | undefined;

    const refreshConnectedDrives = async () => {
      try {
        const drives = await invoke<DriveInfo[]>("list_external_drives");
        if (active) {
          setConnected(drives);
        }
      } catch {
        // The main Refresh action remains responsible for surfacing errors.
        // A transient background check should not interrupt the user.
      }
      if (active) {
        timeout = window.setTimeout(() => void refreshConnectedDrives(), 2000);
      }
    };

    timeout = window.setTimeout(() => void refreshConnectedDrives(), 2000);

    return () => {
      active = false;
      window.clearTimeout(timeout);
    };
  }, []);

  // Commands run concurrently, so responses can arrive out of order. Each
  // loader numbers its requests and ignores any response that is not the latest.
  const folderRequest = useRef(0);
  const catalogueSearchRequest = useRef(0);
  const librarySearchRequest = useRef(0);

  // What the browser shows right now. A scan runs for a long time, and the
  // user may open a drive while it does, so code that finishes later reads
  // this rather than the values from when it started.
  const browserState = useRef({ browserDrive, browserPath, searchQuery });
  browserState.current = { browserDrive, browserPath, searchQuery };

  // The plan panel opens below the file list, which can be long, so bring it
  // into view and move keyboard focus to its first field when a file is picked.
  const planPanel = useRef<HTMLElement>(null);
  const planLocationSelect = useRef<HTMLSelectElement>(null);
  useEffect(() => {
    if (!selectedPlanEntry) return;
    const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    planPanel.current?.scrollIntoView({ behavior: reduceMotion ? "auto" : "smooth", block: "nearest" });
    planLocationSelect.current?.focus({ preventScroll: true });
  }, [selectedPlanEntry]);

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

  const locationForDrive = (persistentIdentifier: string | null) =>
    persistentIdentifier
      ? locations.find(
          (location) =>
            location.kind === "external_drive" &&
            location.driveId === persistentIdentifier,
        )
      : undefined;

  // Media Mapper labels are the primary drive identity; the volume name is
  // only a fallback when no label has been set.
  const driveDisplayName = (persistentIdentifier: string | null, volumeName: string) =>
    locationForDrive(persistentIdentifier)?.userLabel ?? volumeName;

  const beginDriveLabelEdit = (persistentIdentifier: string, currentLabel: string | null) => {
    setEditingDriveLabelId(persistentIdentifier);
    setDriveLabelDraft(currentLabel ?? "");
  };

  const saveDriveLabel = async (persistentIdentifier: string) => {
    setSavingDriveLabel(true);
    setError(null);

    try {
      await invoke("set_drive_label", {
        persistentIdentifier,
        label: driveLabelDraft,
      });
      await loadLocations();
      setEditingDriveLabelId(null);
      setDriveLabelDraft("");
    } catch (cause) {
      setError(String(cause));
    } finally {
      setSavingDriveLabel(false);
    }
  };

  const openFolder = useCallback(async (drive: CataloguedDrive, path: string) => {
    const request = ++folderRequest.current;
    setBrowserDrive(drive);
    setBrowserPath(path);
    setBrowserLoading(true);
    setError(null);
    try {
      const [catalogueEntries, plannedEntries] = await Promise.all([
        invoke<CatalogueEntry[]>("list_catalogue_entries", {
          persistentIdentifier: drive.persistentIdentifier,
          parentPath: path,
        }),
        invoke<PlannedFolderEntry[]>("list_planned_folder_entries", {
          destinationLocationId: `drive:${drive.persistentIdentifier}`,
          parentPath: path,
        }),
      ]);
      if (request !== folderRequest.current) return;
      setEntries(catalogueEntries);
      setPlannedFolderEntries(plannedEntries);
    } catch (cause) {
      if (request !== folderRequest.current) return;
      setError(String(cause));
      setEntries([]);
    } finally {
      if (request === folderRequest.current) setBrowserLoading(false);
    }
  }, []);

  const searchCatalogue = useCallback(async (drive: CataloguedDrive, query: string) => {
    const request = ++catalogueSearchRequest.current;
    setSearchQuery(query);
    const trimmed = query.trim();
    if (!trimmed) { setSearchResults([]); setSearching(false); return; }
    setSearching(true); setError(null);
    await waitForTypingPause();
    // A newer keystroke has taken over; it will run the search instead.
    if (request !== catalogueSearchRequest.current) return;
    try {
      const results = await invoke<CatalogueEntry[]>("search_catalogue", {
        persistentIdentifier: drive.persistentIdentifier, query: trimmed,
      });
      if (request === catalogueSearchRequest.current) setSearchResults(results);
    } catch (cause) {
      if (request === catalogueSearchRequest.current) { setError(String(cause)); setSearchResults([]); }
    }
    finally { if (request === catalogueSearchRequest.current) setSearching(false); }
  }, []);

  const searchLibrary = useCallback(async (query: string) => {
    const request = ++librarySearchRequest.current;
    setLibraryQuery(query);
    const trimmed = query.trim();
    if (!trimmed) {
      setLibraryResults([]);
      setLibrarySearching(false);
      return;
    }

    setLibrarySearching(true);
    setError(null);
    await waitForTypingPause();
    // A newer keystroke has taken over; it will run the search instead.
    if (request !== librarySearchRequest.current) return;
    try {
      const results = await invoke<LibrarySearchResult[]>("search_all_catalogues", { query: trimmed });
      if (request === librarySearchRequest.current) setLibraryResults(results);
    } catch (cause) {
      if (request !== librarySearchRequest.current) return;
      setError(String(cause));
      setLibraryResults([]);
    } finally {
      if (request === librarySearchRequest.current) setLibrarySearching(false);
    }
  }, []);

  const loadDuplicates = useCallback(async () => {
    if (showDuplicates) {
      setShowDuplicates(false);
      setExpandedDuplicate(null);
      return;
    }

    setDuplicatesLoading(true);
    setError(null);

    try {
      const results = await invoke<DuplicateGroup[]>("probable_duplicates");
      setDuplicateGroups(results);
      setShowDuplicates(true);
    } catch (cause) {
      setError(String(cause));
      setDuplicateGroups([]);
    } finally {
      setDuplicatesLoading(false);
    }
  }, [showDuplicates]);

  const loadLargestFiles = useCallback(async () => {
    if (showLargestFiles) {
      setShowLargestFiles(false);
      return;
    }

    setLargestFilesLoading(true);
    setError(null);

    try {
      const results = await invoke<LargestFile[]>("largest_files");
      setLargestFiles(results);
      setShowLargestFiles(true);
    } catch (cause) {
      setError(String(cause));
      setLargestFiles([]);
    } finally {
      setLargestFilesLoading(false);
    }
  }, [showLargestFiles]);

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

  const openDuplicateFile = async (file: DuplicateFile) => {
    const drive = catalogued.find((item) => item.persistentIdentifier === file.driveId);

    if (!drive) {
      setError("That drive catalogue is no longer available.");
      return;
    }

    await openSearchResult(drive, {
      relativePath: file.relativePath,
      name: file.name,
      isDirectory: false,
      sizeBytes: file.sizeBytes,
      modifiedAt: file.modifiedAt,
    });
  };

  const openLargestFile = async (file: LargestFile) => {
    const drive = catalogued.find((item) => item.persistentIdentifier === file.driveId);

    if (!drive) {
      setError("That drive catalogue is no longer available.");
      return;
    }

    await openSearchResult(drive, {
      relativePath: file.relativePath,
      name: file.name,
      isDirectory: false,
      sizeBytes: file.sizeBytes,
      modifiedAt: file.modifiedAt,
    });
  };

  const beginPlanMove = (entry: CatalogueEntry) => {
    setSelectedPlanEntry(entry);
    setPlanDestinationLocationId(
      browserDrive ? `drive:${browserDrive.persistentIdentifier}` : ""
    );

    // Start from the item's current folder. This makes the proposed
    // destination explicit and prevents the UI from making a root-level
    // destination look like the file's existing catalogue location.
    const separator = entry.relativePath.lastIndexOf("/");
    const currentFolder =
      separator >= 0 ? entry.relativePath.slice(0, separator) : "";
    setPlanDestinationFolder(currentFolder);

    setError(null);
  };

  const savePlannedMove = async () => {
    if (!browserDrive || !selectedPlanEntry || !planDestinationLocationId) return;

    const folder = normaliseFolderInput(planDestinationFolder);
    const destinationRelativePath = folder
      ? `${folder}/${selectedPlanEntry.name}`
      : selectedPlanEntry.name;

    setSavingPlan(true);
    setError(null);

    try {
      await invoke<number>("create_planned_move", {
        sourceDriveId: browserDrive.persistentIdentifier,
        sourceRelativePath: selectedPlanEntry.relativePath,
        destinationLocationId: planDestinationLocationId,
        destinationRelativePath,
      });
      await loadPlannedMoves();
      await openFolder(browserDrive, browserPath);

      setSelectedPlanEntry(null);
      setPlanDestinationFolder("");
    } catch (cause) {
      setError(String(cause));
    } finally {
      setSavingPlan(false);
    }
  };

  const planEntryToFolder = async (
    entry: CatalogueEntry,
    destinationFolder: string,
  ) => {
    if (!browserDrive) return;

    const destinationRelativePath = destinationFolder
      ? `${destinationFolder}/${entry.name}`
      : entry.name;

    setError(null);

    try {
      await invoke<number>("create_planned_move", {
        sourceDriveId: browserDrive.persistentIdentifier,
        sourceRelativePath: entry.relativePath,
        destinationLocationId: `drive:${browserDrive.persistentIdentifier}`,
        destinationRelativePath,
      });

      await loadPlannedMoves();
      await openFolder(browserDrive, browserPath);
    } catch (cause) {
      setError(String(cause));
    }
  };

  const addFolderOnThisMac = async () => {
    setError(null);

    try {
      const selected = await open({
        directory: true,
        multiple: false,
        title: "Choose a Media Mapper destination folder",
      });

      if (!selected || Array.isArray(selected)) return;

      const location = await invoke<Location>("add_local_folder_location", {
        path: selected,
      });

      await loadLocations();
      setPlanDestinationLocationId(location.id);
    } catch (cause) {
      setError(String(cause));
    }
  };

  const removePlannedMove = async (id: number) => {
    setError(null);
    try {
      await invoke("remove_planned_move", { id });
      await loadPlannedMoves();
    } catch (cause) {
      setError(String(cause));
    }
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
      // A scan can add a drive's location or rename it, and changes the names
      // and sizes planned moves show, so reload those alongside the drives.
      await Promise.all([refresh(), loadLocations(), loadPlannedMoves()]);

      // A successful rescan replaces the SQLite snapshot. If this drive is
      // currently open, reload the visible folder/search from that new snapshot
      // so the browser cannot keep showing stale in-memory entries.
      const shown = browserState.current;
      if (shown.browserDrive?.persistentIdentifier === drive.persistentIdentifier) {
        if (shown.searchQuery.trim()) {
          await searchCatalogue(shown.browserDrive, shown.searchQuery);
        } else {
          await openFolder(shown.browserDrive, shown.browserPath);
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
    const liveDriveName = driveDisplayName(liveDrive.persistentIdentifier, liveDrive.name);
    const liveDriveHasLabel = liveDriveName !== liveDrive.name;

    const normalisedPlanFolder = normaliseFolderInput(planDestinationFolder);
    const proposedDestinationPath = selectedPlanEntry
      ? (normalisedPlanFolder
          ? `${normalisedPlanFolder}/${selectedPlanEntry.name}`
          : selectedPlanEntry.name)
      : "";

    const planIsCurrentLocation =
      selectedPlanEntry !== null &&
      planDestinationLocationId === `drive:${liveDrive.persistentIdentifier}` &&
      proposedDestinationPath === selectedPlanEntry.relativePath;

    const plannedMoveBySourcePath = new Map(
      plannedMoves
        .filter((move) => move.sourceDriveId === liveDrive.persistentIdentifier)
        .map((move) => [move.sourceRelativePath, move]),
    );

    const plannedFromPath = (planned: PlannedFolderEntry) => {
      // Projected children of a planned folder move should describe the
      // original location of the moved folder, not repeat each child's
      // increasingly long source path.
      const rootMove = plannedMoves.find((move) => move.id === planned.moveId);
      return (
        <PlannedPath
          direction="from"
          locationName={driveDisplayName(
            rootMove?.sourceDriveId ?? planned.sourceDriveId,
            rootMove?.sourceDriveName ?? planned.sourceDriveName,
          )}
          folder={parentFolder(rootMove?.sourceRelativePath ?? planned.sourceRelativePath)}
        />
      );
    };

    const visibleEntries = showHiddenItems
      ? entries
      : entries.filter((entry) => !isHiddenName(entry.name));
    const visiblePlannedFolderEntries = showHiddenItems
      ? plannedFolderEntries
      : plannedFolderEntries.filter((planned) => !isHiddenName(planned.name));
    const visibleSearchResults = showHiddenItems
      ? searchResults
      : searchResults.filter((entry) => !isHiddenPath(entry.relativePath));

    return (
      <main className="app-shell browser-shell">
        <header className="browser-header">
          <button className="back-button" onClick={() => { setBrowserDrive(null); setBrowserPath(""); setEntries([]); setPlannedFolderEntries([]); setSearchQuery(""); setSearchResults([]); }}>
            Back to drives
          </button>
          <div className="browser-title">
            <div className="browser-status">
              <span className={online ? "status-dot" : "status-dot offline-dot"} />
              <span className={online ? "online-label" : "offline-label"}>{online ? "CONNECTED" : "OFFLINE"}</span>
            </div>
            <h1>{liveDriveName}</h1>
            {liveDriveHasLabel && <div className="drive-volume-name">{liveDrive.name}</div>}
          </div>
          {/* At the drive root the breadcrumb would only repeat the title. */}
          {pathParts.length > 0 && <nav className="breadcrumbs" aria-label="Folder path">
            <button onClick={() => void openFolder(liveDrive, "")}>{liveDriveName}</button>
            {pathParts.map((part, index) => {
              const path = pathParts.slice(0, index + 1).join("/");
              const current = index === pathParts.length - 1;
              return (
                <span key={path}>
                  <span className="crumb-separator">/</span>
                  {current
                    ? <span className="crumb-current" aria-current="page">{part}</span>
                    : <button onClick={() => void openFolder(liveDrive, path)}>{part}</button>}
                </span>
              );
            })}
          </nav>}
          <div className="catalogue-search">
            <input type="search" value={searchQuery} placeholder={`Search ${liveDriveName}`}
              aria-label={`Search ${liveDriveName} catalogue`}
              onChange={(event) => void searchCatalogue(liveDrive, event.target.value)} />
            {searchQuery.trim() && <span className="search-summary">
              {searching ? "Searching…" : `${visibleSearchResults.length}${searchResults.length === 200 ? "+" : ""} result${visibleSearchResults.length === 1 ? "" : "s"}`}
            </span>}
            <label className="hidden-items-toggle">
              <input type="checkbox" checked={showHiddenItems} onChange={(event) => setShowHiddenItems(event.target.checked)} />
              Show hidden items
            </label>
          </div>
        </header>

        {error && <div className="notice error">{error}</div>}
        {searchQuery.trim() && <section className="search-results" aria-live="polite">
          {searching ? <div className="browser-message">Searching catalogue…</div>
          : visibleSearchResults.length === 0 ? <div className="browser-message">No matching files or folders.</div>
          : visibleSearchResults.map((entry) => <button className="search-result catalogue-search-result" key={entry.relativePath}
              onClick={() => void openSearchResult(liveDrive, entry)}>
              <span className="search-result-main"><strong>{entry.name}</strong><span>{entry.relativePath}</span></span>
              <span>{entry.modifiedAt ? formatDate(entry.modifiedAt) : "—"}</span>
              <span>{entry.isDirectory ? "Folder" : formatBytes(entry.sizeBytes)}</span>
            </button>)}
        </section>}

        {!searchQuery.trim() && <section className="file-browser">
          <div className="file-browser-head">
            <span>Name</span><span>Modified</span><span>Size</span>
          </div>
          {browserLoading ? (
            <div className="browser-message">Loading catalogue…</div>
          ) : visibleEntries.length === 0 && visiblePlannedFolderEntries.length === 0 ? (
            <div className="browser-message">This folder is empty in the catalogue and plan.</div>
          ) : (
            <>
              {visibleEntries.map((entry) => {
                const plannedMove = plannedMoveBySourcePath.get(entry.relativePath);

                const openOrPlan = () => {
                  if (entry.isDirectory) {
                    void openFolder(liveDrive, entry.relativePath);
                  } else {
                    beginPlanMove(entry);
                  }
                };

                return (
                  <div
                    className={[
                      "file-row",
                      entry.isDirectory ? "folder-row" : "plannable-file-row",
                      plannedMove ? "moving-out-row" : "",
                      draggedEntry?.relativePath === entry.relativePath ? "drag-source-row" : "",
                      dragOverFolderPath === entry.relativePath ? "folder-drop-target" : "",
                    ].filter(Boolean).join(" ")}
                    key={entry.relativePath}
                    role="button"
                    tabIndex={0}
                    draggable
                    onDragStart={(event) => {
                      setDraggedEntry(entry);
                      setDragOverFolderPath(null);
                      event.dataTransfer.effectAllowed = "move";
                      event.dataTransfer.setData("text/plain", entry.relativePath);
                    }}
                    onDragEnd={() => {
                      setDraggedEntry(null);
                      setDragOverFolderPath(null);
                    }}
                    onDragOver={(event) => {
                      if (
                        !entry.isDirectory ||
                        !draggedEntry ||
                        draggedEntry.relativePath === entry.relativePath
                      ) {
                        return;
                      }

                      event.preventDefault();
                      event.dataTransfer.dropEffect = "move";
                      setDragOverFolderPath(entry.relativePath);
                    }}
                    onDragLeave={() => {
                      if (dragOverFolderPath === entry.relativePath) {
                        setDragOverFolderPath(null);
                      }
                    }}
                    onDrop={(event) => {
                      event.preventDefault();
                      event.stopPropagation();

                      const source = draggedEntry;
                      setDraggedEntry(null);
                      setDragOverFolderPath(null);

                      if (
                        !entry.isDirectory ||
                        !source ||
                        source.relativePath === entry.relativePath
                      ) {
                        return;
                      }

                      void planEntryToFolder(source, entry.relativePath);
                    }}
                    onClick={openOrPlan}
                    onKeyDown={(event) => {
                      if (event.key === "Enter" || event.key === " ") {
                        event.preventDefault();
                        openOrPlan();
                      }
                    }}
                  >
                    <span className="file-name">
                      <span className="file-kind" aria-hidden="true">{entry.isDirectory ? "▸" : ""}</span>
                      <span className="file-name-text">
                        <span className="file-name-primary">{entry.name}</span>
                        {plannedMove && (
                          <PlannedPath
                            direction="to"
                            locationName={plannedMove.destinationLocationName}
                            folder={parentFolder(plannedMove.destinationRelativePath)}
                          />
                        )}
                      </span>
                    </span>
                    <span>{entry.modifiedAt ? formatDate(entry.modifiedAt) : "—"}</span>
                    {entry.isDirectory ? (
                      <span className="folder-row-actions">
                        <span>
                          {dragOverFolderPath === entry.relativePath
                            ? "Drop to move here"
                            : "Folder"}
                        </span>
                        <button
                          type="button"
                          className="folder-plan-button"
                          onClick={(event) => {
                            event.stopPropagation();
                            beginPlanMove(entry);
                          }}
                        >
                          Plan move
                        </button>
                      </span>
                    ) : (
                      <span>{formatBytes(entry.sizeBytes)}</span>
                    )}
                  </div>
                );
              })}

              {visiblePlannedFolderEntries
                .filter((planned) =>
                  !entries.some((entry) => entry.relativePath === planned.destinationRelativePath),
                )
                .map((planned) => (
                  <button
                    className={`file-row planned-arrival-row ${planned.isDirectory ? "folder-row" : ""}`}
                    key={`planned:${planned.moveId}:${planned.destinationRelativePath}`}
                    type="button"
                    disabled={!planned.isDirectory}
                    onClick={() => {
                      if (planned.isDirectory) {
                        void openFolder(liveDrive, planned.destinationRelativePath);
                      }
                    }}
                  >
                    <span className="file-name">
                      <span className="file-kind" aria-hidden="true">
                        {planned.isDirectory ? "▸" : ""}
                      </span>
                      <span className="file-name-text">
                        <span className="file-name-primary">{planned.name}</span>
                        {planned.isNewFolder
                          ? <span className="planned-path">New folder in the plan</span>
                          : plannedFromPath(planned)}
                      </span>
                    </span>
                    <span>Planned</span>
                    <span>{planned.isDirectory ? "Folder" : formatBytes(planned.sizeBytes)}</span>
                  </button>
                ))}
            </>
          )}
        </section>}

        {selectedPlanEntry && <section className="plan-move-panel" ref={planPanel}>
          <div className="plan-move-heading">
            <div>
              <p className="eyebrow">PLANNED LOCATION</p>
              <h2>Plan move</h2>
              <p className="section-description">
                {selectedPlanEntry.name} remains at its current location until a future transfer is explicitly executed.
              </p>
            </div>
            <button className="plan-cancel-button" onClick={() => setSelectedPlanEntry(null)}>Cancel</button>
          </div>

          <div className="plan-source">
            <span>Current</span>
            <strong>{liveDriveName} / {selectedPlanEntry.relativePath}</strong>
          </div>

          <div className="plan-fields">
            <label>
              <span>Move to</span>
              <select
                ref={planLocationSelect}
                value={planDestinationLocationId}
                onChange={(event) => setPlanDestinationLocationId(event.target.value)}
              >
                <option value="">Choose a location</option>

                <optgroup label="External drives">
                  {locations
                    .filter((location) => location.kind === "external_drive")
                    .map((location) => (
                      <option key={location.id} value={location.id}>
                        {location.userLabel ?? location.displayName}
                        {location.driveId && connectedIds.has(location.driveId)
                          ? " · Connected"
                          : " · Offline"}
                      </option>
                    ))}
                </optgroup>

                {locations.some((location) => location.kind === "local_folder") && (
                  <optgroup label="This Mac">
                    {locations
                      .filter((location) => location.kind === "local_folder")
                      .map((location) => (
                        <option key={location.id} value={location.id}>
                          {location.displayName}
                        </option>
                      ))}
                  </optgroup>
                )}
              </select>

              <button
                type="button"
                className="plan-add-location"
                onClick={() => void addFolderOnThisMac()}
              >
                Choose folder on this Mac…
              </button>
            </label>

            <label>
              <span>Folder</span>
              <input
                value={planDestinationFolder}
                placeholder="e.g. Video/Archive"
                onChange={(event) => setPlanDestinationFolder(event.target.value)}
              />
            </label>
          </div>

          <div className="plan-preview">
            <span>Planned</span>
            <strong>
              {(() => {
                const destination = locations.find(
                  (location) => location.id === planDestinationLocationId,
                );
                return destination?.userLabel ?? destination?.displayName ?? "Choose a location";
              })()}
              {" / "}
              {normalisedPlanFolder ? `${normalisedPlanFolder}/` : ""}
              {selectedPlanEntry.name}
            </strong>
          </div>

          <div className="plan-actions">
            <span className={planIsCurrentLocation ? "plan-location-warning" : undefined}>
              {planIsCurrentLocation
                ? "Already at this location."
                : "This changes the Media Mapper plan only. No files are moved."}
            </span>
            <button
              className="browse-button"
              disabled={!planDestinationLocationId || savingPlan || planIsCurrentLocation}
              onClick={() => void savePlannedMove()}
            >
              {savingPlan ? "Saving…" : "Plan move"}
            </button>
          </div>
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
          <p className="intro">Catalogue external drives once, then keep browsing them while they're offline.</p>
        </div>
        <button className="refresh-button" onClick={() => void refresh()} disabled={loading || scanningId !== null}>
          {loading ? "Checking…" : "Refresh"}
        </button>
      </header>

      {error && <div className="notice error">{error}</div>}

      {catalogued.length > 0 && <section className="section-block">
        <div className="section-heading"><h2>Search all drives</h2></div>
        <div className="catalogue-search">
          <input
            type="search"
            value={libraryQuery}
            placeholder="Search every catalogued drive"
            aria-label="Search all catalogued drives"
            onChange={(event) => void searchLibrary(event.target.value)}
          />
          {libraryQuery.trim() && <span className="search-summary">
            {librarySearching ? "Searching…" : `${visibleLibraryResults.length}${libraryResults.length === 200 ? "+" : ""} result${visibleLibraryResults.length === 1 ? "" : "s"}`}
          </span>}
          <label className="hidden-items-toggle">
            <input type="checkbox" checked={showHiddenItems} onChange={(event) => setShowHiddenItems(event.target.checked)} />
            Show hidden items
          </label>
        </div>

        {libraryQuery.trim() && <div className="search-results" aria-live="polite">
          {librarySearching ? <div className="browser-message">Searching all catalogues…</div>
          : visibleLibraryResults.length === 0 ? <div className="browser-message">No matching files or folders.</div>
          : visibleLibraryResults.map((result) => <button
              className="search-result"
              key={`${result.driveId}:${result.relativePath}`}
              onClick={() => void openLibraryResult(result)}
            >
              <span className="search-result-main">
                <strong>{result.name}</strong>
                <span>{formatLocationPath(driveDisplayName(result.driveId, result.driveName), result.relativePath)}</span>
              </span>
              <span>{connectedIds.has(result.driveId) ? "Connected" : "Offline"}</span>
              <span>{result.isDirectory ? "Folder" : formatBytes(result.sizeBytes)}</span>
            </button>)}
        </div>}
      </section>}

      <section className="section-block">
        <div className="section-heading"><h2>Connected</h2><span>{connected.length}</span></div>
        {!loading && connected.length === 0 && (
          <div className="empty-state compact"><h3>No external drives detected</h3><p>Connect a drive and it will appear here.</p></div>
        )}
        <div className="drive-list">
          {connected.map((drive) => {
            const catalogue = catalogueFor(drive.persistentIdentifier);
            const scanning = scanningId === drive.persistentIdentifier;
            return (
              <article className="drive-card" key={drive.persistentIdentifier ?? drive.mountPoint}>
                <div className="drive-title-row">
                  <div className="drive-identity">
                    <span className="status-dot" /><span className="online-label">CONNECTED</span>
                    {(() => {
                      const location = locationForDrive(drive.persistentIdentifier);
                      const label = location?.userLabel ?? null;
                      return (
                        <>
                          <h3>{label ?? drive.name}</h3>
                          {label && <div className="drive-volume-name">{drive.name}</div>}
                          {drive.persistentIdentifier && (
                            editingDriveLabelId === drive.persistentIdentifier ? (
                              <div className="drive-label-editor">
                                <input
                                  autoFocus
                                  maxLength={40}
                                  value={driveLabelDraft}
                                  placeholder="Drive label"
                                  onChange={(event) => setDriveLabelDraft(event.target.value)}
                                  onKeyDown={(event) => {
                                    if (event.key === "Enter") void saveDriveLabel(drive.persistentIdentifier!);
                                    if (event.key === "Escape") setEditingDriveLabelId(null);
                                  }}
                                />
                                <button disabled={savingDriveLabel} onClick={() => void saveDriveLabel(drive.persistentIdentifier!)}>Save</button>
                                <button disabled={savingDriveLabel} onClick={() => setEditingDriveLabelId(null)}>Cancel</button>
                              </div>
                            ) : (
                              <button className="drive-label-button" onClick={() => beginDriveLabelEdit(drive.persistentIdentifier!, label)}>
                                {label ? "Edit label" : "Add label"}
                              </button>
                            )
                          )}
                        </>
                      );
                    })()}
                  </div>
                  <div className="drive-actions">
                    <div className="button-row">
                      {catalogue && <button className="browse-button" onClick={() => void openFolder(catalogue, "")}>Browse catalogue</button>}
                      <button className="scan-button" disabled={scanningId !== null || !drive.persistentIdentifier} onClick={() => void scan(drive)}>
                        {scanning ? "Scanning…" : catalogue ? "Rescan drive" : "Scan drive"}
                      </button>
                    </div>
                  </div>
                </div>
                <CapacitySummary totalBytes={drive.totalBytes} availableBytes={drive.availableBytes} />
                <dl className="drive-details offline-details">
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
                  <div className="drive-identity">
                    <span className="status-dot offline-dot" /><span className="offline-label">OFFLINE</span>
                    {(() => {
                      const location = locationForDrive(drive.persistentIdentifier);
                      const label = location?.userLabel ?? null;
                      return (
                        <>
                          <h3>{label ?? drive.name}</h3>
                          {label && <div className="drive-volume-name">{drive.name}</div>}
                          {editingDriveLabelId === drive.persistentIdentifier ? (
                            <div className="drive-label-editor">
                              <input
                                autoFocus
                                maxLength={40}
                                value={driveLabelDraft}
                                placeholder="Drive label"
                                onChange={(event) => setDriveLabelDraft(event.target.value)}
                                onKeyDown={(event) => {
                                  if (event.key === "Enter") void saveDriveLabel(drive.persistentIdentifier);
                                  if (event.key === "Escape") setEditingDriveLabelId(null);
                                }}
                              />
                              <button disabled={savingDriveLabel} onClick={() => void saveDriveLabel(drive.persistentIdentifier)}>Save</button>
                              <button disabled={savingDriveLabel} onClick={() => setEditingDriveLabelId(null)}>Cancel</button>
                            </div>
                          ) : (
                            <button className="drive-label-button" onClick={() => beginDriveLabelEdit(drive.persistentIdentifier, label)}>
                              {label ? "Edit label" : "Add label"}
                            </button>
                          )}
                        </>
                      );
                    })()}
                  </div>
                  <div className="drive-actions">
                    <div className="button-row">
                      <button className="browse-button" onClick={() => void openFolder(drive, "")}>Browse catalogue</button>
                    </div>
                  </div>
                </div>
                <CapacitySummary totalBytes={drive.totalBytes} availableBytes={drive.availableBytes} atLastScan />
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

      {plannedMoves.length > 0 && <section className="section-block">
        <div className="section-heading">
          <h2>Planned moves</h2>
          <span>{plannedMoves.length}</span>
        </div>
        <p className="section-description">Virtual locations only. No files have been moved.</p>

        <div className="planned-move-list">
          {plannedMoves.map((move) => (
            <div className="planned-move-row" key={move.id}>
              <div className="planned-move-main">
                <strong>{move.sourceName}</strong>
                <span>
                  {formatLocationPath(
                    driveDisplayName(move.sourceDriveId, move.sourceDriveName),
                    parentFolder(move.sourceRelativePath),
                  )}
                </span>
                <PlannedPath
                  direction="to"
                  locationName={move.destinationLocationName}
                  folder={parentFolder(move.destinationRelativePath)}
                />
              </div>
              <span>{formatBytes(move.sourceSizeBytes)}</span>
              <button className="plan-remove-button" onClick={() => void removePlannedMove(move.id)}>
                Remove
              </button>
            </div>
          ))}
        </div>
      </section>}

      {catalogued.length > 0 && <section className="section-block">
        <div className="section-heading">
          <h2>Probable duplicates</h2>
          <button
            className="section-action"
            onClick={() => void loadDuplicates()}
            disabled={duplicatesLoading}
          >
            {duplicatesLoading ? "Checking…" : showDuplicates ? "Hide" : "Find duplicates"}
          </button>
        </div>
        <p className="section-description">Same filename and exact file size. Contents have not been compared.</p>

        {showDuplicates && <div className="search-results">
          {duplicateGroups.length === 0 ? (
            <div className="browser-message">No probable duplicates found.</div>
          ) : (
            duplicateGroups.map((group) => {
              const key = `${group.name}:${group.sizeBytes}`;
              const expanded = expandedDuplicate === key;

              return (
                <div className="duplicate-group" key={key}>
                  <button
                    className="search-result"
                    onClick={() => setExpandedDuplicate(expanded ? null : key)}
                  >
                    <span className="search-result-main">
                      <strong>{group.name}</strong>
                      <span>{group.copies} copies · {formatBytes(group.sizeBytes)} each</span>
                    </span>
                    <span>{formatBytes(group.potentialWastedBytes)} potential waste</span>
                    <span>{expanded ? "Hide copies" : "Show copies"}</span>
                  </button>

                  {expanded && <div className="duplicate-files">
                    {group.files.map((file) => (
                      <button
                        className="search-result"
                        key={`${file.driveId}:${file.relativePath}`}
                        onClick={() => void openDuplicateFile(file)}
                      >
                        <span className="search-result-main">
                          <strong>{driveDisplayName(file.driveId, file.driveName)}</strong>
                          <span>{file.relativePath}</span>
                        </span>
                        <span>{connectedIds.has(file.driveId) ? "Connected" : "Offline"}</span>
                        <span>{formatBytes(file.sizeBytes)}</span>
                      </button>
                    ))}
                  </div>}
                </div>
              );
            })
          )}
        </div>}
      </section>}

      {catalogued.length > 0 && <section className="section-block">
        <div className="section-heading">
          <h2>Largest files</h2>
          <button
            className="section-action"
            onClick={() => void loadLargestFiles()}
            disabled={largestFilesLoading}
          >
            {largestFilesLoading ? "Loading…" : showLargestFiles ? "Hide" : "Show 100 largest"}
          </button>
        </div>

        {showLargestFiles && <div className="search-results">
          {largestFiles.length === 0 ? (
            <div className="browser-message">No catalogued files with size information.</div>
          ) : (
            largestFiles.map((file) => (
              <button
                className="search-result"
                key={`${file.driveId}:${file.relativePath}`}
                onClick={() => void openLargestFile(file)}
              >
                <span className="search-result-main">
                  <strong>{file.name}</strong>
                  <span>{formatLocationPath(driveDisplayName(file.driveId, file.driveName), file.relativePath)}</span>
                </span>
                <span>{connectedIds.has(file.driveId) ? "Connected" : "Offline"}</span>
                <span>{formatBytes(file.sizeBytes)}</span>
              </button>
            ))
          )}
        </div>}
      </section>}

      <footer className="safety-note">Scanning reads names, paths, sizes and timestamps only and does not open, rename, move, copy or delete files.</footer>
    </main>
  );
}

export default App;
