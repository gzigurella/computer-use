# Dependency BOM

## Scope

This inventory describes `computer-use-linux` 0.1.0. The machine-readable Rust dependency graph is [dependency-bom.cdx.json](dependency-bom.cdx.json), CycloneDX 1.6, generated from `Cargo.lock` and the Linux-target `cargo tree`. It contains 117 Cargo libraries, including transitive and procedural-macro dependencies. The lockfile is the source of truth for exact versions and licenses.

CI audits the Rust lockfile with `cargo-audit`. The 2026-10-31 audit found no vulnerability advisories. It reports three informational unmaintained-crate advisories for `zbus_names`, `zvariant`, and `zvariant_derive`; they are pulled in by `atspi` 0.30 through zbus 5. RustSec documents these types as merged into zbus 6 ([names](https://rustsec.org/advisories/RUSTSEC-2026-0371), [wire format](https://rustsec.org/advisories/RUSTSEC-2026-0370), and [derive macros](https://rustsec.org/advisories/RUSTSEC-2026-0372)). Removing them requires an AT-SPI release compatible with zbus 6 or replacing the current AT-SPI adapter; upgrading only the direct zbus dependency would leave the old crates in the graph.

The CycloneDX metadata records the target, manifest default features, and resolved root features. Empty feature arrays identify the default Hyprland-only profile; optional backend features are omitted from that artifact.

Refresh it after changing Rust dependencies with:

```sh
python3 scripts/generate-dependency-bom.py
```

CI checks that the committed graph matches `Cargo.lock` with `python3 scripts/generate-dependency-bom.py --check`; only the generated timestamp is ignored.

## Rust application dependencies

| Direct crate | Locked version | Role |
|---|---:|---|
| `base64` | 0.22.1 | MCP screenshot payloads |
| `futures-util` | 0.3.34 | asynchronous streams and cancellation |
| `jpeg-encoder` | 0.6.1 | in-process Hyprland screenshot encoding |
| `memmap2` | 0.9.12 | shared-memory screencopy buffers |
| `serde` / `serde_json` | 1.0.229 / 1.0.151 | MCP, compositor and IPC data |
| `tokio` (`io-std`, `macros`, `net`, `rt-multi-thread`, `signal`, `sync`, `time`) | 1.53.3 | async runtime, I/O, networking, signals and coordination; `fs` and `process` are enabled transitively by `zbus` |
| `uuid` (`v4`) | 1.26.0 | session and observation IDs |
| `zbus` (`tokio`) | 5.19.0 | desktop portal and D-Bus calls |
| `wayland-client` | 0.31.15 | native Wayland overlay, screencopy, and virtual-input clients |
| `wayland-protocols` (`client`, `staging`) | 0.32.13 | in-process ext-data-control clipboard and output-scale viewport protocols |
| `wayland-protocols-misc` (`client`) | 0.3.12 | virtual keyboard protocol |
| `wayland-protocols-wlr` (`client`) | 0.3.12 | layer-shell overlay and screencopy protocol |
| `atspi` (`proxies`, `tokio`) | 0.30.0 | in-process AT-SPI2 D-Bus proxies; no Python/GI runtime |

The current default `x86_64-unknown-linux-musl` release artifact contains only the Hyprland backend and is a static PIE, measured at 8,354,120 bytes in the 2026-10-31 local release build, with no ELF `DT_NEEDED` entries or interpreter. It embeds 293 XKB configuration files plus the license notice (from `xkeyboard-config` 2.48-1 on this build host). Rust crate code is linked into the executable; it is not shipped as a collection of `.so` files. This removes the GNU dynamic loader and C runtime requirement for that artifact.

Some optional features load libraries with `dlopen`, so `DT_NEEDED` and `ldd` alone do not reveal them:

| Library | Used by | Scope |
|---|---|---|
| `libei.so.1` | portal EIS input | optional portal backend |
| `libX11.so.6`, `libXtst.so.6` | X11 capture/input support | optional X11 backend |
| `libxkbcommon.so.0` | GNU build: validating a matching compositor keymap or compiling active Hyprland XKB settings | loaded with `dlopen`, not an ELF dependency; static builds use the Rust XKB compiler |
| XKB data (`xkeyboard-config`) | Static musl build-time input for standard `evdev`/`pc105` layouts | 293 XKB configuration files bundled into the executable with the license notice at `licenses/xkeyboard-config-COPYING`, then extracted to a private temporary directory at startup; normal shutdown removes it and the next startup reaps leftovers from killed processes; no runtime package required; custom rules/models fall back to embedded US |

The wlroots virtual-pointer protocol now uses the existing Rust Wayland client, so it adds no `libwayland-client.so.0` runtime dependency.

## Host runtime dependencies by feature

| Feature / backend | External runtime requirements |
|---|---|
| Hyprland native capture and control | access to the active Wayland and Hyprland IPC sockets; no capture helper required |
| Hyprland visual pointer overlay | compositor advertising `zwlr_layer_shell_v1`, `wl_shm`, and `wp_viewporter`; optional visual aid |
| Hyprland Grim fallback | `grim`; helper deadlines and process-group cleanup are supervised in-process |
| Hyprland virtual pointer | a compositor advertising `zwlr_virtual_pointer_v1` |
| Hyprland virtual keyboard / Unicode | native `zwp_virtual_keyboard_v1` client when exposed; GNU builds use `libxkbcommon.so.0` for active-layout mapping, accepting a compositor keymap only when the active group name matches Hyprland and its serialized map matches locally compiled settings when available; static builds use the Rust compiler with bundled standard XKB data, then US fallback; literal text uses the native keymap path; optional `wtype` fallback |
| X11 implementation | opt-in `backend-x11`; without it, X11/XTest loading, capture, and X11 route actions are excluded |
| Sway adapter | opt-in `backend-sway`; without it, Sway IPC layout, focus, and pointer helpers are excluded |
| Portal backend | opt-in `backend-portal`; without it, portal requests, PipeWire/GStreamer capture, and EIS support are excluded |
| Clipboard | native ext-data-control protocol when exposed; `wl-copy`/`wl-paste` fallback for other Wayland compositors; X11 uses `xclip`/`xsel` |
| AT-SPI tree and actions | session D-Bus, desktop AT-SPI bridge, and the application's accessibility implementation; optional |
| Generic wlroots output geometry | `wlr-randr`; optional outside Hyprland/Sway |
| Sway fallback | `swaymsg`; `wtype` for virtual keyboard when supported |
| Wayland portal | session D-Bus, `xdg-desktop-portal` plus a desktop backend, PipeWire, GStreamer tools and plugins (`pipewiresrc`, `videoconvert`, `videoscale`, JPEG encoder); `compositor` plugin for multi-stream composition; optional `libei` for EIS |
| X11 | X server; dynamically loaded X11/XTest libraries; optional `xdotool`, `xclip` or `xsel`, and either FFmpeg `x11grab` or ImageMagick `import` for capture |
| Build | Rust toolchain/Cargo and the locked crates; `cc` may be used by crate build scripts |

## Self-contained binary assessment

The current artifact is one executable, but not a standalone desktop runtime. The supported Hyprland path uses a native AT-SPI2 client; its external accessibility contract is the desktop D-Bus service, toolkit bridge, and each application's accessible implementation.

A build with Rust target `x86_64-unknown-linux-musl` produces a static-PIE ELF with no `DT_NEEDED` entries or interpreter. The previous Hyprland-only artifact measured 4,823,368 bytes. Static musl Hyprland CI passed three standard GTK and three Qt smoke runs using the Rust XKB backend, including Unicode input, modifier chords, Alt/Super, keypad keys, clipboard restoration, and live `de→fr→it→us` layout refresh; the third run in each series blocks `libxkbcommon`. A fourth isolated GTK run requested a 10-bit 3840×2160 headless output, observed `XRGB2101010`, and verified that native capture produced a JPEG downsampled to a 3200-pixel long edge. Firefox 159 also passed the optional Hyprland browser smoke: its 400-node, 16-level AT-SPI tree exposed page controls and mapped bounds; invoking the button action changed the page title. Live smoke skips clipboard checks if the original contents are non-text. A separate check ran a native session with a visible GTK editable control while the server's `PATH` was empty. The current Rust unit suite passes all 42 tests, including Hyprland 8-bit/10-bit screencopy conversion, versioned screencopy buffer enumeration, screenshot downsampling, and styled-pointer size checks.

This makes the Hyprland executable itself static and moves capture, virtual pointer/keyboard, clipboard, Hyprland IPC, and AT-SPI2 client logic into the binary. Active-layout mapping uses `libxkbcommon` in GNU builds and the pure Rust compiler plus build-time bundled XKB data in static builds; the bundle is extracted to a private temporary directory when the process starts. The XKB dataset and its version are build inputs and should be pinned or recorded for reproducible releases. Custom XKB rules/models and builds without XKB source data retain an embedded US fallback. The native route also relies on the session accessibility bus and toolkit bridges; `wtype` and `grim` support fallback routes, while `wl-copy`/`wl-paste` remain optional integrations for other compositors. A no-install, one-file desktop runtime is not complete because those desktop services, writable temporary storage, and fallback integrations remain host responsibilities.

| Priority | Improvement | Result / trade-off |
|---|---|---|
| 1 | Keep optional X11, portal, and Sway support feature-gated | The default Hyprland release excludes them; other variants remain opt-in and need their own support validation. A universal Linux build would still need host integrations. |
| 2 | Add AT-SPI integration coverage for more toolkits and bridge implementations | GTK 4 and Qt 6 pass in the Hyprland CI matrix, and Firefox 159 passes the optional browser smoke; other toolkits and bridge versions still need representative coverage. |

Even after these changes, the binary must run inside a compatible Linux session and use the compositor's Wayland socket. That operating-system/compositor contract cannot be embedded in an application binary. A realistic target is a single static executable with no helper-program or Python requirement for the Hyprland path, while retaining system services and the compositor as host dependencies.
