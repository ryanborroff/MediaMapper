# Media Mapper UI Guidelines

These guidelines define the interface conventions for Media Mapper.

They are intentionally small. Media Mapper should feel like a focused desktop utility rather than a dashboard or web application.

## 1. Product character

Media Mapper is a calm, functional desktop tool for understanding and reorganising storage.

The interface should be:

- clear before clever
- compact without feeling cramped
- visually quiet
- explicit about what is real and what is planned
- consistent with native macOS conventions where practical
- usable without learning Media Mapper-specific terminology

Avoid decorative UI, unnecessary icons, excessive colour, oversized typography and dashboard-style visual noise.

## 2. Information hierarchy

Use a small number of clearly differentiated levels.

### Page title

The primary screen title describes the current workspace.

Example:

`Drive catalogue`

### Section heading

Use for major groups of information.

Examples:

`Search all drives`
`Probable duplicates`
`Largest files`
`Planned moves`
`Connected`
`Offline catalogue`

Section headings should have similar visual weight throughout the application.

### Card title

The primary identity of a drive or location.

If a user-defined drive label exists, it is the primary title.

Example:

`Mars`

The filesystem volume name then appears as secondary information:

`Backup`

If no user label exists, the filesystem volume name remains the primary title.

### Eyebrow

Small uppercase labels may identify state or context.

Examples:

`MEDIA MAPPER`
`CONNECTED`
`OFFLINE`
`PLANNED LOCATION`

Use these sparingly.

## 3. Typography

Keep the number of text sizes deliberately limited.

Use differences in weight, spacing and colour before introducing another font size.

General hierarchy:

1. page title
2. section/card title
3. normal interface text
4. secondary/supporting text
5. small labels and metadata

Do not create a new font size for every type of metadata.

File lists, search results and duplicate results should use consistent typography.

## 4. Spacing

Prefer consistent vertical rhythm over individually tuned gaps.

Related information should sit together.

Different concepts should have visibly greater separation.

Buttons associated with a heading or description should align with that block rather than floating between text rows.

Cards should have enough internal space to remain readable but should not become oversized containers.

## 5. Buttons and actions

Primary actions use the existing dark filled button treatment.

Examples:

`Scan drive`
`Browse catalogue`
`Plan move`

Secondary actions should normally be quieter text or outline controls.

Examples:

`Cancel`
`Edit label`
`Add label`
`Remove`

Do not give secondary actions the same visual weight as the primary action.

Disabled controls must look disabled rather than merely lower contrast.

Action labels should describe the action directly.

Prefer:

`Plan move`

rather than:

`Confirm`

## 6. Terminology

Use ordinary storage terminology whenever possible.

Preferred terms:

`Drive`
`Folder`
`File`
`Location`
`Move to`
`Planned`
`Connected`
`Offline`

Avoid exposing implementation terminology such as:

`persistent identifier`
`location ID`
`database record`

unless needed for diagnostics.

### Destination controls

When planning a move:

`Move to` identifies the destination drive or Mac location.

`Folder` identifies the folder within that location.

Do not use `Destination` and `Destination folder` together. They are too easily confused.

## 7. Drive identity

External drives have three separate identities.

### Media Mapper label

Optional, user-defined and human-readable.

Examples:

`Mars`
`Archive 2`
`Studio`

This is the primary visible identity when present.

### Volume name

The filesystem name reported by the operating system.

Example:

`Backup`

Preserve this value and show it as secondary information when a Media Mapper label exists.

### Persistent identifier

The machine identity used internally to associate a physical drive with its catalogue.

This should normally remain invisible.

A Media Mapper label must never be used as the database identity of a drive.

Changing a label must not break catalogue records or planned moves.

## 8. Connection state

Connection state describes whether the physical drive is currently mounted.

Use:

`Connected`
`Offline`

Do not use `Disconnected` for a catalogued drive. `Offline` better describes a drive that remains available in Media Mapper but is not physically attached.

Connection state must reflect current physical state, not the state recorded during the previous scan.

The application should update this state automatically while open.

A drive being offline must not prevent browsing its catalogue or planning changes involving it.

## 9. Catalogue, plan and execution

These are three distinct states and must never be visually or verbally blurred.

### Catalogue

Represents Media Mapper's stored metadata describing the real filesystem at the time of its last successful scan.

Browsing the catalogue does not read the physical drive.

### Plan

Represents the organisation the user intends to create.

Planning is virtual.

Creating, changing or removing a plan must not move, rename, copy or delete files.

Use explicit language where ambiguity is possible:

`This changes the Media Mapper plan only. No files are moved.`

### Execution

Future functionality that applies a plan to real storage.

Execution must be presented as a separate deliberate operation.

Never make a planning control look as though it immediately modifies a physical drive.

## 10. Planned locations

Show planned paths in a recognisable path-like form.

Example:

`Mars / Documentaries / Film.mp4`

For a local Mac destination:

`Media Archive / Documentaries / Film.mp4`

Prefer a user-defined drive label over the underlying volume name.

If a proposed destination is identical to the current location, prevent the redundant plan and show:

`Already at this location.`

## 11. Connected and offline drives

Connected drives and offline catalogues are separate interface concepts.

A connected drive may also have an existing catalogue.

Do not duplicate its catalogue as a separate offline card while it is connected.

When it disconnects, its catalogue remains available under the offline catalogue section.

## 12. Colour semantics

Colour should communicate state, not decorate the interface.

### Green

Use for successful or currently available states.

Example:

`CONNECTED`

### Amber

Use for warnings, blocked actions or conditions requiring attention.

Example:

`Already at this location.`

### Red

Reserve for actual errors, destructive actions or danger.

Do not use red for ordinary offline states.

### Neutral grey

Use for secondary metadata, inactive states and offline information.

Keep most of the interface neutral so semantic colour retains meaning.

## 13. Status and microcopy

Status copy should state what happened or what the user needs to know.

Use sentence punctuation when the text is a sentence.

Examples:

`Already at this location.`

`This changes the Media Mapper plan only. No files are moved.`

Avoid marketing language inside the working interface.

Avoid technical explanations unless they help the user make a decision.

## 14. Safety messaging

Media Mapper's read-only catalogue behaviour is a product feature and should remain visible without dominating the interface.

Current catalogue safety principle:

`Scanning reads names, paths, sizes and timestamps only and does not open, rename, move, copy or delete files.`

Catalogue-browser principle:

`This view comes from Media Mapper's local catalogue. Search and browsing do not read the drive.`

Safety messages should be concise, factual and visually secondary.

## 15. Empty states

Empty states should explain both the state and the obvious next action.

Example:

`No external drives detected`

`Connect a drive and it will appear here.`

Drives are detected automatically, so empty states should not instruct users to refresh manually.

## 16. Lists and file information

File-oriented views should behave consistently.

Where applicable, maintain predictable columns for:

- name
- modified date
- size or type
- location/status

Long filenames should truncate cleanly rather than distort the layout.

Folder and file interaction should remain visually consistent between catalogue browsing, search, duplicates and largest-file views.

## 17. Progressive disclosure

Do not expose every capability simultaneously.

Show controls when they become relevant.

Examples:

- drive label editing appears only when requested
- planning controls appear after selecting a file
- duplicate details expand on demand
- execution controls should appear only when there is something executable

This keeps the main catalogue readable as functionality grows.

## 18. New feature checklist

Before adding a new UI pattern, check:

1. Can an existing component or interaction handle it?
2. Does it introduce another unnecessary font size?
3. Is the primary action obvious?
4. Is terminology consistent with the rest of Media Mapper?
5. Does colour communicate a real state?
6. Is it clear whether the user is viewing reality, planning a change or executing one?
7. Does it work when the relevant external drive is offline?
8. Does it preserve the distinction between a drive's label, volume name and persistent identity?

If a new convention is genuinely necessary, update this document with the implementation.
