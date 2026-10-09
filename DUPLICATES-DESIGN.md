# Duplicate awareness: proposed design

**Status: built 2026-10-02**, as designed except where "As built" at the end says otherwise.

The V1 goal is information only. Tidy Drives says which files appear more than once, where the copies are, why it thinks they match, and how much space the confirmed extra copies take. It never deletes, never suggests deleting a particular copy, and never ranks one copy as "the original".

## What exists today

- **Backend: `probable_duplicates`.** It groups files by case-insensitive name and exact size, excluding size 0, and returns the 100 groups with the most space in extra copies. Its test is `probable_duplicates_are_grouped_by_name_ignoring_case_and_size`.
- **No content comparison** exists anywhere. The catalogue holds names, sizes and dates only.
- **Its only UI is the old dashboard,** behind `showLegacyDashboard`, which is always `false`. Users can't reach duplicates today. That UI labelled extra copies "potential waste", which leans towards deletion.
- **Weaknesses of the current query:**
  - it compares names case-insensitively but not Unicode-normalised, so "Café" typed composed and decomposed don't match;
  - it includes hidden system files, such as `._` AppleDouble files, which can number in the thousands on exFAT drives;
  - it has no index on size.
- **Your real catalogue, read-only:** 990 files, 118 GB. 62 groups share a name and size. Only 19 groups of files 1 MB or larger share a size, 49 files and 0.17 GB in all. 4 of those groups have files with different names, which would be missed by name and size, and could be exact copies that were renamed.

## Two kinds of duplicate

| | **Identical** (exact) | **Probable** |
|---|---|---|
| Rule | Same size and same SHA-256 of the full contents | Same size and same name, ignoring case and Unicode form. Contents not compared |
| Names | Can differ. Renamed copies are found | Must match |
| Needs drives connected | Only while being checked. Results are kept afterwards | No. Comes from the catalogue alone |
| Counted in the space total | Yes | No |
| Shown as | "Identical · same contents, checked 1 Oct 2026" | "Probable · same name and size, contents not checked" |

A probable group whose contents are later checked becomes identical, or disappears if its contents differ.

## UX

The feature sits under **Browse**, like a drive. There's no new sidebar item and no dashboard.

**Browse landing:** one quiet row under the drive cards, shown only when there is something to show:

```
Duplicates                                                         ›
12 identical groups · 48.2 GB in extra copies · 30 probable
```

**Duplicates view.** It opens like a drive, with the breadcrumb `All files / Duplicates` and **Browse** still current in the sidebar:

```
All files / Duplicates
Duplicates
Files that appear more than once across your drives. Copies are often
intentional, such as backups. Tidy Drives never removes files.

Identical: 12 groups · 48.2 GB in extra copies           [Check contents]
Checks files that share a size on connected drives. Reads files only.

IDENTICAL
Holiday 2019.mov       3 copies · 24.1 GB each     Same contents, checked 1 Oct
  Source / Films / 2019                    Connected   1 Aug 2019   [Show in folder] [Open]
  Archive / Video                          Offline     1 Aug 2019   [Show in folder]
  Mars / Backups / Films                   Connected   1 Aug 2019   [Show in folder] [Open]
interview_final.wav    2 copies · 1.2 GB each      Same contents, checked 1 Oct  ›

PROBABLE
DSC_0412.JPG           2 copies · 8.4 MB each      Same name and size, not checked  ›
```

- **What the list answers:**
  - *What appears duplicated?* Each group shows its name, size and number of copies.
  - *Where are the copies?* Expanding a group lists each copy as "Drive / Folder", with whether the drive is connected, the copy's date, **Show in folder** (opens Browse there) and **Open**.
  - *Why does Tidy Drives think they match?* Each group carries its reason and the date its contents were checked.
  - *How much space?* The total counts identical groups only: size × (copies − 1), called "extra copies".
- **Order:** identical groups first, then probable, each sorted by space in extra copies. The top 200 groups are listed, with a note when there are more.
- **Not there:**
  - no checkboxes, no selection, no delete or "keep this one", and no marking of an original;
  - no "waste" wording;
  - nothing on Browse rows. Possible later: a quiet note on a file's row, "2 other copies".
- **Check contents:**
  - It is started by hand, never in the background. Before it starts, the window says how much it will read ("About 3.2 GB on Source and Mars").
  - It shows progress the way a scan does, and **Cancel** works at any point. Results checked before a cancel are kept.
  - Like a scan, it can't run during a copy, and the reverse.
  - Accessibility follows ACCESSIBILITY.md: groups are buttons with `aria-expanded`, and the check announces its start and result, not its progress.

## Technical design

### Data

Schema version 2 (see DATABASE.md), added in place with a fixture test for version 1:

```sql
CREATE TABLE content_checks (
    drive_id       TEXT NOT NULL REFERENCES drives(persistent_identifier) ON DELETE CASCADE,
    relative_path  TEXT NOT NULL,
    size_bytes     INTEGER NOT NULL,
    modified_at    INTEGER,
    sample_hash    BLOB NOT NULL,   -- SHA-256 of size + first and last 64 KB
    full_hash      BLOB,            -- SHA-256 of the whole file, when needed
    checked_at     INTEGER NOT NULL,
    PRIMARY KEY (drive_id, relative_path)
);
CREATE INDEX idx_files_size ON files(size_bytes) WHERE is_directory = 0;
```

- **When a stored hash counts:** only while the catalogue entry still has the same size and date. A rescan that finds a changed file makes its hash stale automatically, with no extra bookkeeping. A rescan doesn't touch this table. Rows for files that no longer exist are ignored, and removed when a check runs.
- **Hash:** SHA-256 from the `sha2` crate, a new dependency. Reading is limited by the disk, so a faster hash would gain little, and SHA-256 is the most widely understood choice.

### Finding identical copies, in stages

1. **Size.** From the catalogue, with no reading: files of 1 MB or more that share a size with another file. Files under 1 MB stay probable-only, because checking them costs more than it tells. Hidden files are skipped, as in Browse.
2. **Sample.** Read the first and last 64 KB of each candidate on a connected drive. Different samples mean different contents, and that is settled cheaply. Unrelated files that happen to share a size are almost always separated here.
3. **Full.** Hash the whole file only for candidates whose samples match another file's. Equal full hashes mean identical.

What gets read is therefore roughly the duplicated data itself, not the library. On your catalogue that's under 0.2 GB.

### Safety

The check only ever **reads**:

- each file is opened with `open_regular_file`: no links followed, regular files only;
- reads bypass the cache, so a check doesn't push other files out of memory;
- nothing on any drive is written, renamed or touched;
- before a hash is stored, the file's size and date are checked against the catalogue again;
- the check holds the same lock as scans and copies, so it never runs alongside them.

### Commands

| Command | Does |
|---|---|
| `list_duplicates` | Read-only. Identical and probable groups with their copies, and the totals. Replaces `probable_duplicates`, which nothing reachable calls. |
| `estimate_content_check` | What a check would read, per connected drive |
| `check_duplicate_contents(run_id)` | Runs stages 2 and 3. Emits `duplicate-check-progress`. Cancelled with `cancel_duplicate_check(run_id)`, like a copy. |

### Tests

These are deterministic, on temporary folders:

- identical files with different names are grouped;
- same size with different contents is never called identical, whether they differ at the start, in the middle or at the end;
- a file changed after its check is no longer counted until it is checked again;
- copies on an offline drive keep their checked status;
- a link or a pipe is refused;
- a cancel keeps finished checks and records nothing for the rest;
- the check never changes any file's contents or date;
- names are matched across Unicode forms and case;
- hidden files are excluded;
- the totals count identical groups only;
- the check is refused while a copy or scan runs;
- the version 1 to version 2 migration keeps all data.

## Build order

Each step is a small, separately tested commit:

1. **Probable view.** The Browse entry and Duplicates view with probable groups only. Names normalised, hidden files excluded, size index. No reading of files.
2. **Schema version 2.** `content_checks`, with its migration and fixture test.
3. **Content check.** The staged check, with progress, cancel and estimate.
4. **Identical in the view.** Identical groups, totals and the check controls.
5. **Later, needs a separate decision:** record SHA-256 during copy verification, which already reads both files in full. Tidy Drives' own copies would then show as identical after the destination is rescanned, without being read again. This touches the copy engine, so it is deliberately not part of V1.

## Known limitations

- **Space is approximate.** "Extra copies" assumes each copy uses its own space. APFS clones and hard links share space, and this design doesn't detect them.
- **"Identical" means identical when checked.** A file rewritten later with the same size and date wouldn't be noticed until it is checked again. This is the same limit as SAFETY-MATRIX F4.
- **Files under 1 MB** are never checked, only shown as probable.

## Decisions (2026-10-01)

1. **Placement:** a Duplicates entry under Browse, opening its own view.
2. **Content check:** started by hand only, never in the background or prompted after a scan.
3. **Minimum size for content checks:** 1 MB. Smaller files are shown only as probable.
4. **Hash:** SHA-256 with the `sha2` crate. This was the default; no alternative was asked for.
5. **Step 5,** hashing during copy verification: deferred past V1. The copy engine stays untouched.

## As built (2026-10-02)

Steps 1 to 4 are built. Step 5 is deferred, as decided.

These differ from the design above:

- **The size index came with schema version 2,** not with step 1. That way the schema changed once.
- **When a probable group goes away.** A file stops being probable once its contents are settled against every other file of its size. Every other such file must have been checked, and each must either have a different sample or have been read in full too. So a pair proven different disappears, and a pair with an unchecked copy, or with a check made stale by a rescan, stays probable. In one case a file appears in both lists: two copies proven identical, plus a third with the same name on a drive that wasn't connected for the check. The identical section lists the two; the probable group lists all three, because the third may match them.
- **Group names.** An identical group is named after the name most of its copies share. A copy whose name differs shows its full path.
- **The estimate is an upper bound.** It reads "This reads up to … on Source and Mars, usually much less", because samples settle most files without a full read.
- **The 1 MB threshold is decimal** (1,000,000 bytes), matching how sizes are shown.

In the lists, "Show in folder" opens Browse at that copy's folder. "Open" is offered only while the copy's drive is connected.
