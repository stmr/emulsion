## About

Maintained fork of [ArturKovacs/emulsion](https://github.com/ArturKovacs/emulsion), which is
discontinued. Emulsion is a fast, minimalistic image viewer for Windows, macOS, and Linux.
A note for Linux users: Wayland is less tested than X (older releases had high CPU usage and no
title text there); X is fully supported.

macOS builds are Universal 2 (Apple Silicon + Intel), require macOS 11 or newer, and are not
notarized: right-click the app and choose Open on first launch.

### macOS specifics

- Borderless window: only the image, plus the traffic lights that appear when the pointer
  reaches the top-left corner and fade 800 ms after it leaves. The top edge drags the window.
- The image controls (scale, folder position, gamma, theme, help) live in an overlay that
  appears near the bottom edge. `window.show_bottom_bar = true` pins it, `false` disables it.
- Mouse Back / Mouse Forward step to the previous / next image, and so does a horizontal
  swipe or thumb wheel (MX Master and trackpads included). Mice whose driver sends a keystroke
  work too: `BrowserBack`/`BrowserForward` and `Cmd+[` / `Cmd+]` are bound to the same actions.
- Logitech Options+ note: the MX Master thumb buttons ship assigned to the *Back*/*Forward*
  **gesture** (`OSX_GESTURE_BACK`), which macOS delivers as a swipe rather than a mouse button.
  Emulsion follows that swipe, but if you prefer real buttons, set both thumb buttons to
  "Keystroke assignment" with `Cmd+[` and `Cmd+]`.
- Nothing happening at all? Run
  `RUST_LOG=gelatin=trace /Applications/Emulsion.app/Contents/MacOS/emulsion image.jpg` and press
  the button: whatever `MouseInput`/`KeyboardInput`/`MouseWheel` line appears can be bound in
  `cfg.toml` under `[bindings]`.
- The red button closes the window and leaves the app running; set `window.quit_on_close = true`
  to quit instead.
- Appearance follows the system theme until you pick a theme in the app.
- `T` cycles the folder filmstrip: off, revealed with the controls, or pinned. Click a cell to
  jump to that image. `window.show_thumbnails = "off" | "auto" | "pinned"` sets the startup
  state and `window.thumbnail_size` the cell size in points (default 96).
- Scrolling depends on what is under the pointer: the image zooms, the gamma slider changes
  gamma, the folder bar and the filmstrip step through the images.
- Middle click on the image toggles between 1:1 and your fit mode (Fill or Best fit) without
  changing it. `F` fits, `E` is best fit (original size for small images), `Q` is 1:1.
- HEIC/HEIF (iPhone photos) and AVIF open through the system's ImageIO, and camera RAW files
  (CR2, CR3, NEF, ARW, DNG, RAF, ORF, RW2, PEF, SRW) show their embedded preview. On Windows
  and Linux RAW files are not listed.
- Colour managed for wide-gamut screens: the window is Display P3, images with an embedded
  profile (Display P3, Adobe RGB, ...) keep their colours, untagged images are treated as sRGB.

### Key bindings

Bindings in `cfg.toml` under `[bindings]` may carry modifiers: `Alt`/`Option`, `Ctrl`/`Control`,
`Cmd`/`Super`/`Logo`, `Shift` and `CmdCtrl` (Cmd on macOS, Ctrl elsewhere). A binding with a
modifier fires only with it held; a binding without `Shift+` also fires with Shift held, since
Shift is often needed to type the key. `Ctrl++` binds the plus key. A binding with an unknown
modifier is ignored and logged.

### Gamma

`[` or `-` darkens, `]` or `+` brightens, `\` resets, all in 0.1 steps between 0.25 and 4.0
(1.0 is neutral). Double-clicking or middle-clicking the gamma slider also resets it, and
scrolling over it changes gamma. Gamma is a display-only transform; files are never modified.
It persists across images for the session; set `image.reset_gamma_on_navigation = true` to
reset it whenever a new image is opened, and `image.gamma = 1.2` to choose the starting value.

### Permissions and making Emulsion the default viewer

Emulsion lists the folder of the image you open so previous/next work, so macOS asks once per
protected location (Desktop, Documents, Downloads, external and network volumes). The grant is
permanent, but it is tied to the app's *code signature*: an ad-hoc signature is only a hash of
the binary, so every new build looks like a different app and the prompt returns.

Give your local builds a stable identity once:

```
./distribution/macos/create_signing_cert.sh   # self-signed, code-signing only, this user only
./distribution/macos/create_dmg.sh            # picks the identity up automatically
```

The designated requirement printed by the build then names the certificate instead of a hash,
and the permission you grant survives every later rebuild. Grant it once, or tick Emulsion in
System Settings → Privacy & Security → Full Disk Access to stop the per-folder prompts.

To make it the default viewer: select an image in Finder, ⌘I, "Open with" → Emulsion → Change
All. Emulsion registers as a `Default` handler for the image types it reads, so it also shows
up first in "Open With".

Contribution is welcome. Feel free to post feature requests, bug reports, and pull requests.

## Building and Installing

Building from source needs the Rust toolchain pinned in `rust-toolchain.toml`.
On macOS, `./distribution/macos/create_dmg.sh` builds a Universal 2 `Emulsion.app` with AVIF
support and packages `Emulsion.dmg`; it needs `cargo-bundle`, `create-dmg`, `meson`, `ninja`,
`nasm`, and `pkg-config`.

In many cases it's a good start to try running `cargo install emulsion`. If that build fails or if emulsion panics on startup, look into the `nix-example/emulsion/default.nix` file and locate `rpathLibs` which lists the libraries that emulsion depends on. Install the dev version of those libraries then try running the build/install again. For example on Ubuntu one can install `libXi` by running

```
sudo apt install libXi-dev
```

For the [Nix Package Manager](https://nixos.wiki/wiki/Nix) users: The Nix expressions found within `nix-example` is in theory able to build a working executable from *a* state of the emulsion source code. There is no guarantee that the built executable will be identical to any released version of emulsion. The Nix expression is provided to find the dependencies and for those who like tinkering with Nix but otherwise I advise against using it.

### Notes about Cargo Features

All packages on the website come with avif support, however it is not a default feature as the dependecies are not trivial to set up. On macOS, AVIF opens through the system decoder even without the feature. If you are bulding from source (eg using `cargo install`) and would like emulsion to open avif files on other platforms, I recommend taking a look at the [release workflow](.github/workflows/release-packages.yml) for steps to install the avif development dependencies.

When installing Emulsion through the Windows installer, Emulsion will have networking enabled and will by default check for updates. However none of the other versions have networking and neither does the default feature-set. This also means that Emulsion will not have networking dependent capabilities when invoking
```
cargo install emulsion
```

To enable such features when installing with cargo, run
```
cargo install emulsion --features=networking
```

## Reporting Bugs

If Emulsion closed unexpectedly please locate the `"panic.txt"` file. This file has a different location depending on the target platform.

- Windows: `%localappdata%\emulsion\data`
- MacOS: `$HOME/Library/Application Support/emulsion`
- Linux: `$XDG_DATA_HOME/emulsion` or `$HOME/.local/share/emulsion`

`panic.txt` is capped at 1 MiB; the previous contents move to `panic.old.txt`.

When posting a bug report please upload the contents of this file to GitHub. If you deem it too large just paste the last panic entry between the rows of equal signs. If there's no `"panic.txt"` file, describe the scenario in which you experienced the faulty behaviour, and steps to reproduce it if you believe that could help.
