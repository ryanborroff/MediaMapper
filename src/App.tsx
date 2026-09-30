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
  // When a scan of this drive last completed. The catalogue is a snapshot
  // from then; the drive may have changed since.
  lastScannedAt: number | null;
  // When Media Mapper last saw this drive connected. Kept for later use.
  lastConnectedAt: number | null;
  fileCount: number;
  directoryCount: number;
  cataloguedBytes: number;
  // Folders the last scan could not fully read, so the catalogue is
  // incomplete there.
  unreadableFolderCount: number;
};

type ScanProgress = {
  persistentIdentifier: string;
  fileCount: number;
  directoryCount: number;
  cataloguedBytes: number;
  skippedCount: number;
  currentPath: string;
};

type ScanResult = ScanProgress & {
  unreadableFolderCount: number;
  // A few of the unreadable folders; "" is the top folder of the drive.
  unreadableExamples: string[];
};

function unreadableSummary(result: ScanResult) {
  const count = result.unreadableFolderCount;
  const example = result.unreadableExamples[0];
  const folders = count === 1 ? "1 folder" : `${count.toLocaleString()} folders`;
  const including = example === undefined
    ? ""
    : `, including ${example === "" ? "the top folder of the drive" : example}`;
  return `Couldn't read ${folders}${including}. Their contents are missing from the catalogue.`;
}

type CatalogueEntry = {
  relativePath: string;
  name: string;
  isDirectory: boolean;
  sizeBytes: number | null;
  modifiedAt: number | null;
  // A folder the last scan could not fully read.
  unreadable?: boolean;
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

type PlanPreflightDestination = {
  locationId: string;
  displayName: string;
  kind: string;
  moveCount: number;
  knownBytes: number;
  unknownSizeCount: number;
  availableBytes: number | null;
  projectedAvailableBytes: number | null;
  capacitySufficient: boolean | null;
};

// Sent by the backend as a file is copied and then verified.
type TransferProgress = {
  plannedMoveId: number;
  stage: "copying" | "verifying";
  bytes: number;
  totalBytes: number;
};

type PlanPreflightIssue = {
  code: string;
  message: string;
  moveId: number | null;
  // The destination an issue is about when it is not about one move.
  locationId: string | null;
};

// The issues that stop one planned move from being copied, by the same rule
// the backend applies before each copy: issues about that move, issues about
// its destination, and any issue that names neither.
function blockingIssues(move: PlannedMove, issues: PlanPreflightIssue[]) {
  return issues.filter((issue) =>
    issue.moveId !== null ? issue.moveId === move.id
      : issue.locationId !== null ? issue.locationId === move.destinationLocationId
        : true);
}

type PlanPreflight = {
  moveCount: number;
  knownBytes: number;
  unknownSizeCount: number;
  destinations: PlanPreflightDestination[];
  issues: PlanPreflightIssue[];
};

type PlanLiveValidation = {
  ready: boolean;
  issues: PlanPreflightIssue[];
};

type TransferRecord = {
  id: number;
  plannedMoveId: number | null;
  sourceDriveId: string;
  sourceRelativePath: string;
  destinationLocationId: string;
  destinationRelativePath: string;
  totalBytes: number | null;
  copiedBytes: number;
  status: string;
  errorMessage: string | null;
  createdAt: number;
  startedAt: number | null;
  completedAt: number | null;
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

const relativeTime = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });

// How long ago a drive was last scanned, such as "Scanned 12 minutes ago".
// It only says when; whether the drive has changed since is unknown.
function formatScanAge(timestamp: number | null, now = Date.now()) {
  if (!timestamp) return "Never scanned";
  const seconds = Math.max(0, Math.floor(now / 1000 - timestamp));
  if (seconds < 60) return "Scanned just now";
  const units: [Intl.RelativeTimeFormatUnit, number][] = [
    ["minute", 60],
    ["hour", 60 * 60],
    ["day", 24 * 60 * 60],
    ["month", 30 * 24 * 60 * 60],
    ["year", 365 * 24 * 60 * 60],
  ];
  let [unit, size] = units[0];
  for (const [candidate, candidateSize] of units) {
    if (seconds >= candidateSize) [unit, size] = [candidate, candidateSize];
  }
  return `Scanned ${relativeTime.format(-Math.floor(seconds / size), unit)}`;
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

// Where a transfer copies from and to, as full "Location / Folder / File"
// paths. Shared by waiting, in-progress and history rows so they read alike.
function TransferPaths({ from, to }: { from: string; to: string }) {
  return (
    <>
      <span className="transfer-path" title={from}><span>From</span>{from}</span>
      <span className="transfer-path" title={to}><span>To</span>{to}</span>
    </>
  );
}

const RECENT_TRANSFER_COUNT = 10;

function fileName(relativePath: string) {
  return relativePath.split("/").pop() ?? relativePath;
}

// Pending, copying and verifying are only seen in history when a run ended
// without recording an outcome, such as the app quitting mid-copy.
function transferOutcome(status: string) {
  if (status === "completed") return { label: "Copied and verified", tone: "completed" };
  if (status === "failed") return { label: "Failed", tone: "failed" };
  return { label: "Did not finish", tone: "incomplete" };
}

function App() {
  const [connected, setConnected] = useState<DriveInfo[]>([]);
  const [catalogued, setCatalogued] = useState<CataloguedDrive[]>([]);
  const [loading, setLoading] = useState(true);
  const [scanningId, setScanningId] = useState<string | null>(null);
  const [scanProgress, setScanProgress] = useState<ScanProgress | null>(null);
  const [scanComplete, setScanComplete] = useState<ScanResult | null>(null);
  const [cancellingId, setCancellingId] = useState<string | null>(null);
  const [scanCancelledId, setScanCancelledId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [activeView, setActiveView] = useState<"drives" | "browse" | "plan" | "transfers">("drives");
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
  const [planPreflight, setPlanPreflight] = useState<PlanPreflight | null>(null);
  const [planValidation, setPlanValidation] = useState<PlanLiveValidation | null>(null);
  const [showCopyConfirmation, setShowCopyConfirmation] = useState(false);
  const [executingPlan, setExecutingPlan] = useState(false);
  // The files being copied in the current run, in order, and how many have
  // been copied and verified so far.
  const [executionProgress, setExecutionProgress] = useState<{ moves: PlannedMove[]; completed: number } | null>(null);
  const [executionResult, setExecutionResult] = useState<{ stopped: boolean; message: string } | null>(null);
  // Set synchronously so a second click cannot start another run before
  // React re-renders with the button disabled.
  const executionRunning = useRef(false);
  const [fileProgress, setFileProgress] = useState<TransferProgress | null>(null);
  // Set by Cancel copy: stops the file in progress and any not yet started.
  const cancelRequested = useRef(false);
  const [cancellingCopy, setCancellingCopy] = useState(false);
  const [transfers, setTransfers] = useState<TransferRecord[]>([]);
  const [showAllTransfers, setShowAllTransfers] = useState(false);
  const [plannedFolderEntries, setPlannedFolderEntries] = useState<PlannedFolderEntry[]>([]);
  const [locations, setLocations] = useState<Location[]>([]);
  const [editingDriveLabelId, setEditingDriveLabelId] = useState<string | null>(null);
  const [driveLabelDraft, setDriveLabelDraft] = useState("");
  const [savingDriveLabel, setSavingDriveLabel] = useState(false);
  const [selectedPlanEntry, setSelectedPlanEntry] = useState<CatalogueEntry | null>(null);
  // Why the plan panel's last action was refused. Shown inside the panel: the
  // page's error banner is at the top, out of sight below a long folder.
  const [planError, setPlanError] = useState<string | null>(null);
  const [planDestinationLocationId, setPlanDestinationLocationId] = useState("");
  const [planDestinationFolder, setPlanDestinationFolder] = useState("");
  const [folderPickerOpen, setFolderPickerOpen] = useState(false);
  const [folderPickerPath, setFolderPickerPath] = useState("");
  const [folderPickerEntries, setFolderPickerEntries] = useState<CatalogueEntry[]>([]);
  const [folderPickerLoading, setFolderPickerLoading] = useState(false);
  const [savingPlan, setSavingPlan] = useState(false);
  const [draggedEntry, setDraggedEntry] = useState<CatalogueEntry | null>(null);
  const [dragOverFolderPath, setDragOverFolderPath] = useState<string | null>(null);
  // Why the last drop onto a folder was refused. Shown on that folder's row,
  // where the user is looking: the page's error banner can be out of sight.
  const [dropRefusal, setDropRefusal] = useState<{
    folderPath: string;
    itemName: string;
    message: string;
  } | null>(null);

  const loadLocations = useCallback(async () => {
    try {
      setLocations(await invoke<Location[]>("list_locations"));
    } catch (cause) {
      setError(String(cause));
    }
  }, []);

  const loadTransfers = useCallback(async () => {
    try {
      setTransfers(await invoke<TransferRecord[]>("list_transfers"));
    } catch (cause) {
      setError(String(cause));
    }
  }, []);

  const loadPlannedMoves = useCallback(async () => {
    try {
      const [moves, preflight, validation] = await Promise.all([
        invoke<PlannedMove[]>("list_planned_moves"),
        invoke<PlanPreflight>("get_plan_preflight"),
        invoke<PlanLiveValidation>("validate_plan"),
      ]);
      setPlannedMoves(moves);
      setPlanPreflight(preflight);
      setPlanValidation(validation);
    } catch (cause) {
      setError(String(cause));
    }
  }, []);

  const copyPlannedFiles = useCallback(async () => {
    if (!planValidation || !planPreflight) return;

    // Blocked files are skipped; a problem with one file no longer stops
    // the others.
    const issues = [...planValidation.issues, ...planPreflight.issues];
    const fileMoves = plannedMoves.filter((move) => !move.sourceIsDirectory);
    const readyMoves = fileMoves.filter((move) => blockingIssues(move, issues).length === 0);
    const skipped = fileMoves.length - readyMoves.length;
    const skippedNote = skipped > 0
      ? ` ${skipped.toLocaleString()} blocked ${skipped === 1 ? "file was" : "files were"} skipped.`
      : "";

    if (readyMoves.length === 0 || executionRunning.current) {
      return;
    }

    executionRunning.current = true;
    cancelRequested.current = false;
    setCancellingCopy(false);
    setExecutingPlan(true);
    setExecutionResult(null);
    setShowCopyConfirmation(false);
    setExecutionProgress({ moves: readyMoves, completed: 0 });

    let completed = 0;
    const copiedSoFar = () => completed === 0
      ? "Nothing was copied."
      : `${completed.toLocaleString()} ${completed === 1 ? "file was" : "files were"} copied and verified.`;

    try {
      // The backend reruns final live validation immediately before every
      // individual copy. UI validation is informative, not the safety gate.
      for (const move of readyMoves) {
        if (cancelRequested.current) break;
        setFileProgress(null);
        await invoke<TransferRecord>("execute_planned_move", {
          plannedMoveId: move.id,
        });

        completed += 1;
        setExecutionProgress({ moves: readyMoves, completed });
      }

      setExecutionResult(cancelRequested.current && completed < readyMoves.length
        ? {
          stopped: true,
          message: `Copy cancelled. ${copiedSoFar()} The rest stay planned.${skippedNote}`,
        }
        : {
          stopped: false,
          message: `${completed.toLocaleString()} ${completed === 1 ? "file" : "files"} copied and verified. Originals were left untouched.${skippedNote}`,
        });
      await Promise.all([loadPlannedMoves(), loadTransfers()]);
    } catch (cause) {
      const cancelled = String(cause).includes("Copy cancelled.");
      setExecutionResult({
        stopped: true,
        message: cancelled
          ? `Copy cancelled. ${copiedSoFar()} The file being copied was not finished; it and the rest stay planned.${skippedNote}`
          : completed > 0
            ? `${completed.toLocaleString()} ${completed === 1 ? "file was" : "files were"} copied and verified before the transfer stopped. ${String(cause)}${skippedNote}`
            : `Nothing was copied. ${String(cause)}${skippedNote}`,
      });
      await Promise.all([loadPlannedMoves(), loadTransfers()]);
    } finally {
      executionRunning.current = false;
      cancelRequested.current = false;
      setCancellingCopy(false);
      setFileProgress(null);
      setExecutingPlan(false);
      setExecutionProgress(null);
    }
  }, [
    loadPlannedMoves,
    loadTransfers,
    planPreflight,
    planValidation,
    plannedMoves,
  ]);

  const cancelCopy = async () => {
    cancelRequested.current = true;
    setCancellingCopy(true);
    try {
      await invoke("cancel_transfer");
    } catch (cause) {
      setError(String(cause));
    }
  };

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
    void loadTransfers();
    void loadLocations();
  }, [refresh, loadPlannedMoves, loadTransfers, loadLocations]);

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

  // A refusal describes the destination it was given; once the user picks
  // another item, location or folder it no longer applies.
  useEffect(() => {
    setPlanError(null);
  }, [selectedPlanEntry, planDestinationLocationId, planDestinationFolder]);

  useEffect(() => {
    let dispose: (() => void) | undefined;
    void listen<ScanProgress>("scan-progress", (event) => setScanProgress(event.payload))
      .then((unlisten) => { dispose = unlisten; });
    return () => { dispose?.(); };
  }, []);

  useEffect(() => {
    let dispose: (() => void) | undefined;
    void listen<TransferProgress>("transfer-progress", (event) => setFileProgress(event.payload))
      .then((unlisten) => { dispose = unlisten; });
    return () => { dispose?.(); };
  }, []);

  // Recovery of interrupted transfers finishes after launch, once drives are
  // listed, and can change both history and the plan.
  useEffect(() => {
    let dispose: (() => void) | undefined;
    void listen("transfers-recovered", () => {
      void loadTransfers();
      void loadPlannedMoves();
    }).then((unlisten) => { dispose = unlisten; });
    return () => { dispose?.(); };
  }, [loadTransfers, loadPlannedMoves]);

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

  const sourceDriveName = (driveId: string) =>
    driveDisplayName(
      driveId,
      catalogued.find((drive) => drive.persistentIdentifier === driveId)?.name ??
        locationForDrive(driveId)?.displayName ??
        "Unknown drive",
    );

  const plannedMovePaths = (move: PlannedMove) => ({
    from: formatLocationPath(driveDisplayName(move.sourceDriveId, move.sourceDriveName), move.sourceRelativePath),
    to: formatLocationPath(move.destinationLocationName, move.destinationRelativePath),
  });

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
    setDropRefusal(null);
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
    // Do not preselect the source drive. Planning should begin with an
    // intentional destination choice rather than an immediate invalid state.
    setPlanDestinationLocationId("");
    setPlanDestinationFolder("");
    setFolderPickerOpen(false);
    setFolderPickerPath("");
    setFolderPickerEntries([]);
    setPlanError(null);
    setError(null);
  };

  const openDestinationFolderPicker = async (path = "") => {
    const location = locations.find((item) => item.id === planDestinationLocationId);
    if (!location || location.kind !== "external_drive" || !location.driveId) {
      setError("Choose a catalogued external drive before choosing a folder.");
      return;
    }

    setFolderPickerOpen(true);
    setFolderPickerPath(path);
    setFolderPickerEntries([]);
    setFolderPickerLoading(true);
    setError(null);

    try {
      const results = await invoke<CatalogueEntry[]>("list_catalogue_entries", {
        persistentIdentifier: location.driveId,
        parentPath: path,
      });
      setFolderPickerEntries(results.filter((entry) => entry.isDirectory));
    } catch (cause) {
      setError(String(cause));
      setFolderPickerEntries([]);
    } finally {
      setFolderPickerLoading(false);
    }
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
      setPlanError(String(cause));
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
      setDropRefusal({ folderPath: destinationFolder, itemName: entry.name, message: String(cause) });
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
      setPlanError(String(cause));
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
      const result = await invoke<ScanResult>("scan_drive", { persistentIdentifier: drive.persistentIdentifier });
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

      // Keep the summary on screen when some folders could not be read.
      if (result.unreadableFolderCount === 0) {
        window.setTimeout(() => setScanComplete(null), 4000);
      }
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
  let showLegacyDashboard: boolean = false;
  // Connected drives are listed first, then offline ones. Both follow the
  // catalogue's order: most recently scanned first, then by name. Connected
  // drives never scanned come last, still in name order.
  const catalogueOrder = new Map(catalogued.map((drive, index) => [drive.persistentIdentifier, index]));
  const connectedInOrder = [...connected].sort((first, second) =>
    (catalogueOrder.get(first.persistentIdentifier ?? "") ?? catalogued.length) -
    (catalogueOrder.get(second.persistentIdentifier ?? "") ?? catalogued.length));
  const offline = catalogued.filter((drive) => !connectedIds.has(drive.persistentIdentifier));
  const showTransfers =
    plannedMoves.length > 0 || transfers.length > 0 || executionProgress !== null || executionResult !== null;

  if (activeView === "transfers" && !browserDrive) {
    return (
      <main className="app-shell app-navigation-shell">
        <aside className="app-sidebar" aria-label="Media Mapper">
          <div className="sidebar-brand">MediaMapper</div>
          <nav className="sidebar-navigation" aria-label="Main navigation">
            <button className="sidebar-item" type="button" onClick={() => setActiveView("drives")}>Drives</button>
            <button className="sidebar-item" type="button" onClick={() => setActiveView("browse")}>Browse</button>
            <button className="sidebar-item" type="button" onClick={() => setActiveView("plan")}>Plan</button>
            <button className="sidebar-item active" type="button" aria-current="page">Transfers</button>
          </nav>
        </aside>
        <div className="app-content transfers-home">
          <header className="app-header">
            <div>
              <p className="eyebrow">TRANSFERS</p>
              <h1>Transfers</h1>
            </div>
          </header>

          {error && <div className="notice error">{error}</div>}

          {executionProgress && (() => {
            const { moves, completed } = executionProgress;
            const current = moves[Math.min(completed, moves.length - 1)];
            const paths = plannedMovePaths(current);
            return (
              <section className="transfer-active-card" aria-live="polite">
                <span className="transfer-active-label">COPYING</span>
                <h2>{current.sourceName}</h2>
                <TransferPaths from={paths.from} to={paths.to} />
                <div className="transfer-file-progress">
                  <span style={{ width: `${moves.length ? (completed / moves.length) * 100 : 0}%` }} />
                </div>
                <p>{Math.min(completed + 1, moves.length)} of {moves.length} files · Each file is verified before the next begins.</p>
              </section>
            );
          })()}

          {!executionProgress && executionResult && (
            <section className={`transfer-result-card ${executionResult.stopped ? "failed" : "completed"}`}>
              <div>
                <strong>{executionResult.stopped ? "Copy stopped" : "Copy finished"}</strong>
                <p>{executionResult.message}</p>
              </div>
              <button className="section-action" onClick={() => setExecutionResult(null)}>Dismiss</button>
            </section>
          )}

          {!executionProgress && !executionResult && plannedMoves.length > 0 && (
            <section className="transfer-waiting-card">
              <div>
                <span className="transfer-active-label">PLANNED</span>
                <h2>{plannedMoves.length} {plannedMoves.length === 1 ? "transfer" : "transfers"} waiting</h2>
                <p>Open Plan to see what is required before copying can begin.</p>
              </div>
              <button className="browse-button" onClick={() => setActiveView("plan")}>View plan</button>
            </section>
          )}

          <section className="transfer-history-section">
            <div className="section-heading">
              <h2>Recent transfers</h2>
            </div>
            {transfers.length === 0 ? (
              <div className="transfer-history-empty">
                <h3>No transfers yet</h3>
                <p>Copied and verified files will appear here.</p>
              </div>
            ) : (
              <>
                <div className="transfer-history-new">
                  {(showAllTransfers ? transfers : transfers.slice(0, RECENT_TRANSFER_COUNT)).map((transfer) => {
                    const outcome = transferOutcome(transfer.status);
                    const size = transfer.status === "completed" ? transfer.copiedBytes : transfer.totalBytes;
                    const location = locations.find((item) => item.id === transfer.destinationLocationId);
                    return (
                      <div className={`transfer-history-new-row ${outcome.tone}`} key={transfer.id}>
                        <span className="transfer-history-mark" aria-hidden="true">{outcome.tone === "completed" ? "✓" : outcome.tone === "failed" ? "×" : "–"}</span>
                        <div className="transfer-item">
                          <strong>{fileName(transfer.sourceRelativePath)}</strong>
                          <TransferPaths
                            from={formatLocationPath(sourceDriveName(transfer.sourceDriveId), transfer.sourceRelativePath)}
                            to={formatLocationPath(location ? location.userLabel ?? location.displayName : "Unknown location", transfer.destinationRelativePath)}
                          />
                          {transfer.status === "failed" && transfer.errorMessage && <p className="transfer-error">{transfer.errorMessage}</p>}
                        </div>
                        <span className="transfer-history-size">{size === null ? "" : formatBytes(size)}</span>
                        <div className="transfer-outcome">
                          <span className="transfer-status">{outcome.label}</span>
                          <span>{formatDate(transfer.completedAt ?? transfer.startedAt ?? transfer.createdAt)}</span>
                        </div>
                      </div>
                    );
                  })}
                </div>
                {transfers.length > RECENT_TRANSFER_COUNT && (
                  <button className="transfer-more-button" onClick={() => setShowAllTransfers((value) => !value)}>
                    {showAllTransfers ? "Show recent only" : `Show all ${transfers.length.toLocaleString()}`}
                  </button>
                )}
              </>
            )}
          </section>
        </div>
      </main>
    );
  }

    if (activeView === "plan" && !browserDrive) {
    const fileMoves = plannedMoves.filter((move) => !move.sourceIsDirectory);
    const folderMoveCount = plannedMoves.length - fileMoves.length;
    const knownBytes = planPreflight?.knownBytes ?? fileMoves.reduce((sum, move) => sum + (move.sourceSizeBytes ?? 0), 0);
    const sourceIds = Array.from(new Set(plannedMoves.map((move) => move.sourceDriveId)));
    const destinationIds = Array.from(new Set(plannedMoves.map((move) => move.destinationLocationId)));
    const missingNames = Array.from(new Set([
      ...sourceIds
        .filter((id) => !connectedIds.has(id))
        .map((id) => sourceDriveName(id)),
      ...destinationIds.flatMap((id) => {
        const location = locations.find((item) => item.id === id);
        if (!location || location.kind === "local_folder") return [];
        return location.driveId && !connectedIds.has(location.driveId)
          ? [location.userLabel ?? location.displayName]
          : [];
      }),
    ]));
    const planReady = plannedMoves.length > 0 && planValidation?.ready === true && (planPreflight?.issues.length ?? 0) === 0;
    const canCopy = planReady && fileMoves.length > 0 && !executingPlan;

    return (
      <main className="app-shell app-navigation-shell">
        <aside className="app-sidebar" aria-label="Media Mapper">
          <div className="sidebar-brand">MediaMapper</div>
          <nav className="sidebar-navigation" aria-label="Main navigation">
            <button className="sidebar-item" type="button" onClick={() => setActiveView("drives")}>Drives</button>
            <button className="sidebar-item" type="button" onClick={() => setActiveView("browse")}>Browse</button>
            <button className="sidebar-item active" type="button" aria-current="page">Plan</button>
            <button className="sidebar-item" type="button" onClick={() => { setBrowserDrive(null); setActiveView("transfers"); }}>Transfers</button>
          </nav>
        </aside>
        <div className="app-content plan-home">
          <header className="app-header">
            <div>
              <p className="eyebrow">PLAN</p>
              <h1>Planned transfers</h1>
              <p className="intro">Organise your files. Decide what goes where. Copy only when you’re ready.</p>
            </div>
          </header>

          {error && <div className="notice error">{error}</div>}

          {plannedMoves.length === 0 ? (
            <section className="plan-empty">
              <h2>Nothing planned yet</h2>
              <p>Browse your catalogued files and choose Plan move to add files here.</p>
              <button className="browse-button" type="button" onClick={() => setActiveView("browse")}>Browse files</button>
            </section>
          ) : (
            <>
              <section className="plan-overview">
                <div>
                  <span>Planned</span>
                  <strong>{plannedMoves.length.toLocaleString()} {plannedMoves.length === 1 ? "item" : "items"} · {formatBytes(knownBytes)}</strong>
                </div>
                <div>
                  <span>Status</span>
                  <strong className={planReady ? "plan-ready-text" : "plan-waiting-text"}>
                    {planReady ? "Ready to transfer" : missingNames.length > 0 ? `Waiting for ${missingNames.join(" and ")}` : "Needs attention"}
                  </strong>
                </div>
              </section>

              {missingNames.length > 0 && (
                <section className="connect-instruction">
                  <span className="connect-instruction-label">NEXT STEP</span>
                  <h2>Connect {missingNames.join(" and ")}</h2>
                  <p>{missingNames.length === 1 ? "This location is" : "These locations are"} required before the planned transfer can run. No files will be staged on this Mac.</p>
                </section>
              )}

              {(planPreflight?.issues.length ?? 0) > 0 && missingNames.length === 0 && (
                <section className="plan-attention">
                  <strong>Plan needs attention</strong>
                  <ul>{planPreflight!.issues.map((issue, index) => <li key={`${issue.code}:${index}`}>{issue.message}</li>)}</ul>
                </section>
              )}

              <section className="plan-file-list">
                {plannedMoves.map((move) => {
                  const paths = plannedMovePaths(move);
                  return (
                    <div className="plan-file-row" key={move.id}>
                      <div className="transfer-item">
                        <strong>{move.sourceName}</strong>
                        <span className="transfer-path" title={paths.from}>{paths.from}</span>
                        <span className="transfer-path" title={paths.to}><span className="plan-destination-arrow">→</span>{paths.to}</span>
                      </div>
                      <div className="plan-file-side">
                        <span>{formatBytes(move.sourceSizeBytes)}</span>
                        <button className="plan-remove-button" disabled={executingPlan} onClick={() => void removePlannedMove(move.id)}>Remove</button>
                      </div>
                    </div>
                  );
                })}
              </section>

              <section className="plan-review">
                <div>
                  <strong>{fileMoves.length.toLocaleString()} {fileMoves.length === 1 ? "file" : "files"} · {formatBytes(knownBytes)} will be copied</strong>
                  <span>Originals remain untouched.</span>
                  {folderMoveCount > 0 && <span>{folderMoveCount} planned {folderMoveCount === 1 ? "folder is" : "folders are"} not executable yet.</span>}
                </div>
                {!showCopyConfirmation ? (
                  <button className="section-action" disabled={!canCopy} onClick={() => { setExecutionResult(null); setShowCopyConfirmation(true); }}>Continue</button>
                ) : (
                  <div className="plan-copy-actions">
                    <button className="secondary-button" disabled={executingPlan} onClick={() => setShowCopyConfirmation(false)}>Cancel</button>
                    <button className="section-action" disabled={!canCopy} onClick={() => void copyPlannedFiles()}>
                      {executingPlan ? "Copying…" : "Copy and verify"}
                    </button>
                  </div>
                )}
              </section>

              {showCopyConfirmation && (
                <p className="plan-confirmation-note">Final checks run again immediately before each copy. MediaMapper copies directly between locations and verifies the result byte for byte.</p>
              )}
            </>
          )}
        </div>
      </main>
    );
  }

    if (activeView === "browse" && !browserDrive) {
    return (
      <main className="app-shell app-navigation-shell">
        <aside className="app-sidebar" aria-label="Media Mapper">
          <div className="sidebar-brand">MediaMapper</div>
          <nav className="sidebar-navigation" aria-label="Main navigation">
            <button className="sidebar-item" type="button" onClick={() => setActiveView("drives")}>Drives</button>
            <button className="sidebar-item active" type="button" aria-current="page">Browse</button>
            <button className="sidebar-item" type="button" onClick={() => { setBrowserDrive(null); setActiveView("plan"); }}>Plan</button>
            <button className="sidebar-item" type="button" onClick={() => { setBrowserDrive(null); setActiveView("transfers"); }}>Transfers</button>
          </nav>
        </aside>
        <div className="app-content browse-home">
          <header className="app-header">
            <div>
              <p className="eyebrow">BROWSE</p>
              <h1>All files</h1>
            </div>
          </header>

          {error && <div className="notice error">{error}</div>}

          <section className="browse-search-block">
            <input
              className="browse-global-search"
              type="search"
              value={libraryQuery}
              placeholder="Search files"
              aria-label="Search all catalogued files"
              autoFocus
              onChange={(event) => void searchLibrary(event.target.value)}
            />
            <label className="hidden-items-toggle">
              <input type="checkbox" checked={showHiddenItems} onChange={(event) => setShowHiddenItems(event.target.checked)} />
              Show hidden items
            </label>
          </section>

          {libraryQuery.trim() ? (
            <section className="browse-results" aria-live="polite">
              <div className="browse-results-heading">
                <span>{librarySearching ? "Searching…" : `${visibleLibraryResults.length}${libraryResults.length === 200 ? "+" : ""} results`}</span>
              </div>
              {librarySearching ? <div className="browser-message">Searching catalogues…</div>
              : visibleLibraryResults.length === 0 ? <div className="browser-message">No matching files or folders.</div>
              : visibleLibraryResults.map((result) => (
                <button className="browse-result-row" key={`${result.driveId}:${result.relativePath}`} onClick={() => void openLibraryResult(result)}>
                  <span className="browse-result-name">
                    <strong>{result.name}</strong>
                    <span>{formatLocationPath(driveDisplayName(result.driveId, result.driveName), parentFolder(result.relativePath))}</span>
                  </span>
                  <span className={connectedIds.has(result.driveId) ? "browse-drive-state connected" : "browse-drive-state"}>
                    {connectedIds.has(result.driveId) ? "Connected" : "Offline"}
                  </span>
                  <span>{result.isDirectory ? "Folder" : formatBytes(result.sizeBytes)}</span>
                </button>
              ))}
            </section>
          ) : (
            <section className="browse-drives">
              <div className="section-heading"><h2>Browse by drive</h2></div>
              <div className="browse-drive-grid">
                {catalogued.map((drive) => (
                  <button className="browse-drive-card" key={drive.persistentIdentifier} onClick={() => { setActiveView("browse"); void openFolder(drive, ""); }}>
                    <span className="browse-drive-card-top">
                      <strong>{driveDisplayName(drive.persistentIdentifier, drive.name)}</strong>
                    </span>
                    <span>{drive.fileCount.toLocaleString()} files · {formatBytes(drive.cataloguedBytes)} · {connectedIds.has(drive.persistentIdentifier) ? "Connected" : "Offline"}</span>
                  </button>
                ))}
              </div>
              {catalogued.length === 0 && <div className="empty-state compact"><h3>No catalogued drives</h3><p>Scan a drive first, then its files will appear here.</p></div>}
            </section>
          )}
        </div>
      </main>
    );
  }

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
      <main className="app-navigation-shell">
        <aside className="app-sidebar">
          <div className="sidebar-brand">MediaMapper</div>
          <nav className="sidebar-navigation" aria-label="Main navigation">
            <button className="sidebar-item" type="button" onClick={() => { setBrowserDrive(null); setBrowserPath(""); setEntries([]); setPlannedFolderEntries([]); setSearchQuery(""); setSearchResults([]); setActiveView("drives"); }}>Drives</button>
            <button className="sidebar-item active" type="button" aria-current="page" onClick={() => { setBrowserDrive(null); setBrowserPath(""); setEntries([]); setPlannedFolderEntries([]); setSearchQuery(""); setSearchResults([]); setActiveView("browse"); }}>Browse</button>
            <button className="sidebar-item" type="button" onClick={() => { setBrowserDrive(null); setActiveView("plan"); }}>Plan</button>
            <button className="sidebar-item" type="button" onClick={() => { setBrowserDrive(null); setActiveView("transfers"); }}>Transfers</button>
          </nav>
        </aside>
        <div className="app-content browse-detail">
          <header className="app-header browse-detail-header">
            <div>
              <p className="eyebrow">BROWSE</p>
              <nav className="browse-detail-breadcrumb" aria-label="Browse path">
                <button type="button" onClick={() => { setBrowserDrive(null); setBrowserPath(""); setEntries([]); setPlannedFolderEntries([]); setSearchQuery(""); setSearchResults([]); }}>All files</button>
                <span>/</span>
                <button type="button" className={pathParts.length === 0 ? "current" : undefined} onClick={() => void openFolder(liveDrive, "")}>{liveDriveName}</button>
                {pathParts.map((part, index) => {
                  const path = pathParts.slice(0, index + 1).join("/");
                  const current = index === pathParts.length - 1;
                  return (
                    <span className="browse-detail-crumb" key={path}>
                      <span>/</span>
                      {current
                        ? <span className="current">{part}</span>
                        : <button type="button" onClick={() => void openFolder(liveDrive, path)}>{part}</button>}
                    </span>
                  );
                })}
              </nav>
              <h1>{pathParts.length > 0 ? pathParts[pathParts.length - 1] : liveDriveName}</h1>
              {pathParts.length === 0 && liveDriveHasLabel && <div className="drive-volume-name">{liveDrive.name}</div>}
            </div>
          </header>

          <section className="browse-search-block browse-detail-search">
            <input
              className="browse-global-search"
              type="search"
              value={searchQuery}
              placeholder={`Search ${liveDriveName}`}
              aria-label={`Search ${liveDriveName}`}
              onChange={(event) => void searchCatalogue(liveDrive, event.target.value)}
            />
            <label className="hidden-items-toggle">
              <input type="checkbox" checked={showHiddenItems} onChange={(event) => setShowHiddenItems(event.target.checked)} />
              Show hidden items
            </label>
          </section>
          {searchQuery.trim() && (
            <div className="browse-results-heading">
              {searching ? "Searching…" : `${visibleSearchResults.length}${searchResults.length === 200 ? "+" : ""} results`}
            </div>
          )}

        {error && <div className="notice error">{error}</div>}
        {searchQuery.trim() && <section className="file-browser" aria-live="polite">
          <div className="file-browser-head">
            <span>Name</span><span>Modified</span><span>Size</span><span>Action</span>
          </div>
          {searching ? <div className="browser-message">Searching catalogue…</div>
          : visibleSearchResults.length === 0 ? <div className="browser-message">No matching files or folders.</div>
          : visibleSearchResults.map((entry) => {
              const plannedMove = plannedMoveBySourcePath.get(entry.relativePath);
              return (
                <button
                  className={`file-row search-row ${plannedMove ? "moving-out-row" : ""}`}
                  key={entry.relativePath}
                  type="button"
                  onClick={() => void openSearchResult(liveDrive, entry)}
                >
                  <span className="file-name">

                    <span className="file-name-text">
                      <span className="file-name-primary">{entry.name}</span>
                      <span className="file-location">{formatLocationPath(liveDriveName, parentFolder(entry.relativePath))}</span>
                      {entry.unreadable && (
                        <span className="unreadable-note">Couldn't be read during the last scan</span>
                      )}
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
                  <span>{entry.isDirectory ? "Folder" : formatBytes(entry.sizeBytes)}</span>
                  <span />
                </button>
              );
            })}
        </section>}

        {!searchQuery.trim() && <section className="file-browser">
          <div className="file-browser-head">
            <span>Name</span><span>Modified</span><span>Size</span><span />
          </div>
          {browserLoading ? (
            <div className="browser-message">Loading catalogue…</div>
          ) : visibleEntries.length === 0 && visiblePlannedFolderEntries.length === 0 ? (
            <div className="browser-message">This folder is empty in the catalogue and plan.</div>
          ) : (
            <>
              {visibleEntries.map((entry) => {
                const plannedMove = plannedMoveBySourcePath.get(entry.relativePath);

                const openRow = () => {
                  if (entry.isDirectory) {
                    void openFolder(liveDrive, entry.relativePath);
                  }
                };

                const openFile = () => {
                  if (!entry.isDirectory) {
                    if (!online) {
                      setError(`Connect ${liveDriveName} to open this file.`);
                      return;
                    }
                    void invoke("open_catalogued_file", {
                      driveId: liveDrive.persistentIdentifier,
                      relativePath: entry.relativePath,
                    }).catch((reason) => setError(String(reason)));
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
                      setDropRefusal(null);
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
                    onClick={openRow}
                    onDoubleClick={openFile}
                    onKeyDown={(event) => {
                      if (event.key === "Enter" && entry.isDirectory) {
                        event.preventDefault();
                        openRow();
                      }
                    }}
                  >
                    <span className="file-name">

                      <span className="file-name-text">
                        <span className="file-name-primary">{entry.name}</span>
                        {entry.unreadable && (
                          <span className="unreadable-note">Couldn't be read during the last scan</span>
                        )}
                        {dropRefusal?.folderPath === entry.relativePath && (
                          <span className="drop-refusal" role="alert">
                            Couldn't plan {dropRefusal.itemName} here. {dropRefusal.message}
                          </span>
                        )}
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
                    <span className={dragOverFolderPath === entry.relativePath ? "drop-hint" : undefined}>
                      {entry.isDirectory
                        ? (dragOverFolderPath === entry.relativePath ? "Drop to move here" : "Folder")
                        : formatBytes(entry.sizeBytes)}
                    </span>
                    <span className="row-action">
                      {!entry.isDirectory && !plannedMove && (
                        <button
                          type="button"
                          onClick={(event) => {
                            event.stopPropagation();
                            beginPlanMove(entry);
                          }}
                        >
                          Add to plan
                        </button>
                      )}
                    </span>
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

                      <span className="file-name-text">
                        <span className="file-name-primary">
                          <span className="planned-tag">Planned</span>{planned.name}
                        </span>
                        {planned.isNewFolder
                          ? <span className="planned-path">New folder in the plan</span>
                          : plannedFromPath(planned)}
                      </span>
                    </span>
                    <span aria-label="Not on the drive yet">—</span>
                    <span>{planned.isDirectory ? "Folder" : formatBytes(planned.sizeBytes)}</span>
                    <span />
                  </button>
                ))}
            </>
          )}
        </section>}

        {selectedPlanEntry && <section className="plan-move-panel compact-plan-panel" ref={planPanel}>
          <div className="plan-move-heading">
            <div>
              <h2>Add to plan</h2>
              <p className="plan-file-summary" title={selectedPlanEntry.name}>
                {selectedPlanEntry.name}
              </p>
            </div>
            <button className="plan-cancel-button" onClick={() => setSelectedPlanEntry(null)}>Cancel</button>
          </div>

          <div className="plan-fields compact-plan-fields">
            <label>
              <span>Location</span>
              <select
                ref={planLocationSelect}
                value={planDestinationLocationId}
                onChange={(event) => {
                  if (event.target.value === "__choose_mac__") {
                    void addFolderOnThisMac();
                    return;
                  }
                  setPlanDestinationLocationId(event.target.value);
                  setPlanDestinationFolder("");
                  setFolderPickerOpen(false);
                  setPlanError(null);
                }}
              >
                <option value="">Choose a location</option>
                <option value="__choose_mac__">Choose a folder on this Mac…</option>

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
            </label>

            <label>
              <span>Folder</span>
              {(() => {
                const destination = locations.find(
                  (location) => location.id === planDestinationLocationId,
                );
                const canBrowseCatalogue =
                  destination?.kind === "external_drive" && Boolean(destination.driveId);

                return canBrowseCatalogue ? (
                  <button
                    type="button"
                    className="folder-picker-button"
                    aria-expanded={folderPickerOpen}
                    onClick={(event) => {
                      event.preventDefault();
                      event.stopPropagation();
                      void openDestinationFolderPicker(planDestinationFolder);
                    }}
                  >
                    <span className="folder-picker-path" title={planDestinationFolder || "Top level"}>
                      {planDestinationFolder || "Top level"}
                    </span>
                    <span className="folder-picker-choose">Choose…</span>
                  </button>
                ) : (
                  <div className="folder-picker-readonly">
                    {destination?.kind === "local_folder"
                      ? "Selected folder"
                      : "Choose a location first"}
                  </div>
                );
              })()}
            </label>
          </div>

          {folderPickerOpen && (() => {
            const parts = folderPickerPath ? folderPickerPath.split("/") : [];

            return (
              <div className="folder-picker" role="dialog" aria-label="Choose destination folder">
                <div className="folder-picker-header">
                  <strong>Choose folder</strong>
                  <button type="button" onClick={() => setFolderPickerOpen(false)}>Cancel</button>
                </div>
                <nav className="folder-picker-breadcrumbs" aria-label="Destination folder path">
                  <button type="button" onClick={() => void openDestinationFolderPicker("")}>
                    Top level
                  </button>
                  {parts.map((part, index) => {
                    const path = parts.slice(0, index + 1).join("/");
                    return (
                      <span key={path}>
                        <span>/</span>
                        <button type="button" onClick={() => void openDestinationFolderPicker(path)}>
                          {part}
                        </button>
                      </span>
                    );
                  })}
                </nav>
                <div className="folder-picker-list">
                  {folderPickerLoading ? (
                    <div className="folder-picker-empty">Loading folders…</div>
                  ) : folderPickerEntries.length === 0 ? (
                    <div className="folder-picker-empty">No folders inside this folder.</div>
                  ) : folderPickerEntries.map((entry) => (
                    <button
                      type="button"
                      key={entry.relativePath}
                      onClick={() => void openDestinationFolderPicker(entry.relativePath)}
                    >
                      <span>{entry.name}</span><span>›</span>
                    </button>
                  ))}
                </div>
                <div className="folder-picker-footer">
                  <button
                    type="button"
                    className="browse-button"
                    onClick={() => {
                      setPlanDestinationFolder(folderPickerPath);
                      setFolderPickerOpen(false);
                    }}
                  >
                    Choose this folder
                  </button>
                </div>
              </div>
            );
          })()}

          {planDestinationLocationId && (
            <div className="plan-destination-preview">
              <span>Will be copied to</span>
              <strong title={(() => {
                const destination = locations.find(
                  (location) => location.id === planDestinationLocationId,
                );
                const destinationName =
                  destination?.userLabel ?? destination?.displayName ?? "Destination";
                return formatLocationPath(destinationName, normalisedPlanFolder);
              })()}>
                {(() => {
                  const destination = locations.find(
                    (location) => location.id === planDestinationLocationId,
                  );
                  const destinationName =
                    destination?.userLabel ?? destination?.displayName ?? "Destination";
                  return formatLocationPath(destinationName, normalisedPlanFolder);
                })()}
              </strong>
            </div>
          )}

          <div className="plan-actions compact-plan-actions">
            {planError ? (
              <span className="plan-location-warning" role="alert">{planError}</span>
            ) : planIsCurrentLocation ? (
              <span className="plan-location-warning">Choose a different destination.</span>
            ) : (
              <span>Nothing is copied until you run the plan.</span>
            )}
            <button
              className="browse-button"
              disabled={!planDestinationLocationId || savingPlan || planIsCurrentLocation}
              onClick={() => void savePlannedMove()}
            >
              {savingPlan ? "Saving…" : "Add to plan"}
            </button>
          </div>
        </section>}

        </div>
      </main>
    );
  }

  return (
    <main className="app-shell app-navigation-shell">
      <aside className="app-sidebar" aria-label="Media Mapper">
        <div className="sidebar-brand">MediaMapper</div>
        <nav className="sidebar-navigation" aria-label="Main navigation">
          <button className="sidebar-item active" type="button" aria-current="page">Drives</button>
          <button className="sidebar-item" type="button" onClick={() => { setBrowserDrive(null); setActiveView("browse"); }}>Browse</button>
          <button className="sidebar-item" type="button" onClick={() => { setBrowserDrive(null); setActiveView("plan"); }}>Plan</button>
          <button className="sidebar-item" type="button" onClick={() => { setBrowserDrive(null); setActiveView("transfers"); }}>Transfers</button>
        </nav>
      </aside>
      <div className="app-content">
      <header className="app-header">
        <div>
          <p className="eyebrow">DRIVES</p>
          <h1>Your storage</h1>
        </div>
        <button className="refresh-button" onClick={() => void refresh()} disabled={loading || scanningId !== null}>
          {loading ? "Checking…" : "Refresh"}
        </button>
      </header>

      {error && <div className="notice error">{error}</div>}

      {showLegacyDashboard && catalogued.length > 0 && <section className="section-block">
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
                <span>{formatLocationPath(driveDisplayName(result.driveId, result.driveName), parentFolder(result.relativePath))}</span>
              </span>
              <span>{connectedIds.has(result.driveId) ? "Connected" : "Offline"}</span>
              <span>{result.isDirectory ? "Folder" : formatBytes(result.sizeBytes)}</span>
            </button>)}
        </div>}
      </section>}

      <section className="section-block">
        <div className="section-heading"><h2>Connected</h2></div>
        {!loading && connected.length === 0 && (
          <div className="empty-state compact"><h3>No external drives detected</h3><p>Connect a drive and it will appear here.</p></div>
        )}
        <div className="drive-list">
          {connectedInOrder.map((drive) => {
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
                                {label ? "Edit name" : "Add label"}
                              </button>
                            )
                          )}
                        </>
                      );
                    })()}
                  </div>
                  <div className="drive-actions">
                    <div className="button-row">
                      {catalogue && <button className="browse-button" onClick={() => { setActiveView("browse"); void openFolder(catalogue, ""); }}>Browse</button>}
                      <button className="scan-button" disabled={scanningId !== null || executingPlan || !drive.persistentIdentifier} onClick={() => void scan(drive)}>
                        {scanning ? "Scanning…" : catalogue ? "Rescan" : "Scan"}
                      </button>
                    </div>
                  </div>
                </div>
                <CapacitySummary totalBytes={drive.totalBytes} availableBytes={drive.availableBytes} />
                <div className="drive-meta">
                  <span>
                    {catalogue
                      ? `${catalogue.fileCount.toLocaleString()} files · ${catalogue.directoryCount.toLocaleString()} folders · ${formatBytes(catalogue.cataloguedBytes)} catalogued`
                      : "Not catalogued yet"}
                  </span>
                  <span title={catalogue?.lastScannedAt ? formatDate(catalogue.lastScannedAt) : undefined}>{formatScanAge(catalogue?.lastScannedAt ?? null)}</span>
                  {catalogue && catalogue.unreadableFolderCount > 0 && (
                    <span className="unreadable-note">Couldn't read {catalogue.unreadableFolderCount.toLocaleString()} {catalogue.unreadableFolderCount === 1 ? "folder" : "folders"}</span>
                  )}
                </div>
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
                    <div className="scan-complete">
                      Scan complete · {scanComplete.fileCount.toLocaleString()} files · {scanComplete.directoryCount.toLocaleString()} folders · {formatBytes(scanComplete.cataloguedBytes)}{scanComplete.skippedCount > 0 ? ` · ${scanComplete.skippedCount.toLocaleString()} skipped` : ""}
                      {scanComplete.unreadableFolderCount > 0 && (
                        <div className="unreadable-note">{unreadableSummary(scanComplete)}</div>
                      )}
                    </div>
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
        <div className="section-heading"><h2>Offline</h2>{offline.length > 0 && <span className="count-badge count-badge-muted">{offline.length}</span>}</div>
        {offline.length === 0 ? (
          <p className="section-empty">No catalogued drives are offline.</p>
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
                              {label ? "Edit name" : "Add label"}
                            </button>
                          )}
                        </>
                      );
                    })()}
                  </div>
                  <div className="drive-actions">
                    <div className="button-row">
                      <button className="browse-button" onClick={() => { setActiveView("browse"); void openFolder(drive, ""); }}>Browse</button>
                    </div>
                  </div>
                </div>
                <CapacitySummary totalBytes={drive.totalBytes} availableBytes={drive.availableBytes} atLastScan />
                <div className="drive-meta">
                  <span>{drive.fileCount.toLocaleString()} files · {drive.directoryCount.toLocaleString()} folders · {formatBytes(drive.cataloguedBytes)} catalogued</span>
                  <span title={drive.lastScannedAt ? formatDate(drive.lastScannedAt) : undefined}>{formatScanAge(drive.lastScannedAt)}</span>
                  {drive.unreadableFolderCount > 0 && (
                    <span className="unreadable-note">Couldn't read {drive.unreadableFolderCount.toLocaleString()} {drive.unreadableFolderCount === 1 ? "folder" : "folders"}</span>
                  )}
                </div>
              </article>
            ))}
          </div>
        )}
      </section>

      {showLegacyDashboard && executionProgress && (() => {
        const { moves, completed } = executionProgress;
        const current = moves[Math.min(completed, moves.length - 1)];
        const paths = plannedMovePaths(current);

        return (
          <section className="section-block" aria-live="polite">
            <div className="section-heading"><h2>In progress</h2></div>
            <div className="transfer-progress">
              <strong>
                Copying {Math.min(completed + 1, moves.length).toLocaleString()} of{" "}
                {moves.length.toLocaleString()} {moves.length === 1 ? "file" : "files"}
              </strong>
              <div className="transfer-item">
                <strong>{current.sourceName}</strong>
                <TransferPaths from={paths.from} to={paths.to} />
              </div>
              {fileProgress?.plannedMoveId === current.id && (() => {
                const percent = fileProgress.totalBytes > 0
                  ? Math.min(100, Math.floor((fileProgress.bytes / fileProgress.totalBytes) * 100))
                  : 100;
                return (
                  <div className="transfer-file-progress">
                    <span>
                      {fileProgress.stage === "copying" ? "Copying" : "Verifying"}{" "}
                      {formatBytes(fileProgress.bytes)} of {formatBytes(fileProgress.totalBytes)} · {percent}%
                    </span>
                    <div
                      className="capacity-bar"
                      role="progressbar"
                      aria-label={fileProgress.stage === "copying" ? "Copying" : "Verifying"}
                      aria-valuemin={0}
                      aria-valuemax={100}
                      aria-valuenow={percent}
                    >
                      <span style={{ width: `${percent}%` }} />
                    </div>
                  </div>
                );
              })()}
              <p>Each file is copied and verified before the next one starts. Originals stay in place.</p>
              <div>
                <button className="secondary-button" disabled={cancellingCopy} onClick={() => void cancelCopy()}>
                  {cancellingCopy ? "Cancelling…" : "Cancel copy"}
                </button>
              </div>
            </div>
          </section>
        );
      })()}

      {showLegacyDashboard && !executionProgress && executionResult && <section className="section-block">
        <div className="section-heading">
          <h2>{executionResult.stopped ? "Copy stopped" : "Copy finished"}</h2>
          <button className="section-action" onClick={() => setExecutionResult(null)}>Dismiss</button>
        </div>
        <p className={`transfer-result${executionResult.stopped ? " failed" : ""}`} role="status">
          {executionResult.message}
        </p>
      </section>}

      {showLegacyDashboard && showTransfers && <section className="section-block">
        <div className="section-heading">
          <h2>Waiting to transfer</h2>
          <span>{plannedMoves.length}</span>
        </div>
        {plannedMoves.length === 0 ? (
          <p className="section-empty">Nothing is waiting. Plan a move from a drive's catalogue to add it here.</p>
        ) : <>
          <p className="section-description">Planned only. Nothing is copied until you choose Copy and verify.</p>

          {planPreflight && <div className="plan-summary" aria-live="polite">
            {planValidation && (
              <div className={`plan-validation ${planValidation.ready ? "ready" : "blocked"}`}>
                <strong>{planValidation.ready ? "Current checks passed" : "Plan needs attention"}</strong>
                <span>
                  {planValidation.ready
                    ? "Connected sources and destinations have passed the current checks."
                    : `${planValidation.issues.length.toLocaleString()} ${planValidation.issues.length === 1 ? "live issue needs" : "live issues need"} attention.`}
                </span>
                {!planValidation.ready && (
                  <ul>
                    {planValidation.issues.map((issue, index) => (
                      <li key={`live:${issue.code}:${issue.moveId ?? "plan"}:${index}`}>
                        {issue.message}
                      </li>
                    ))}
                  </ul>
                )}
              </div>
            )}

            <div className="plan-summary-total">
              <strong>
                {planPreflight.moveCount.toLocaleString()} {planPreflight.moveCount === 1 ? "move" : "moves"}
                {" · "}
                {planPreflight.unknownSizeCount > 0 ? "at least " : ""}
                {formatBytes(planPreflight.knownBytes)}
              </strong>
              {planPreflight.unknownSizeCount > 0 && (
                <span>
                  {planPreflight.unknownSizeCount.toLocaleString()} {planPreflight.unknownSizeCount === 1 ? "file has" : "files have"} unknown size
                </span>
              )}
            </div>

            <div className="plan-destinations">
              {planPreflight.destinations.map((destination) => (
                <div className="plan-destination" key={destination.locationId}>
                  <strong>{destination.displayName}</strong>
                  <span>
                    {destination.unknownSizeCount > 0 ? "At least " : ""}
                    {formatBytes(destination.knownBytes)} planned
                    {destination.projectedAvailableBytes !== null
                      ? ` · ${formatBytes(destination.projectedAvailableBytes)} free after`
                      : " · capacity unknown"}
                  </span>
                  {destination.capacitySufficient === false && (
                    <span className="plan-summary-warning">Not enough catalogued free space</span>
                  )}
                  {destination.capacitySufficient === null && destination.availableBytes !== null && (
                    <span className="plan-summary-note">Final capacity cannot be confirmed while some file sizes are unknown.</span>
                  )}
                </div>
              ))}
            </div>

            {planPreflight.issues.length > 0 && (
              <div className="plan-issues">
                <strong>
                  {planPreflight.issues.length.toLocaleString()} {planPreflight.issues.length === 1 ? "issue needs" : "issues need"} attention
                </strong>
                <ul>
                  {planPreflight.issues.map((issue, index) => (
                    <li key={`${issue.code}:${issue.moveId ?? "plan"}:${index}`}>{issue.message}</li>
                  ))}
                </ul>
              </div>
            )}
          </div>}

          {(() => {
            const fileMoves = plannedMoves.filter((move) => !move.sourceIsDirectory);
            const folderMoveCount = plannedMoves.length - fileMoves.length;
            const checksLoaded = planValidation !== null && planPreflight !== null;
            const issues = [...(planValidation?.issues ?? []), ...(planPreflight?.issues ?? [])];
            const readyCount = fileMoves.filter((move) => blockingIssues(move, issues).length === 0).length;
            const blockedCount = fileMoves.length - readyCount;
            // Copying and scanning never overlap: a scan holds the catalogue's
            // write lock, so a copy could not record its own completion.
            const canCopy = checksLoaded && readyCount > 0 && !executingPlan && scanningId === null;

            return (
              <div className="plan-copy">
                {!showCopyConfirmation ? (
                  <div className="plan-copy-row">
                    <div>
                      <strong>Copy planned files</strong>
                      <span>
                        Copies are verified before completion. Originals stay in place.
                      </span>
                      {folderMoveCount > 0 && (
                        <span>
                          {folderMoveCount.toLocaleString()} planned {folderMoveCount === 1 ? "folder is" : "folders are"} not executable yet.
                        </span>
                      )}
                      {checksLoaded && blockedCount > 0 && (
                        <span>
                          {blockedCount.toLocaleString()} planned {blockedCount === 1 ? "file needs" : "files need"} attention and will be skipped.
                        </span>
                      )}
                      {scanningId !== null && (
                        <span>Copying is available when the current scan finishes.</span>
                      )}
                    </div>
                    <button
                      className="section-action"
                      disabled={!canCopy}
                      onClick={() => {
                        setExecutionResult(null);
                        setShowCopyConfirmation(true);
                      }}
                    >
                      Review copy
                    </button>
                  </div>
                ) : (
                  <div className="plan-copy-confirmation">
                    <div>
                      <strong>Copy {readyCount.toLocaleString()} {readyCount === 1 ? "file" : "files"}?</strong>
                      <span>
                        Media Mapper will run final checks again, copy each file with its dates and tags, verify it byte for byte, and leave every original untouched.
                      </span>
                      {blockedCount > 0 && (
                        <span>
                          {blockedCount.toLocaleString()} blocked {blockedCount === 1 ? "file" : "files"} will be skipped.
                        </span>
                      )}
                    </div>
                    <div className="plan-copy-actions">
                      <button
                        className="secondary-button"
                        disabled={executingPlan}
                        onClick={() => setShowCopyConfirmation(false)}
                      >
                        Cancel
                      </button>
                      <button
                        className="section-action"
                        disabled={!canCopy}
                        onClick={() => void copyPlannedFiles()}
                      >
                        {executingPlan ? "Copying…" : "Copy and verify"}
                      </button>
                    </div>
                  </div>
                )}

              </div>
            );
          })()}

          <div className="planned-move-list">
            {plannedMoves.map((move) => {
              const paths = plannedMovePaths(move);
              const runIndex = executionProgress?.moves.findIndex((item) => item.id === move.id) ?? -1;
              const runState =
                !executionProgress || runIndex === -1 ? ""
                  : runIndex < executionProgress.completed ? "Copied"
                    : runIndex === executionProgress.completed ? "Copying…"
                      : "Waiting";

              return (
                <div className="planned-move-row" key={move.id}>
                  <div className="transfer-item">
                    <strong>{move.sourceName}</strong>
                    <TransferPaths from={paths.from} to={paths.to} />
                    {!executingPlan && !move.sourceIsDirectory && (() => {
                      const blocking = blockingIssues(move, [
                        ...(planValidation?.issues ?? []),
                        ...(planPreflight?.issues ?? []),
                      ]);
                      return blocking.length > 0 && (
                        <span className="plan-blocked-note">{blocking[0].message}</span>
                      );
                    })()}
                  </div>
                  <span>{formatBytes(move.sourceSizeBytes)}</span>
                  {executingPlan ? (
                    <span className="transfer-run-state">{runState}</span>
                  ) : (
                    <button className="plan-remove-button" onClick={() => void removePlannedMove(move.id)}>
                      Remove
                    </button>
                  )}
                </div>
              );
            })}
          </div>
        </>}
      </section>}

      {showLegacyDashboard && showTransfers && <section className="section-block">
        <div className="section-heading">
          <h2>Transfer history</h2>
          {transfers.length > 0 && <span>{transfers.length}</span>}
        </div>
        {transfers.length === 0 ? (
          <p className="section-empty">No transfers yet. Copied files will be listed here.</p>
        ) : <>
          <p className="section-description">Most recent first. Original files are left untouched.</p>

          <div className="transfer-history">
            {(showAllTransfers ? transfers : transfers.slice(0, RECENT_TRANSFER_COUNT)).map((transfer) => {
              const outcome = transferOutcome(transfer.status);
              const size = transfer.status === "completed" ? transfer.copiedBytes : transfer.totalBytes;
              const location = locations.find((item) => item.id === transfer.destinationLocationId);

              return (
                <div className={`transfer-history-row ${outcome.tone}`} key={transfer.id}>
                  <div className="transfer-item">
                    <strong>{fileName(transfer.sourceRelativePath)}</strong>
                    <TransferPaths
                      from={formatLocationPath(sourceDriveName(transfer.sourceDriveId), transfer.sourceRelativePath)}
                      to={formatLocationPath(location ? location.userLabel ?? location.displayName : "Unknown location", transfer.destinationRelativePath)}
                    />
                    {transfer.status === "failed" && transfer.errorMessage && (
                      <p className="transfer-error">{transfer.errorMessage}</p>
                    )}
                  </div>
                  <span>{size === null ? "" : formatBytes(size)}</span>
                  <div className="transfer-outcome">
                    <span className="transfer-status">{outcome.label}</span>
                    <span>{formatDate(transfer.completedAt ?? transfer.startedAt ?? transfer.createdAt)}</span>
                  </div>
                </div>
              );
            })}
          </div>

          {transfers.length > RECENT_TRANSFER_COUNT && (
            <button className="transfer-more-button" onClick={() => setShowAllTransfers((showAll) => !showAll)}>
              {showAllTransfers ? "Show recent only" : `Show all ${transfers.length.toLocaleString()}`}
            </button>
          )}
        </>}
      </section>}

      {showLegacyDashboard && catalogued.length > 0 && <section className="section-block">
        <div className="section-heading"><h2>Tools</h2></div>
        <div className="tool-row">
          <div>
            <h3>Probable duplicates</h3>
            <p>Same filename and exact file size. Contents have not been compared.</p>
          </div>
          <button
            className="section-action"
            onClick={() => void loadDuplicates()}
            disabled={duplicatesLoading}
          >
            {duplicatesLoading ? "Checking…" : showDuplicates ? "Hide" : "Find duplicates"}
          </button>
        </div>

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
                          <strong>{formatLocationPath(driveDisplayName(file.driveId, file.driveName), parentFolder(file.relativePath))}</strong>
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

        <div className="tool-row">
          <div>
            <h3>Largest files</h3>
            <p>The 100 biggest files across every catalogued drive.</p>
          </div>
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
                  <span>{formatLocationPath(driveDisplayName(file.driveId, file.driveName), parentFolder(file.relativePath))}</span>
                </span>
                <span>{connectedIds.has(file.driveId) ? "Connected" : "Offline"}</span>
                <span>{formatBytes(file.sizeBytes)}</span>
              </button>
            ))
          )}
        </div>}
      </section>}

      </div>
    </main>
  );
}

export default App;
