# Desktop browser

The `browser-desktop` crate provides one Rust desktop shell for Windows, Linux, and macOS.
[Rune](https://github.com/anomalyco/rune) owns its window, controls, text input, and GPU
presentation. The browser's existing Rust engine renders page content; Lumen runs page
JavaScript. Rune is contained within the desktop crate so another toolkit can replace it
without changing the browser engine.

The original Swift/AppKit shell remains available on macOS via `scripts/build.sh`.

## Build and launch

Use **Rust 1.99.1 or newer** for the desktop crate. Rune is pinned to revision
`1dba3423fd1f1904553422e7814700f817abe8af`; the workspace's Rune-related dependency patches
must use the same revision when updating it.

From the repository root:

```sh
cargo run --locked -p browser-desktop
cargo run --locked -p browser-desktop -- --url https://example.com
cargo build --locked --release -p browser-desktop
```

The release executable is `target/release/browser` on Linux/macOS and
`target/release/browser.exe` on Windows. Launch scripts accept the same app arguments:

```sh
# Linux or macOS
./scripts/run-browser.sh --url https://example.com
```

```powershell
# Windows PowerShell
.\scripts\run-browser.ps1 --url https://example.com
```

### Windows

Install Rust's default **MSVC** toolchain and the **Desktop development with C++** workload
from Visual Studio Build Tools. Rune uses Direct3D 12 for presentation and can use Windows'
WARP software adapter when no suitable hardware adapter is present. A normal desktop login
is required to open a window.

The GNU Rust target is not the CI target. Building it additionally requires MinGW-w64
binutils; Rune's [installation notes](https://github.com/anomalyco/rune#add-it) describe the
`dlltool` setup.

### Linux

Install the Wayland and XKB development libraries before building. For Debian/Ubuntu:

```sh
sudo apt-get install pkg-config libxkbcommon-dev libwayland-dev \
  libxkbcommon-x11-0 mesa-vulkan-drivers fonts-dejavu-core
```

Fedora uses `libxkbcommon-devel` and `wayland-devel`; Arch uses `libxkbcommon` and `wayland`.
Run the window from an X11 or Wayland desktop session. Rune presents through Vulkan, so
install the Vulkan driver appropriate to your GPU, or Mesa's software Vulkan driver.
The desktop app bundles Inter as its default page font. Installed fonts still provide
fallback glyphs; DejaVu Sans also supports the standalone engine test suite.

A missing `DISPLAY` and `WAYLAND_DISPLAY` means there is no desktop for a window. SSH,
containers, and CI can still run the headless tools below when a Vulkan adapter is available.

### macOS

Install the Xcode command-line tools (`xcode-select --install`) and Rust. The shared desktop
app is a Rust executable and uses Rune's Metal presentation. It does not require building
the separate Swift shell. The Swift shell's existing app-bundle and signing workflow remains
separate.

## Headless inspection

The desktop crate enables Rune's `tooling` feature, including in release builds. Rune can
render the shell without opening a window:

```sh
cargo run --locked -p browser-desktop -- --wait-for-page --snap /tmp/browser.png --size 1200x800
cargo run --locked -p browser-desktop -- --wait-for-page --tree /tmp/browser-tree.json
```

Use paths appropriate to the host OS, for example `browser.png` on Windows. A snapshot still
needs a GPU backend or software Vulkan adapter on Linux. Rune's `--script` tooling can drive
controls before taking a snapshot; see the [Rune tooling documentation](https://github.com/anomalyco/rune/blob/dev/crates/rune/README.md#agent-tooling-headless-no-window-same-renderer-fixed-virtual-clock).
The page engine performs real asynchronous work. Use `--wait-for-page` for snapshots and
scripted inspection: it waits for the worker to finish queued page operations, with a
30-second timeout. It cannot guarantee that a page will never start more network work later.
Omit this flag during ordinary interactive browsing so the UI never waits for a page.

## Validation and scope

`.github/workflows/desktop.yml` checks, tests, and builds the desktop package on Windows,
Linux, and macOS, then uploads the executable for each runner. These build checks do not
replace manual checks of windowing, GPU drivers, input methods, and display scaling on each
OS. The binaries are CI artifacts; they are not installers or signed application bundles.
After downloading an artifact on Linux or macOS, restore its executable bit with
`chmod +x browser` before launching.
Each artifact includes the browser MIT license and the bundled Inter font's SIL Open Font
License (`Inter-LICENSE.txt`). Retain those license files when redistributing the binary.
Font provenance is recorded in `crates/desktop/assets/README.md`.

The shared shell uses the same engine and its existing web-platform limitations as the
Swift app. JavaScript execution has a 20-second deadline, including timer and promise
callbacks. Closing a tab interrupts its running JavaScript without blocking the desktop UI.
After a deadline, reload the page to resume scripting; the last rendered document remains
visible. Native host operations use their own timeouts, and JavaScript heap-byte limits
are not yet available. See the [Lumen patch notes](../vendor/lumen/README.browser.md).

Engine-only builds can omit the desktop toolkit and its system libraries:

```sh
cargo build --locked --workspace --exclude browser-desktop
cargo test --locked --workspace --exclude browser-desktop
```
