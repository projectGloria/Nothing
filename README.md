# Nothing

Instant file-name search for Windows, in the spirit of voidtools Everything. Type and the
results are there in the same frame.

Measured on the author's machine (Windows 11, Crucial P3 NVMe, 16 threads, C: = 1.1M files):

| | |
|---|---|
| First full index of C: (raw MFT read) | **3.3 s** (1.34 GB MFT; disk-bound at ~450 MB/s) |
| Every later start (snapshot + journal catch-up) | **86 ms** |
| Typical query (`chernobyl`, `*.zip`, `mkv`) over 1.1M entries | **0.7–0.9 ms** |
| Broad / path queries (`a`, `users\nihil\desktop`) | 6–9 ms |
| Live update latency (create/rename/delete/resize) | < 1 s |
| Index memory | ~96 bytes/file (~100 MB for 1.1M files) |
| Binaries | `nothing.exe` 600 KB, `nothing-svc.exe` 540 KB, no runtime deps |

## Install

Run **`nothing-setup.exe`**: a single 1.6 MB file with both binaries inside. Use
`nothing-setup.exe /S` for a silent install. Build it with:

```
build-installer.cmd        → target\release\nothing-setup.exe
```

Setup asks for elevation once. It copies both exes to `C:\Program Files\Nothing`,
registers the **Nothing Index** service (LocalSystem, auto-start, restarts on failure),
starts it, adds the UI to startup (tray, hidden), and registers Nothing in **Settings ▸
Apps ▸ Installed apps**. Running a newer setup over an existing install updates it in place
and keeps the index and settings.

Without the setup you can also run `nothing.exe --install` next to `nothing-svc.exe`, or
use **Tools ▸ Install / Repair Indexing Service**. To remove everything, uninstall it from
Settings ▸ Apps, or use **Tools ▸ Uninstall** or `nothing.exe --uninstall`.

Press **Win+Alt+Space** anywhere to summon the window. Closing the window hides it to the
tray, so the next summon is instant.

## Search syntax

| Query | Meaning |
|---|---|
| `foo bar` | both (AND) |
| `foo \| bar` | either (OR, binds tighter than AND) |
| `!foo` | exclude |
| `"a b"` | phrase containing a space |
| `*.mkv`, `a?c` | wildcards, matched against the whole name |
| `users\nihil\` | a `\` in a term matches against the full path |
| `ext:jpg;png` | by extension |
| `file:` / `folder:` | only files / only folders |
| `case:` / `path:` | per-term match case / match full path (stackable: `case:path:Src`) |

Search ▸ Match Case (Ctrl+I) and Match Path (Ctrl+U) apply to every term.

## Keys

Enter open · Ctrl+Enter open containing folder · Ctrl+C copy files · Ctrl+Shift+C copy
full paths · Del delete (Recycle Bin, Explorer's own dialogs) · Alt+Enter properties ·
Shift+F10 / right-click full Explorer context menu · drag rows into any app ·
Ctrl+A select all · Tab / ↓ / ↑ move between search box and list · typing in the list goes
to the search box · Esc clears the search, then hides · F5 refresh · click headers to sort.

## How it is fast

- **No directory walking.** The service reads the NTFS Master File Table directly with 8
  parallel unbuffered readers, and every other core parses 1 KB records as they arrive. A
  plain directory walk of the same drive takes 18–40 s.
- **No polling.** After the scan it tails the NTFS USN change journal, which the kernel
  writes on every create/rename/delete/write anyway.
- **No rescans.** The index is snapshotted to `%ProgramData%\Nothing\index.bin` (raw array
  dumps, loaded with a handful of `read_exact` calls), together with the journal position.
  A restart replays only what changed while it was down.
- **Struct-of-arrays index.** Names live in one contiguous arena, plus a case-folded twin of
  the exact same byte length, so both share one offset table. Plain substring queries run
  SIMD `memmem` over long contiguous stretches of that arena on all cores, from a persistent
  thread pool, instead of calling a matcher per file.
- **Sorting is free.** Name, size and date orders are kept presorted and updated
  incrementally. A query filters a sorted permutation through a match bitset, so results
  come out already sorted. Path order is a single integer sort, built lazily.
- **Typing narrows.** When the new query provably narrows the previous one (you typed
  another character), only the previous results are re-checked.
- **Path matching without paths.** Folder-level answers are memoized with a small
  "could the match cross into a child name" bitmask, so `users\nihil\desktop` costs about
  one memo lookup per file instead of building 1.1M path strings.
- **Virtual UI.** The window is raw Win32: a `LVS_OWNERDATA` ListView that holds no items,
  only a count. It pulls the ~40 visible rows on demand over a named pipe, on a separate
  thread, so the UI thread never waits on the service.

## Layout

```
crates/core   index, query engine, sort orders, snapshot format, pipe protocol (no Win32, unit-tested)
crates/svc    nothing-svc.exe: MFT reader, USN tailer, engine, named-pipe server, Windows service
crates/ui     nothing.exe: window, virtual list, dark mode, Explorer integration, tray, hotkey, installer
```

## Build

Rust stable with the MSVC toolchain.

```
cargo build --release
cargo test
```

The release profile uses fat LTO, one codegen unit, `panic=abort`, stripped symbols and a
static CRT.

Developer modes of `nothing-svc.exe`:

```
nothing-svc --scan C: --bench "chernobyl" "*.zip"   # elevated: one-shot MFT scan + query timings
nothing-svc --console [--for SECS]                   # elevated: run the engine in a console
nothing-svc --walk C:\Users\me --serve               # unelevated: directory-walk index, serves the UI
```

## Settings

`%APPDATA%\Nothing\settings.ini` holds the window position, column widths, sort and
search options, and the hotkey (`[hotkey] modifiers=` Alt 1, Ctrl 2, Shift 4, Win 8;
`key=` virtual-key code). The theme follows Windows; set `NOTHING_THEME=light|dark` to
override it.

## Limits (v1)

- Local NTFS volumes with a drive letter only (no FAT/exFAT/ReFS/network).
- Folder sizes are not computed.
- Like Everything, the SYSTEM indexer can show names of files your account cannot open.
  That is fine on a single-user PC.
- No regex, `size:` or `dm:` filters yet.
