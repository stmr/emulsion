## Unreleased

### Fixed
- Very large JPEGs open faster (100 MP: ~427 → ~400 ms): opaque formats skip the transparency
  scan before upload, and large RGB images are converted to RGBA on all cores
- Tests cover the cursor-anchored zoom (also for a widget away from the window corner) and the
  pixel-art switch to real pixels from 4× zoom

## 14.0.1

### Fixed
- Opening an image is faster than in 14.0.0 and v12.3: the opened file is decoded while its
  folder is still being listed instead of after, files with an image extension are no longer
  read during listing, and loading is checked every few ms instead of every 50 ms
  (24 MP JPEG in a 20-file folder: 429 ms in 14.0.0, 239 ms in v12.3, 221 ms now)
- Neighbours are prefetched only after the shown image is ready, so they no longer delay it
- The image cache is capped at 1 GB, bringing memory use back to v12.3's level for photo
  folders

## 14.0.0

Major version because key bindings with modifiers now require them (see Changed), which can
change how an existing `cfg.toml` behaves.

### Added
- `--version` and the macOS About panel show the commit the build was made from, with
  `-dirty` for builds with uncommitted changes
- Middle click on the image toggles between 1:1 and the current fit mode (Fill or Best fit)
  without changing it; middle click on the gamma slider resets gamma
- macOS: HEIC/HEIF (iPhone photos) open and get thumbnails through ImageIO with their
  orientation; camera RAW files (CR2, CR3, NEF, ARW, DNG, RAF, ORF, RW2, PEF, SRW) show their
  embedded preview; AVIF opens without the `avif` feature
- macOS: wide-gamut output. The window is Display P3, so P3 photos keep their colours instead
  of being clipped to sRGB; sRGB/untagged images are mapped on the GPU at no extra cost
- SVGs render at the window's size instead of a fixed 4096 px and re-render sharper when
  zoomed in (up to 16384 px), keeping the current picture on screen meanwhile
- macOS and Windows show a native error dialog when Emulsion can't start (no OpenGL 3.3)
- Scrolling over an overlay acts on that overlay: the gamma slider changes gamma, the folder
  bar and the thumbnail strip step through the images; only the image itself zooms
- macOS: `./distribution/macos/create_signing_cert.sh` creates a local code signing identity;
  `create_dmg.sh` uses it automatically (or `CODESIGN_IDENTITY`), so the Files and Folders
  permission stays granted across rebuilds instead of being re-asked for every ad-hoc build
- macOS: Files and Folders usage descriptions in the bundle, so the permission prompt explains
  that Emulsion lists the folder for previous/next navigation
- `-` / `+` are bound to gamma decrease / increase next to `[` / `]`

### Changed
- New installs fill the window with the image (`E` switches back to best fit)
- macOS: the bundle claims the image types with `LSHandlerRank = Default`, so Emulsion is
  offered first in "Open With" and as the default viewer
- Rust 1.100; resvg/usvg 0.50, muda 0.21, objc2 0.6 (AppKit/Foundation 0.3), and the
  remaining dependencies updated to their latest compatible releases
- Key bindings with modifiers now require them: `Cmd+O`, `Shift+X`, `Control+X` and
  `Super+X` no longer fire on the bare key. Unknown modifiers are logged and the binding is
  ignored; `Ctrl++` binds plus. Bindings without `Shift+` still fire with Shift held
- Releases are built with `--locked` after the test suite passes; CI lints the whole workspace
  on Linux and macOS and checks the AVIF build
- Dropped the `lazy_static`, `backtrace` and `sys-info` dependencies (std `LazyLock`,
  `std::backtrace`, `libc`/`windows-sys` for total memory), unused gelatin API and dead
  commented-out code. No C toolchain is needed any more, so the workspace cross-checks for
  Linux and Windows from a Mac

### Fixed
- macOS: opening an image from Finder works again. The document handling now lives on the
  application delegate (`application:openFile:`), which is where `NSApplication` routes the
  `odoc` Apple Event; the previous Apple Event handler was replaced during launch, so
  LaunchServices reported that Emulsion cannot open files in the Image format
- Double-clicking the gamma slider only resets gamma; the second click no longer also moves
  the slider to the pointer
- Thumbnails were uploaded upside down and ignored EXIF orientation; they now match the image
- Thumbnails decode on up to four workers with a box-sampled downscale, so a strip of twelve
  20 MP JPEGs fills in ~86 ms instead of ~510 ms
- Launching without an image no longer pops an Open dialog over the empty window; use
  File → Open
- SVGs render with correct transparency; semi-transparent areas and anti-aliased edges were
  darkened (premultiplied output read as straight alpha)
- SVGs smaller than 512 bytes, or with an upper-case `.SVG` extension, open again
- SVG thumbnails appear in the filmstrip; the current image's cell no longer turns into a
  solid yellow block while its thumbnail is missing
- Icons, labels, thumbnails and the help screen no longer darken their translucent edges
  (alpha was applied twice)
- System fonts are scanned once per session instead of for every SVG
- Downscaled icons, labels, thumbnails and the help screen sample their mipmaps instead of
  aliasing on non-Retina and fractionally scaled displays
- The new-version button and `--help` point to this fork instead of the discontinued
  upstream site; a failure to open the browser is logged instead of crashing
- macOS: colours are no longer oversaturated on wide-gamut screens. The window was untagged,
  so sRGB pixels were shown as P3 and skin tones leaned red; it is now tagged and colour-matched
- Images with an embedded ICC profile (Display P3, Adobe RGB, ...) are converted to the window
  gamut, in the main view and the thumbnails (~7 ms for 24 MP; sRGB and untagged files skip it).
  A profile is only skipped when both its primaries and its tone curve match, so DCI-P3
  (gamma 2.6) or gamma-2.2 "sRGB" files are converted correctly
- Crashes: dropping, hovering or opening a non-image file next to images; empty folders;
  a decoder panic (the image now shows an error instead of loading forever); a failed
  neighbour prefetch during slideshows; zooming before the first image is shown; missing
  OpenGL 3.3 (now a clear error); a full disk when saving the cache on quit
- The cache is written atomically, a broken cache no longer shows the first-launch help, and
  config/cache parse errors are logged with the file and reason. An `[updates]` section
  without `check_updates` no longer discards the whole config
- The update check times out after 10 s and never delays quitting
- CPU: loader threads and the folder watcher no longer poll or spin, animations no longer
  busy-wait before each frame (a 10 fps GIF went from ~9% to ~2% CPU), and pointer moves no
  longer redraw the whole window because of the title or thumbnail strip
- Cache: focus changes and deletes keep the decoded neighbours, eviction keeps the images
  closest to the current one, prefetch looks both ways and honours the memory budget for
  huge images, and a file changed on disk is reloaded
- Zoomed-out images use a full mipmap chain with smooth level blending (no shimmer or
  popping), and transparent images no longer get dark fringes when minified
- 16-bit images are colour-converted at 16 bits; HDR/EXR images are tone-mapped and
  sRGB-encoded instead of clipped and too dark
- Keys no longer trigger two actions on non-US layouts (AZERTY); custom commands run in the
  background instead of freezing the window
- The thumbnail strip refreshes when switching to a folder with the same number of images,
  and evicted thumbnails load again when they scroll back into view
- Triple click no longer toggles fullscreen twice; scroll zoom anchors on the cursor
- Redraw on display scale changes; no 1 px drift at fractional scales
- Linux: the Wayland app id / X11 `WM_CLASS` are set again and the Wayland-only passes run
  only on Wayland
- `panic.txt` is capped at 1 MiB; clipboard copy of rotated images uses less memory
- Windows installer: VC++ runtime installs silently and is verified, uninstall removes
  everything; macOS: `create_signing_cert.sh` works with the stock LibreSSL `openssl`
- Prefetching an animation decodes only its first frame; the rest loads when it is shown.
  Requests that are no longer wanted stop decoding and free their slot right away
- Folder listing runs in the background, so large or network folders no longer freeze the
  window; file order is deterministic
- Moving an image to the Trash no longer blocks the window, menu and key use the same delete
  and copy code, and a failed delete shows the on-screen failure icon
- Keyboard panning is no longer twice as fast on Retina displays
- Windows/Linux: camera RAW files are no longer listed, since they can't be decoded there
- A failed move to Trash shows its own "Failed to Delete" notification

## 13.0.0

First release of the `stmr/emulsion` fork (upstream is discontinued).

### Added
- macOS: native menu bar (App, File, Edit, View, Window, Help) that dispatches the same
  actions as the configurable key bindings
- macOS: Finder integration (document types in the bundle, `odoc`/`rapp` Apple Events,
  opening an image in the running window, native Open dialog when launched with no image)
- macOS: borderless window with the real traffic lights, revealed on pointer proximity in the
  top-left corner and hidden again after 800 ms; slim top strip drags the window
- macOS: `quit_on_close` config option; by default the red button closes the window and the
  app stays running
- macOS: Mouse Back / Mouse Forward navigate to the previous / next image
- Nondestructive gamma (0.25–4.0, neutral 1.0) with a logarithmic slider centered on 1.0,
  `[` / `]` / `\` bindings, double-click reset, and the `image.gamma` /
  `image.reset_gamma_on_navigation` config options
- Appearance follows the desktop theme on new installs; switching the theme in the app keeps
  the manual choice
- Universal 2 (arm64 + x86_64) macOS build script with AVIF, pinned deployment target 11.0,
  ad-hoc signing, and DMG packaging: `./distribution/macos/create_dmg.sh`
- Horizontal swipe / thumb-wheel navigation, so Logitech's Back/Forward *gesture* assignment
  (the MX Master default) steps through the folder; `BrowserBack`/`BrowserForward` and
  `Cmd+[` / `Cmd+]` are bound to the same actions for drivers that send keystrokes
- Optional folder filmstrip: `T` (or View → Cycle Thumbnails) cycles off → auto-hide → pinned,
  the mode is remembered, cells are 96 pt by default (`window.thumbnail_size`), decoding happens
  off the UI thread, and the cache is capped at `min(5% of RAM, 512 MiB)`

### Changed
- The bottom bar is now an overlay above the image: hidden by default, revealed near the
  bottom edge, hidden again after 800 ms. `window.show_bottom_bar = true` pins it,
  `false` disables it entirely
- Bundle identifier is now `io.github.stmr.emulsion` and the bundle uses the `.icns` icon
- Rust 2024 edition, pinned toolchain (`rust-toolchain.toml`), refreshed dependencies
  (thiserror 2, rand 0.10, toml 1, trash 5, resvg/usvg 0.48, kamadak-exif 0.6, ureq 3,
  derive_builder 0.20)
- Release workflow publishes only on `v*` tags and uploads build artifacts otherwise

## 12.0 on 2026-04-02

### Changed
- Windows: Fix for an issue where the window appeared with a briefly white background before the UI was loaded

## 11.0 on 2024-05-05

### Added
- If emulsion was closed while being maximized, then next it will start up maximized
- If emulsion fails to load an image, then the image path will be shown in the title bar regardless (along with a message "FAILED TO OPEN")

### Changed
- **Breaking**: The name of some keys (used for key bindings) have changed. As of writing this, a list of all supported key names can be found in the `virtual_keycode_to_string` function in the source code.
- **Breaking**: Dropped support for systems that don't have at least OpenGL 3.3 (In other words: from now on Emulsion requires OpenGL 3.3 to run. Which is more than 10 years old by now)
- Emulsion will now make a better attempt at placing new windows to the same location where the last window was closed. (This mostly affects muti-monitor setups)

## 10.3 on 2022-10-23

No change is made to emulsion, this release is only made to fix errors
in the build system and to re-trigger building the distributables

## 10.2 on 2022-10-16

No change is made to emulsion, this release is only made to fix errors
in the build system and to re-trigger building the distributables

## 10.1 on 2022-10-15

No change is made to emulsion, this release is only made to fix errors
in the build system and to re-trigger building the distributables

## 10.0 on 2022-10-15

### Added
- Added configuration keyword to switch between dark and light mode.
- Added `pan_vert` and `pan_hor` as input bindings to pan with the mouse only vertically or horizontally

### Changed
- Fix for not being able to delete images on some systems.

## 9.0 on 2021-04-27

### Added
- Added `start_maximized` configuration field
- Some very high resolution images that were shown entirely black, are now shown correctly.
- Allow copying to the clipboard on Wayland

### Changed
- Fixed spurious crashes on Wayland
- Fixed incorrect file association method in the Windows installer
- Fixed a bug that caused the entire system to freeze on X11 desktops.
- Fixed some animated gifs showing as a still image
- The parent folder could previously be deleted after deleting the last open image. This is now prevented.

## 8.0 on 2021-01-02

### Changed
- The original scale button now shows "1:1" instead of "1"
- Changed the configuration folder on macOS (run `emulsion -h` to find where it's located)
- The image copied to the clipboard can now be pasted into a wider range of programs on Windows

### Added
- The Windows installer now adds a Start menu entry.
- Added multiple configuration fields: https://github.com/ArturKovacs/emulsion/pull/160
- Zooming and camera panning can now be bound to keyboard input.

## 7.2 on 2020-11-30

### Changed
- Fixed the issue that the first image was shown from the folder after deleting an image instead of the next.
- Fixed a crash for when trying to open an image with invalid EXIF orientation.

## 6.0 on 2020-10-08

### Added
- The currently shown image can now be copied to the clipboard with a keyboard shortcut.
- Image files without a filename extension can now be opened.
- The image antialiasing (filtering) mode can now be manually toggled.

### Changed
- Fixed an issue that caused the images to be loaded from the hard drive even if they were already loaded and avaiable in the cache.
- Fixed an issue that prevented some images from correctly fitting inside the viewport.

## 5.0 on 2020-08-17

### Added

- AVIF support
- Supported MIME types are now added to the `.destkop` file for the linux release; so the file manager should offer up Emulsion for any supported filetype before applications that don't support said filetype.

### Changed

- Panning can now be done with Left Click (instead of Right Click)
- Now there are limits on panning and zooming to avoid getting the image "too far from the view area"
- Some images that were shown with an incorrect orientation are now shown corrently.
- Opening a file through the CLI in the current folder does not need a leading dot-slash (./) anymore
- Fixed an issue where sometimes one image would get stuck preventing the user from switching to another image from the folder.

## 4.0 on 2020-06-23

### Changed
- Image files are now ordered case-insensitively

## 3.0 on 2020-06-21

### Added
- Linux .deb package
- Animated PNG support
- Support for opening directories (both drag & drop and cli argument)
- Best fit mode that displays images at their original size when they fit into the window instead of stretching them.

### Changed
- Changed the UI layout to accomodate the scaling mode buttons.
- Directory contents and the current file is refreshed when the Emulsion window gains focus.
- ~~Image files are now ordered case-insensitively~~
- Significantly decreased CPU and GPU usage.
- Instead of the description, the program name is shown in many context on Windows.
- Many images that couldn't be opened with Emulsion now can be.
- The '1' key can now by default be used for setting the scaling mode to "original size"
- The Return key can now by default be used to toogle full-screen mode.

## 2.1 on 2020-05-23

### Added
- Custom commands to execute on the current image.
- Command line arguments on Linux/macOS to print the current version and the search location of the config file.

### Changed
- Better display quality for certain large images.

## 2.0 on 2020-05-02

### Added
- Animated gif support.
- Custom key-bindings.
- Config entry to disable update-checks.
- The playback or presentation state is now displayed in the titlebar.

### Changed
- Pressing escape in full-screen mode will go back to windowed mode instead of exiting.
- Improved legibility of the info screen.
- Made the dark shade even darker.
- When compiling from source, networking is an optional feature.

### Fixed
- The windows installer won't replace the Adobe Reader DC icon.
- Fixed hang when starting up on Wayland.

## 1.9 on 2020-04-21

Baseline for the changelog.
