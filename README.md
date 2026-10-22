# photo_copy

A desktop app for consolidating photos and videos into one organized library without losing anything.

New photos land in a **staging** folder (phone exports, old drives, copies from other computers).
The app finds what you already have, helps you remove duplicates, and moves the rest into the
right subfolder of your **destination** library (`2024/`, `2025/trip/`, ...). Duplicates are found
by content, not by name, including copies of the same photo whose metadata differs.

> **Nothing happens automatically.** Every scan, check, move and removal is an explicit action,
> destructive actions ask for confirmation, and "remove" never deletes: files go to a `.trash`
> folder you empty yourself.

## How it works

The app follows the workflow as three steps, shown as a stepper at the top. Steps guide, they
never block.

### 1. Folders

Choose the destination (your organized library) and the staging folder (new photos), then
**Index files**. Indexing reads every file once and stores its hashes in a small index inside the
folder; later **Rescans** only read new or changed files.

From here you can also **Check .trash...**: it verifies that every file you trashed from staging
still has a copy in the destination, and shows thumbnails of those that do not, so you know it is
safe to empty `staging/.trash`.

### 2. Duplicates

Three lists, each with a count (amber), a check (green) or a dash (not checked yet):

- **Destination**: duplicate groups inside the library.
- **Staging**: duplicate groups inside staging.
- **Staging vs destination**: staging files you already have in the library. Runs when you click
  *Check staging against destination*, then refreshes itself.

Each list shows **exact duplicates** first, then **same photo, different metadata**. Click a
thumbnail for a large preview of every copy; for same-photo groups a *What differs* table lists
the EXIF fields that differ (date, GPS, export IDs, ...) so you can keep the richer copy.

*Trash all N from staging...* removes every staging file that is already in the destination in
one go. Each file is re-checked on disk first, so an out-of-date index can never cause a loss;
every decision is written to a CSV log in `staging/.trash/`.

### 3. Organize

A photo browser for staging and destination: thumbnail grid or a Finder-like list (name, date
taken, size, kind), sortable by column. Select several photos and **Move N to destination
folder...**, or double-click one to view it, step through with the arrow keys, move it, or
**Trash...** it to discard photos you do not want to keep.

## How duplicates are detected

Every file gets two hashes from a single read:

| Hash | Covers | A match means |
|---|---|---|
| **File hash** (BLAKE3) | every byte | the files are identical and interchangeable |
| **Image hash** | the encoded image or video only, without metadata | the same photo or video, possibly with other metadata |

The image hash skips EXIF (including Apple's MakerNote), XMP, comments, embedded thumbnails,
HDR gain maps, iPhone timed-metadata tracks (Live Photo ID, location) and creation dates. It
always includes the orientation (EXIF orientation, HEIF rotation, video display matrix), so two
copies that might display rotated differently never match.

| Format | Image hash |
|---|---|
| JPEG, PNG | yes |
| HEIC / HEIF (including tiled iPhone photos) | yes |
| MOV, MP4, M4V | yes (not fragmented MP4) |
| GIF, WebP, BMP, TIFF, AVI | file hash only |

It was validated against exiftool's `ImageDataHash` on a real iPhone library of ~4,700 files:
the same 670 groups (634 photos, 36 videos).

**Not detected:** re-encoded copies (HEIC converted to JPEG, resized or edited versions) and a MOV
and an MP4 made from the same recording. These have different image data.

## Safety

- **No deletion.** Trashed files are moved to `.trash/` inside their folder, keeping their folder
  structure. Emptying it is up to you.
- **No overwriting.** Moves copy the file, verify the copy's hash, then remove the original. A
  name that already exists is never replaced: single moves ask for a new name, batch moves skip
  the file and report it.
- **Verified bulk trash.** Before a staging file is trashed as "already in the destination", both
  files are re-hashed from disk.
- **Logs.** Bulk trashing writes `staging/.trash/trash-log-<date>_<time>.csv`, one row per file,
  as it happens.

## Files the app creates

| Where | What |
|---|---|
| `<folder>/.photo_index.json` | the index: relative path, size, modification time, file hash and image hash per file. Written atomically. |
| `<folder>/.trash/` | trashed files and bulk-trash logs |
| app data folder (`~/Library/Application Support/Photo-Copy` on macOS) | the last used folders |

Scans ignore hidden files and folders (names starting with `.`), which covers the index and
`.trash`, plus system folders such as `lost+found` and `$RECYCLE.BIN`.

## Building and running

Requires [Rust](https://rustup.rs) (stable, 1.82 or newer).

```sh
cargo run --release -p photo_copy_app     # the desktop app
cargo build --release -p photo_copy_cli   # the CLI, at target/release/photo-copy
```

To try it without touching your photos, point it at the sample folders in `demo/`
(`demo/library` as destination, `demo/staging` as staging).

### Platforms

The app has only been used and tested on **macOS**, where HEIC thumbnails use `sips` (built in).

The code compiles for **Linux** and **Windows**, but the app has never been run on either, so
expect rough edges. What is known so far:

- **Linux:** HEIC thumbnails need `heif-convert` (`apt install libheif-examples`). The folder
  picker relies on `xdg-desktop-portal` with a file chooser backend; without one, *Choose folder*
  does nothing. Open / Show in folder use `xdg-open`. File names must be valid UTF-8.
- **Windows:** opening files and showing them in Explorer is not implemented, joined paths can mix
  `\` and `/`, a console window may appear next to the app, and there are no HEIC thumbnails.
  Building `blake3` needs the MSVC toolchain.

## Command line

The CLI is a thin wrapper over the same core library, useful for scripting and for checking
results on a real library.

```text
photo-copy index <folder>              build/update the hash index of a folder
photo-copy dups <folder>               list duplicate and same-photo groups (uses the saved index)
photo-copy compare <staging> <library> check staging files against the library (uses saved indexes)
photo-copy trash <folder> <rel-path>   move a file to <folder>/.trash
photo-copy move <file> <dest-folder>   copy+verify+remove a file into a folder (never overwrites)
photo-copy hash <file>...              print the file hash and the image hash (- if none)
```

## Languages

English (default) and Brazilian Portuguese, switchable with the EN / PT-BR selector at the top
right. The choice is not remembered.

## Project layout

A Cargo workspace with three crates:

| Crate | Contents |
|---|---|
| `crates/core` | library without UI: scanning, hashing (`image_hash.rs`), index, duplicate groups, staging comparison, safe move and trash, verified bulk trash, trash check, thumbnails, EXIF dates |
| `crates/app` | the egui desktop app; only calls `core` |
| `crates/cli` | the command-line wrapper |

## Development

```sh
cargo test --workspace                     # core logic is tested with temporary folders
cargo clippy --workspace --all-targets
cargo fmt
```

UI texts are translated through `tr("...")` / `trf("...", ...)` in `crates/app/src/i18n.rs`,
with the English text as the key. A test fails if a key in the sources has no pt-BR entry or its
`{placeholders}` differ.

## Future improvements

Contributions are welcome. Known gaps and ideas, roughly by area:

**Platforms**
- Run and test the app on Linux and Windows, and fix what comes up (see [Platforms](#platforms)).
- Windows: open and reveal files through Explorer, normalise path separators, hide the console
  window, report HEIC as unsupported instead of failing.
- Linux: a typed path field as a fallback when no file-chooser portal is available; document the
  runtime packages (libxkbcommon, libwayland/libx11, fontconfig, a Vulkan or OpenGL driver,
  xdg-desktop-portal, xdg-utils, libheif-examples); support non-UTF-8 file names.
- Continuous integration (tests on macOS, Linux and Windows).

**Browsing and previews**
- Video thumbnails and playback in the viewer. Videos show a placeholder tile today; right-click ›
  Open plays them in the default app.
- A disk cache for thumbnails (they are kept in memory only, so large folders decode again after a
  restart).
- A folder tree next to the breadcrumbs in Organize.
- Trash from the right-click menu and the selection bar (several photos at once), and a keyboard
  shortcut for Trash in the viewer.
- Remember the language, the list sort order and the grid/list choice between runs; detect the
  system language.

**Duplicate detection**
- Visually similar photos (a perceptual hash), to catch copies the exact hashes cannot: HEIC
  converted to JPEG, resized or re-compressed copies, a MOV and an MP4 of the same recording.
  These need human review, so they would be a separate list.
- A rule for "same photo, different metadata" groups, such as *keep the copy with the most
  metadata*, so hundreds of re-exported pairs do not have to be reviewed one by one.
- Image hashes for GIF, WebP, TIFF and AVI, and for fragmented MP4.
- Show XMP and video metadata (QuickTime keys such as location and creation date) in the
  *What differs* table, which reads EXIF only today.

**Safety and robustness**
- Restore a file from `.trash` to its original place from inside the app.
- Protect newer indexes from older versions of the app: an older version that rescans a folder
  rewrites its index without the image hashes, and the newer version then does not know they are
  missing.
- Faster first indexing of large libraries on external drives. The app is designed for hundreds to
  a few thousand files per folder.

## License

MIT: free to use, modify and share. See [LICENSE](LICENSE).
