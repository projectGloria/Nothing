# "Nothing" — an Everything-class instant file search for Windows

## Context
You want a daily-driver replacement for voidtools Everything: type → results in the same frame, across ~1.4M files, with a tiny footprint and Everything's no-nonsense 4-column UI (Name / Path / Size / Date Modified, dark theme, item count in status bar). The repo (`Nothing`) is empty, so this is greenfield. Decisions already made: **a background SYSTEM service does the indexing** (no UAC prompt per launch) and **v1 indexes local NTFS volumes only**.

The one physical fact that drives everything: you can't walk directories fast enough. Speed comes from **reading the NTFS Master File Table raw** (sequential multi-MB reads, parsed in memory), then **tailing the USN change journal** for live updates. That needs admin rights, hence the service.

## Stack (and why)
- **Rust, stable-x86_64-pc-windows-msvc** (rustc 1.99 + MSVC 14.44 + SDK 10.0.26100 are already installed). Native code, no GC, no runtime; memory safety matters when you're hand-parsing on-disk binary records.
- **Raw Win32 via `windows-sys`**, no GUI framework. A virtual `SysListView32` is exactly what Everything uses, and it's the cheapest possible table: it only asks for the ~40 visible rows.
- Runtime deps: `windows-sys`, `memchr` (SIMD substring search). Build-only: `embed-resource` (manifest + icon). No async runtime, no rayon — `std::thread::scope` is enough.
- Release profile: `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `strip = true`, `opt-level = 3`; `.cargo/config.toml` sets `target-feature=+crt-static` so the exes need no VC++ redistributable.

## Layout
```
Cargo.toml                 workspace + release profile
.cargo/config.toml         crt-static
crates/core/               lib: index model, query parser, matcher, sort orders, persistence, IPC protocol
crates/svc/  -> nothing-svc.exe   MFT reader, USN monitor, Windows service, named-pipe server
crates/ui/   -> nothing.exe       window, virtual listview, dark mode, shell integration, pipe client, --install
```

## Index (crates/core) — the part that makes it fast
Struct-of-arrays, one per volume, indexed by a dense `u32` entry id:
- `parent: Vec<u32>`, `flags: Vec<u8>` (dir/hidden/system), `size: Vec<u64>`, `mtime: Vec<u64>`, `name_off: Vec<u32>`
- **Names arena**: one contiguous `Vec<u8>` of UTF-8 names, `\0`-separated; plus a **case-folded twin arena** used only for matching.
- `frn_to_id: Vec<u32>` indexed directly by MFT record number (MFT record numbers are dense — a flat array beats any hash map).
- Paths are never stored; they're built on demand by walking `parent` (only for visible rows / path-matching).
- Target: **< 70 bytes/entry** → ~100 MB service RSS for 1.4M files (Everything is in the same ballpark).

**Search** = one SIMD `memmem` pass over the folded arena, split across all cores at `\0` boundaries (30 MB at memory bandwidth ≈ a few ms). Hit offset → entry id via binary search on `name_off`. Output is a match bitset.

**Sorting is precomputed, not done per query**: keep presorted permutations for Name, Size, Date (built at index time, Path built lazily on first use). Result = walk the chosen permutation, emit ids whose bit is set → already sorted, O(n) with no comparisons. Live updates insert/remove in the permutations via binary search + memmove (sub-ms); big churn bursts trigger a re-sort.

**Incremental narrowing**: if the new query is a strict refinement of the previous one (you typed another character), scan only the previous result set.

**Query syntax v1**: space = AND, `|` = OR, `!` = NOT, `"quoted phrase"`, `*`/`?` wildcards (whole-name match), a term containing `\` matches against full path, `ext:jpg;png`, `file:`, `folder:`, `case:`. (Regex, `size:`, `dm:` filters are v2.)

**Persistence**: on shutdown + every N minutes, dump the arrays to `%ProgramData%\Nothing\index.bin` (raw little-endian blobs + header with per-volume journal id and last USN). On boot: load (~100 ms), replay the USN journal from the saved USN; if the journal id changed or wrapped, full MFT rescan for that volume only.

## Service (crates/svc)
1. **Volume discovery**: `FindFirstVolumeW` → keep fixed drives where `GetVolumeInformationW` says NTFS; re-check on `SERVICE_CONTROL_DEVICEEVENT`.
2. **MFT scan** (per volume, one thread each): open `\\.\C:`, `FSCTL_GET_NTFS_VOLUME_DATA` for record size, `FSCTL_GET_RETRIEVAL_POINTERS` on `$MFT` for its extents, read extents in 4 MB unbuffered chunks, apply update-sequence fixups, parse attributes:
   - `0x10 $STANDARD_INFORMATION` → mtime, attributes
   - `0x30 $FILE_NAME` → name + parent FRN (prefer Win32/POSIX namespace, skip DOS 8.3 duplicates; extra hard links become extra entries)
   - `0x80 $DATA` (unnamed) → size (resident length or non-resident real size)
   - `0x20 $ATTRIBUTE_LIST` / extension records → merge into base record
   - Overlap I/O and parsing (reader thread fills buffers, parser thread consumes).
   - Target: **C: (≈1.4M entries) cold scan < 3 s on SSD**.
3. **Live updates**: per-volume thread blocks on `FSCTL_READ_USN_JOURNAL` (`BytesToWaitFor = 1`); handles create / delete / rename-old+new / close-with-data-change. Size/mtime refresh via `OpenFileById` + `GetFileInformationByHandleEx`. Updates are batched under a write lock (~50 ms), then UI is notified.
4. **IPC**: named pipe `\\.\pipe\nothing`, `PIPE_REJECT_REMOTE_CLIENTS`, DACL = SYSTEM + interactive user. Binary length-prefixed messages:
   - `Query{gen, text, sort, asc}` → `Count{gen, total}` (service cancels any older gen still running)
   - `Rows{gen, first, n}` → name, path, size, mtime, flags for that window
   - push `Changed` → UI re-runs the current query
   - Note: like Everything, a SYSTEM indexer can reveal names of files the user can't open. Fine for a single-user PC; worth knowing.
5. Service plumbing: `StartServiceCtrlDispatcherW`, auto-start, runs as LocalSystem, low CPU/IO priority during initial scan.

## UI (crates/ui) — Everything's layout, nothing more
- Manifest: PerMonitorV2 DPI, Common Controls v6.
- Window = menu bar (File/Edit/View/Search/Help) + `EDIT` search box + `SysListView32` (`LVS_REPORT | LVS_OWNERDATA`, full-row select) with Name / Path / Size / Date Modified + status bar (`1,407,908 items`, selection total size).
- Every `EN_CHANGE` sends a query immediately (no debounce — search is single-digit ms). `SetItemCountEx` with the total; `LVN_ODCACHEHINT` fetches one page of rows per pipe round-trip; `LVN_GETDISPINFO` serves from a small row cache. UI RSS target **< 15 MB**.
- **Dark mode** following the system theme: `DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)`, uxtheme `SetPreferredAppMode`/`AllowDarkModeForWindow`, `SetWindowTheme(.., "DarkMode_Explorer")`, custom-draw for header/status bar text.
- **Icons**: system image list, `SHGetFileInfoW(SHGFI_USEFILEATTRIBUTES | SHGFI_SYSICONINDEX)` keyed by extension (no disk access); real icons for `.exe/.lnk/.ico` loaded on a background thread.
- Interactions: Enter / double-click = open; Ctrl+Enter = open containing folder with item selected (`SHOpenFolderAndSelectItems`); right-click = **real Explorer context menu** (`IContextMenu` via `SHCreateDefaultContextMenu`); drag rows out to other apps (`SHCreateDataObject` + `DoDragDrop`); Ctrl+C copies items, Ctrl+Shift+C copies full path; column header click = sort; Esc clears search.
- **Instant summon**: tray icon, closing hides instead of exiting, configurable global hotkey (default `Win+Alt+Space`) shows window with search box focused and text selected. Optional start-at-login (HKCU Run).
- Settings in `%AppData%\Nothing\settings.ini`: window rect, column widths, sort, hotkey.
- Status bar shows "Indexing C:… 63%" while the service is scanning; if the service isn't installed, the window says so with an "Install service" button.

## Install
`nothing.exe --install` self-elevates (`ShellExecuteW "runas"`), copies both exes to `%ProgramFiles%\Nothing`, `CreateServiceW` (auto-start, LocalSystem), starts it. `--uninstall` reverses and deletes the index. No MSI in v1.

## Build order
1. Workspace + `core` index model + MFT reader, exposed as `nothing-svc --scan C:` printing entry count, elapsed ms, bytes/entry.
2. Query parser, matcher, presorted orders, incremental narrowing; `--bench "query"` mode.
3. Persistence + USN tailing.
4. Service wrapper + pipe server/protocol.
5. UI: window, virtual list, search box, status bar, dark mode, icons.
6. Shell integration: open, context menu, drag-out, copy, tray, hotkey.
7. `--install`/`--uninstall`, settings file.

## Verification
- `cargo test`: query parser, wildcard/AND/OR/NOT matcher, MFT record parser + fixup against captured raw-record fixtures, persistence round-trip.
- `nothing-svc --scan C:` (elevated): entry count should be within a fraction of a percent of Everything's **1,407,908** on your machine; must hit < 3 s and < 70 B/entry.
- `nothing-svc --bench`: per-query timings for 1-char, 3-char, path, wildcard and 4-term queries; target < 10 ms p99 over the full index.
- Live updates: create / rename / delete / grow a file in Explorer → visible in the UI within ~1 s; reboot → index loads from disk, catches up via USN without a rescan.
- Footprint: Task Manager — service ~100 MB, UI < 15 MB, near-zero CPU at idle.
- Manual UX pass against the Everything screenshot: dark theme, icons, sort by each column, context menu, drag into another app, hotkey summon.
