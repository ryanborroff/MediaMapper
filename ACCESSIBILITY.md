# Accessibility

Keyboard and VoiceOver status of Media Mapper's window. The audit, done in Phase 5 of [HARDENING.md](HARDENING.md), fixed defects without changing the design. Re-run the checks at the end after any change to the window.

## What was found and fixed

| Area | Found | Fixed |
|---|---|---|
| Browse rows | Folder rows and search-result rows were `<div role="button">` with buttons inside them. The children of a button are hidden from VoiceOver, so **Open**, **Add to plan** and **Remove** couldn't be reached (RELEASE-TESTING R4). Space didn't activate the rows. | Rows are no longer buttons. A click anywhere on a row still works as before. For keyboard and VoiceOver, the name is a real button: it opens the folder or the file. A file's location in search results is a button that opens its folder. |
| Planned arrivals | A planned file was a disabled button ("dimmed button") | It's plain text. A planned folder is still a button. |
| Plan panel | Closing it or adding to the plan left focus nowhere. Escape did nothing. | Escape closes it. Focus returns to the row's **Add to plan**, or to the **Remove** that replaced it. |
| Folder picker | `role="dialog"` without dialog behaviour. Focus stayed behind when it opened and was lost when moving between folders. Escape did nothing. | It's a labelled group controlled by its button (`aria-expanded`, `aria-controls`). Focus moves to the first folder, or to **Choose this folder** when there are none. Escape closes only the picker. Focus returns to its button. The current folder is marked. |
| Subfolder button | The surrounding `<label>` replaced its name with "Subfolder (optional)", so the chosen folder wasn't read | It's named "Subfolder (optional), *folder*, Choose…" |
| Copy confirmation | **Continue** and **Cancel** replace each other, which dropped focus | **Continue** moves focus to the confirmation text, not to **Copy and verify**, so pressing Return twice can't start a copy. **Cancel** and Escape return focus to **Continue**. |
| Copy progress and result | The progress area was a live region that changed several times a second, which VoiceOver would read endlessly. Focus was lost when the area closed. | One status message per stage: "Copying *file*, file 1 of 3", "Verifying…", then the outcome. A cancel is announced as cancelled. Focus moves to the progress area, then to the result, then to the page heading after **Done** or **Dismiss**. |
| Scanning | The scan area was a live region with a running count | Announces "Scanning *drive*", then the result or "Scan cancelled". Focus moves from **Scan** to **Cancel scan**, and back to **Rescan** at the end. |
| Search | The whole results list was a live region | Only the count is announced ("4 results"). Results are a list. |
| Errors | The error banner wasn't announced | `role="alert"` |
| Drive labels | The label field was named only by its placeholder. Escape and Save left focus nowhere. | Named "Label for *drive*". Focus returns to **Add label** or **Edit name**. |
| Repeated buttons | Many identical **Add to plan**, **Remove**, **Browse**, **Scan** and **Show in Finder** buttons | Each is described by its file or drive name (`aria-describedby`). The visible names are unchanged. |
| Decorative text | "/", "›" and "→" were read aloud | Hidden from VoiceOver. Plan rows read "From … To …". |
| "Not on the drive yet" | `aria-label` on a plain `<span>`, which isn't reliably read | Visually hidden text |
| Status by colour | "Space after transfer" turned red when there wasn't enough space, with nothing else said | Adds "(not enough space)" for VoiceOver. Everything else that uses colour already has text beside it. |
| Contrast | "From" and "To" used the disabled grey: 2.85:1 in light mode, 2.65:1 in dark | They now use the secondary text colour: 5.3:1 light, 5.6:1 dark |
| Search field focus | `outline: none`, with a 14% tint as the only sign of focus | Uses the app's standard focus ring |

## Checked and left as is

- **Contrast.** Every text colour meets 4.5:1 on every background in both appearances. Muted text, the lowest, is 4.7:1. Disabled controls are exempt. The focus ring is at least 4.2:1 against its background.
- **Focus ring.** One rule, `:focus-visible`, for every control. Unchanged.
- **Navigation.** The sidebar uses real buttons, with `aria-current="page"` on the current view. Focus stays in the sidebar after switching views, as in Finder.
- **Labels.** Search fields, the location menu and **Show hidden items** are labelled.
- **Disabled controls.** Native `disabled` throughout.
- **Progress bar.** `role="progressbar"` with a value, which VoiceOver can read on demand.
- **Drag and drop** onto a folder is mouse only. **Add to plan** does the same with the keyboard.
- **Reduced motion.** Scrolling to the plan panel is instant when Reduce Motion is on.

## Verified

The window was run in a browser with stand-in data. Real key presses drove it, and the accessibility tree and focus were read after each step:

- In Browse, the accessibility tree now exposes each row's name and its **Add to plan** button. They were hidden before.
- Tab to **Add to plan** → Return opens the panel with focus on **Location** → Escape closes it with focus back on **Add to plan**.
- Subfolder → Return opens the picker on its first folder → Return opens that folder and focuses **Choose this folder** → Escape closes the picker only, with focus on the Subfolder button.
- **Add to plan** in the panel saves it, and focus lands on the row's new **Remove**.
- **Continue**, then Return again, didn't start a copy. Escape returned focus to **Continue**.
- **Copy and verify** made exactly three announcements: copying, verifying, finished. Focus went from progress to result, then to the heading after **Done**.
- **Cancel** during a copy announced "Copy cancelled. No files were copied…". The plan was kept.
- **Rescan** moved focus to **Cancel scan**, announced the start and the result, then returned focus to **Rescan**.
- The label editor is named "Label for Source", and Escape returned focus to **Add label**.
- Screenshots in light and dark mode match the previous layout. The only visible change is the focus ring on search fields.

VoiceOver itself can't be driven from here, and the app runs in WebKit, not Chromium. The checklist below covers that.

## VoiceOver checklist (by hand)

Turn on VoiceOver with ⌘F5. VO means Control-Option.

1. **Drives.** VO-arrow through a drive card. Its name, status, capacity and scan time are read. **Scan**: hear "Scanning …" and then the result. **Cancel scan** is focused while the scan runs.
2. **Browse.** Open a drive. Each row reads its name as a button, then the date and size. VO-arrow reaches **Add to plan**, which is read with the file's name.
3. **Search.** Type in the search field. Hear only the count, such as "4 results". Tab reaches each result's name, location and **Add to plan**.
4. **Add to plan.** The panel opens on **Location**. The Subfolder button reads the chosen folder. In the picker, the current folder is announced. Escape closes the picker, then the panel.
5. **Plan.** Planned files read "*name*, From …, To …". **Continue** reads the confirmation. **Copy and verify** announces each file's stage and the outcome, and nothing in between.
6. **Cancel** during a copy: hear "Copy cancelled", never "failed".
7. **Transfers.** History is a list. **Show in Finder** is read with the file's name.
8. With **Increase contrast** and **Reduce motion** turned on, the window is still usable.

## Remaining

| Issue | Class |
|---|---|
| The VoiceOver checklist above hasn't been run in the app itself | Should fix before V1 |
| The Browse file table is a grid of `<div>`s, not a table. Its column headings (Name, Modified, Size) are read as loose text, and cells aren't tied to them. The values explain themselves (dates, sizes, "Folder"). Table roles would need the planned-folder rows, which are buttons, restructured. | Safe to defer |
| Switching views doesn't announce the new view. The sidebar marks it as current, and focus stays there. | Safe to defer |
| "1 results" and "1 files" | Phase 6 |
| The old dashboard code behind `showLegacyDashboard` (always `false`) wasn't audited, because it never renders | Safe to defer: remove it |
