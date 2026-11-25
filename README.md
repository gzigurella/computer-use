# computer-use-linux

A local, harness-independent MCP server for Linux desktop inspection and control.

The only supported Linux desktop target today is **Hyprland on Wayland**. Its preferred capture backend uses native wlroots screencopy and in-process JPEG encoding; `grim` remains an optional fallback. Other targets may exist in the code and can appear in capability discovery, but they are experimental and are not a supported product target. `computer_status` reports what the current Login Session exposes; a detected capability does not imply support for every desktop or compositor.

Terms such as Target, Backend, Output, Session and Observation have specific meanings in this project; see [GLOSSARY.md](GLOSSARY.md) and [docs/adr](docs/adr).

## Support and roadmap

The server uses one MCP API with selectable backend implementations. The project will focus on making the Hyprland experience reliable and complete before expanding desktop coverage.

1. **Hyprland support:** finish and maintain end-to-end capture, pointer, keyboard, scrolling, monitor selection, accessibility, packaging, and release checks on Hyprland.
2. **Backend contract:** keep the public API stable and make backend availability and capabilities explicit through `computer_status`.
3. **Linux expansion:** document adapter requirements and invite community contributions for other compositors and desktop environments. Sway, generic wlroots, X11, and portal-only targets remain experimental until they have dedicated support and validation.

There is no promise of universal Linux compatibility; each backend needs its own implementation and verification against the target compositor.

Hyprland input falls back from legacy CLI dispatchers to the current Lua dispatcher API when needed. Held keyboard chords and mouse modifiers use a persistent native Wayland virtual keyboard so modifier state is visible to applications. At session start, the virtual keyboard reads the compositor's compiled `wl_keyboard` keymap and accepts it only when its active group name matches Hyprland's reported active keymap and, when local compilation succeeds, the serialized map matches Hyprland's active XKB settings. Otherwise it compiles those settings with `libxkbcommon`; static builds also have a pure Rust fallback for standard `evdev`/`pc105` rules using XKB data embedded at build time and extracted to a private temporary directory at startup. If neither compiler can reproduce the configured layout, it reports and uses an embedded US keymap. It checks for layout changes before each key chord. Named keys resolve across XKB levels, including automatic Shift and AltGr for symbols such as `[` on an Italian layout; the action result reports automatic modifiers. If a named keysym is absent from the host layout, the virtual keyboard adds it to its private XKB map with a spare extended evdev keycode when available. Keys that still cannot be resolved safely are rejected instead of being sent at a possibly incorrect physical position. Unicode text uses a per-call keymap and does not depend on the active layout. On Hyprland 0.56.2, smoke tests verify capture, pointer movement, left/right/double click, drag selection, Ctrl+A and mixed-modifier chords, F35 and keypad input, Ctrl+click, Unicode text, smooth and discrete scroll, and AT-SPI actions in GTK 4 and native Qt 6 Wayland clients. Both smoke clients check mixed-scale and rotated coordinate mapping on a temporary portrait output at 125% scale; GTK also verifies the overlay keeps a constant physical size as that output changes between 125% and 150% scale.

On Hyprland, AI input uses the shared native pointer. `computer_start` draws a translucent, click-through mouse arrow through the native Wayland layer-shell, shared-memory, and viewporter protocols; the viewporter keeps its physical size consistent across output scales. Both native screencopy and the Grim fallback composite that same arrow into the agent's JPEG, so the model sees the styled AI pointer rather than a separate compositor cursor. Grim uses a PPM frame only for this compositing path and the binary's existing JPEG encoder; ordinary Grim captures remain direct JPEG. It does not need GTK4 or `gtk4-layer-shell`. If multiple outputs are active, pass `output` to choose the output captured and controlled by the session; the overlay appears there too. `overlay_monitor` is still accepted as a deprecated alias. The overlay is only a visual aid and does not provide an independent pointer. Computer Use never hides or changes the native cursor; it can overlap the AI arrow.

For AT-SPI actions, the Hyprland adapter checks the active application's accessible window bounds against compositor geometry. It maps safe window-relative toolkit bounds to screenshot coordinates and withholds control bounds and accessibility actions when the geometry cannot be mapped safely; visual screenshot-based control remains available.

Builds are native to the selected Rust target. Build the server for the same target architecture as the host.

## Build and run

Build on Linux with Rust. The default build contains the Hyprland backend; the experimental X11, Sway, and portal backends are opt-in Cargo features. X11/XTest and xkbcommon are loaded only when their input paths are used. The GNU Hyprland build uses `libxkbcommon.so.0`; static musl builds bundle XKB data from the build host and extract it to a private temporary directory, so `xkb-data` is not required at runtime for standard `evdev`/`pc105` layouts. The bundle includes the XKB data license notice, is removed at normal shutdown, and stale bundles from a killed server are removed at the next startup. Set `XKB_CONFIG_ROOT` at build time to bundle another data tree; record that tree's version when producing releases because it is a build input. If XKB data is missing from both the bundle and runtime system, or custom rules or models cannot be compiled by the Rust backend, input falls back to the embedded US keymap. Literal text still uses the native keymap path. The preferred Hyprland capture path does not require `grim`; helper commands are supervised in-process with deadlines and process-group cleanup; `wlr-randr` is optional for output geometry and pointer input on generic wlroots Grim fallbacks. X11 screenshots use FFmpeg `x11grab` when available and fall back to ImageMagick `import`; portal capture uses GStreamer with PipeWire, `videoscale`, and either `jpegenc` or the libav `avenc_mjpeg` encoder:

```sh
cargo build --release
./target/release/computer-use-linux --stdio
```

The default build excludes the experimental backends and reports that in `computer_status.capabilities.build_features`; explicitly selecting an excluded backend returns a rebuild hint. Enable experimental backends with `--features backend-x11,backend-sway,backend-portal` or select only the backends needed by your build.

For a statically linked x86_64 Linux executable for the Hyprland target:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --locked --release --target x86_64-unknown-linux-musl --bin computer-use-linux
```

This default artifact contains only the supported Hyprland implementation. To include experimental backends, add the corresponding Cargo features (`backend-x11`, `backend-sway`, or `backend-portal`) to the build command.

This emits a static-PIE binary under `target/x86_64-unknown-linux-musl/release/` with no ELF `DT_NEEDED` entries. The supported `hyprland-native` backend implements capture, pointer and keyboard input, clipboard, compositor IPC, and the AT-SPI2 client in-process. It still uses the desktop accessibility bus and the applications' toolkit bridges. Fallback backends may use host desktop helpers; see [DEPENDENCY-BOM.md](DEPENDENCY-BOM.md).

### Use with MCP clients

`scripts/computer-use-mcp.sh` starts the server over stdio. It uses the static release build when present, falls back to `target/release/computer-use-linux`, and honours `COMPUTER_USE_LINUX_BINARY` for any other path or architecture. When the client was not started from the graphical Login Session (for example a systemd service or SSH), it fills in missing desktop variables such as `WAYLAND_DISPLAY` and `HYPRLAND_INSTANCE_SIGNATURE` from `systemctl --user show-environment`. Variables the client already has are left alone.

**Claude Code.** The repository ships a project-scoped [`.mcp.json`](.mcp.json), so running `claude` from this checkout offers the `computer-use-linux` server after you approve it. To make it available in every project instead:

```sh
claude mcp add --scope user computer-use-linux -- /path/to/computer-use/scripts/computer-use-mcp.sh
```

Claude Code exposes the tools as `mcp__computer-use-linux__computer_status`, `…__computer_start`, `…__computer_observe`, `…__computer_act` and `…__computer_stop`. Screenshots arrive as image content blocks.

**Codex.** Add the launcher to `~/.codex/config.toml`:

```toml
[mcp_servers.computer_use_linux]
command = "/path/to/computer-use/scripts/computer-use-mcp.sh"
args = []
```

Set `COMPUTER_USE_LINUX_BINARY` in the MCP entry's `env` table (`env` in `.mcp.json`, or `-e` with `claude mcp add`) to choose another server binary.

## Verify

`scripts/mcp-conformance.py` needs no desktop or Python packages. It checks the stdio server against the MCP schema rules that real clients such as Claude Code enforce: JSON-RPC framing, `initialize` version negotiation and instructions, tool definitions, and well-formed `content` and `structuredContent` in every tool result, including errors. All smoke clients share the same strict validator (`scripts/mcp_result.py`), so they fail on replies a real client would reject.

Run the unit tests, the X11 smoke test on a disposable X server, and the Wayland
smoke test inside a headless Sway session. The smoke tests check capture, input,
and session cleanup. The X11 smoke test uses GTK4 to verify click, keyboard,
text, scroll, and drag selection. The Wayland smoke
test checks click, Enter, Ctrl+A, text, and smooth-scroll delivery in GTK4. It
also verifies that a drag selects text in a GTK text view and that the client's
Wayland trace contains motion between drag press and release.
It needs Sway, GTK4 Python bindings, Foot, Grim, `wlr-randr`, `wtype`, and
`dbus-run-session`. CI runs both experimental backends and checks the build
without optional backends. A separate Ubuntu 26.04 job builds the static musl
default Hyprland-only binary, verifies that it has no dynamic ELF
dependencies or interpreter, then starts an isolated headless Hyprland instance.
The smoke creates a temporary second output in portrait orientation at 125% scale
and exercises output selection, transformed coordinate mapping, and native GTK 4
and Qt 6 input on it. It does not add an output to the developer's running
compositor. It moves the AI pointer through the center and four inset corners
of the rotated output, checking both Hyprland's cursor position and the visual
arrow (with one-pixel tolerance for screenshot-to-logical rounding). During
the live session it also repositions the selected output, verifies that actions
based on the old display layout are rejected, and checks that a fresh
observation updates the display geometry and overlay position. The GTK CI adds
an isolated 3840×2160 headless-output run, requests 10-bit output, reports the
selected framebuffer format, and verifies the captured JPEG is downsampled to
a 3200-pixel long edge. It also checks that the embedded XKB
bundle is private and removed at graceful
shutdown, and that a later server reaps a bundle left by a killed process. The job requires mapped
AT-SPI actions and cross-client plain-text clipboard support, so missing
accessibility or clipboard services fail the smoke instead of silently
degrading to visual-only coverage. Each toolkit runs once with an empty
clipboard and once with sentinel text, verifying cleanup restores each original
state. The smoke also blocks `libxkbcommon` for the server process and checks
that the Rust XKB compiler uses standard XKB data, falling back to embedded US
only when compatible data is unavailable. Static musl GTK 4 and Qt 6
runs both exercise live us/de/fr/it layout refresh; the GNU build uses the
`libxkbcommon` compiler for the same matrix.

```sh
cargo test --locked --bin computer-use-linux
python3 scripts/mcp-conformance.py target/release/computer-use-linux
dbus-run-session -- cargo test --locked --all-features --bin computer-use-linux
cargo build --locked --release --features backend-x11,backend-sway,backend-portal --bin computer-use-linux
xvfb-run --auto-servernum --server-args='-screen 0 1920x1080x24' \
  python3 scripts/x11-smoke.py target/release/computer-use-linux
dbus-run-session -- python3 scripts/wayland-smoke.py target/release/computer-use-linux
```

The live Wayland compositor fallback was verified on Hyprland 0.56.2. CI also
starts a two-output headless Sway compositor and verifies screenshot, pointer,
click, drag movement, Enter, Ctrl+A, typed text, and smooth-scroll delivery in a
focused GTK window on the second output. This checks that screenshot coordinates
map to compositor coordinates across a monitor offset.
For a logged-in Hyprland session, `scripts/hyprland-smoke.py` exercises the
default `auto` route and checks screenshot, click, keyboard, text, scroll, drag,
and cleanup without opening the portal consent picker. Add `--headless-output`
to create and remove a temporary portrait display at 125% scale and verify mixed-
scale, rotated coordinate mapping. The optional `--qt` mode uses PySide6 to
exercise a native
Qt 6 Wayland client; install PySide6 for the Python interpreter running the
script. That receiver checks click, keyboard, Unicode text, scrolling, and drag
selection.
It also verifies text replacement and an indexed AT-SPI action on a visible
control when running against a toolkit that exposes those interfaces.
The nested `scripts/hyprland-ci.sh` path checks both modern and legacy MCP
Streamable HTTP discovery, protocol-header validation, authentication, and the
full desktop start/observe/stop flow,
then also requires smooth and discrete scroll delivery. Hyprland's virtual-pointer wheel-event fix
([Hyprland #15319](https://github.com/hyprwm/Hyprland/pull/15319)) is included
in the 0.56.0 release line used by the CI compositor.
These checks do not cover the interactive ScreenCast/RemoteDesktop portal chooser.
The opt-in portal smoke below waits for the system source chooser, then checks
two captured frames, fresh observation IDs, and session cleanup. It is optional
and requires a person to approve a source; Hyprland's default route does not use
it.

```sh
python3 scripts/hyprland-smoke.py target/release/computer-use-linux
python3 scripts/hyprland-smoke.py --headless-output target/release/computer-use-linux
python3 scripts/hyprland-smoke.py --qt target/release/computer-use-linux
python3 scripts/hyprland-smoke.py --headless-output --qt target/release/computer-use-linux
python3 scripts/hyprland-http-smoke.py target/release/computer-use-linux
python3 scripts/portal-smoke.py target/release/computer-use-linux
# Optional Firefox web-content accessibility and action check; needs Firefox.
python3 scripts/firefox-atspi-smoke.py target/release/computer-use-linux
```

The Firefox check creates a private temporary profile and local page, verifies
that web controls and screenshot-mapped bounds appear in the AT-SPI tree, invokes
a button through its advertised action, and then removes the profile. When more
than one output is active, pass `--monitor OUTPUT` to select the test display.

Stdio is the default and is intended for MCP clients that launch and manage a local server process. To expose MCP Streamable HTTP to another local client, use:

```sh
./target/release/computer-use-linux --http 8765
```

HTTP binds only to `127.0.0.1`, requires the printed bearer token, checks the Host and optional Origin headers, and serves `/mcp`. Keep the token private. Actions execute immediately without human confirmation, so connect only an agent trusted to operate this desktop. MCP requests receive an SSE response with keepalive comments; closing the response stream cancels the in-flight request. HTTP notifications receive `202 Accepted`.

The server supports legacy MCP handshakes for `2025-03-26`, `2025-06-18` and `2025-11-25` (and answers `initialize` with its usage instructions), plus stateless requests using `2026-07-28`. Modern requests include protocol version and client capabilities in `_meta`; the HTTP transport also validates the required MCP headers. Older clients continue to use `initialize` and the negotiated legacy version. Over HTTP, a legacy request without an `MCP-Protocol-Version` header is treated as `2025-03-26`, as the MCP specification requires.

Portal requests subscribe before dispatch when the backend supports predictable request handles, then resubscribe to the returned handle for older portal implementations. Cancelled or timed-out requests close their pending portal request so chooser UI can be dismissed.

Stdio handles independent MCP requests concurrently, rejects duplicate in-flight request IDs, and caps pending requests at 32. It honors `notifications/cancelled` for pending requests. Cancellation stops waiting for reads and startup; for `computer_act`, a running wait ends immediately, native Wayland text stops after releasing its current key, and a non-X11 drag stops at the next path point after releasing its button. Other in-progress actions finish before the rest of the batch is skipped. The server does not refresh the observation after cancellation. A cancelled MCP call has no result; the stdio request or HTTP handler remains in flight until held-input cleanup finishes. HTTP response-stream cancellation and stdio cancellation follow the same behavior. Dropping a portal-backed session attempts to close it, including when startup or capture is cancelled.

On stdio EOF, Ctrl-C, or SIGTERM, the server lets an in-progress `computer_act` action finish, skips any remaining batch actions, closes its desktop session, and waits for held-key and held-button cleanup before exiting.

## Tools

- `computer_status` reports preflight capabilities, `available_backends` (each with the `target` it controls, `available`, and `supported`), `recommended_backend` (the first backend that is both available and supported), and `output_selection_required` with `recommended_output` when exactly one Hyprland output is known. It includes `output_discovery_error` if the monitor list could not be trusted; the agent must not guess an output when selection is required or discovery failed. `available_backends` reports runtime availability and project support; a backend is available only when its implementation is included in the binary and the session can use it. `capabilities.build_features` lists the optional implementations included at compile time. When AT-SPI probing fails, `capabilities.accessibility_probe_error` gives the reason. `hyprland-native` uses in-process Wayland screencopy and needs no capture helper. `hyprland-grim` is the optional fallback and reports `grim` in `missing_dependencies`; helper processes use in-process deadlines and process-group cleanup. Hyprland IPC is accessed directly over its session socket. Known Hyprland `outputs` include every named active output even if geometry is incomplete, along with description, logical position, pixel resolution (`width`/`height`), scale, transformed logical dimensions, and focus state when available. The agent can make an informed output choice before starting. Portal source selection happens during portal startup. The status also reports helper-process supervision, display socket, text and scroll input, clipboard, native virtual keyboard and data-control clipboard support, Grim fallback, cursor capture, virtual-pointer and portal capabilities, accessibility support, and GStreamer multi-stream support. Portal input still depends on consent and selected-stream geometry; `computer_start` returns the capabilities confirmed for its session.
- `computer_start` opens one process-local desktop session, captures a real frame from the selected backend, and returns that initial screenshot, an `observation_id`, its `session_id`, `active_backend`, `active_target`, and capabilities confirmed by the selected backend. On Hyprland, `selected_output` echoes the output captured and controlled by the session, and `capabilities.native_cursor_hidden` reports the compositor's current native cursor visibility without changing it. Use the returned observation for the first action; `capture_validated` is true only after the frame succeeds. `backend` selects `auto` (default, prefers `hyprland-native` on Hyprland), `hyprland-native`, `hyprland-grim`, `wayland-portal`, `sway-grim`, `wayland-grim`, or `x11`; unavailable explicit backends fail without silently switching. Only the Hyprland target is currently supported; the other targets are experimental. The legacy Wayland `wayland_mode` selector remains supported when `backend` is omitted. On Hyprland, `output` (deprecated alias: `overlay_monitor`; passing both with different values is an error) scopes compositor capture, pointer coordinates, and keyboard focus checks to the chosen output. Portal mode may require consent. Actual input support is capability-reported.
- `computer_observe` captures the selected output or compositor layout and returns a JPEG, an `observation_id`, `selected_output` on Hyprland, the portal `source` type when available, the active window identity when AT-SPI exposes it inside the captured image, and a bounded AT-SPI tree with control labels, actions, active/focused state, bounds in their reported coordinate space (`screen` or safely mapped `window`), and screenshot-relative `image_bounds` when the compositor position is known. The tree is capped at 400 nodes, 16 levels, and 100 children per node; its `truncated` flag indicates when any limit omitted part of the tree. On Hyprland it also reports the live `capabilities.visual_pointer_overlay` and `capabilities.native_cursor_hidden` states and automatically restarts the overlay on the selected output if its process exited. `accessibility_actions_available` reports whether control bounds can be mapped safely; check each node's `actions` array to see whether its toolkit exposes an invocable action (the array may be empty). `accessibility_actions_reason` explains when AT-SPI actions are unavailable because geometry cannot be mapped safely. Starting a new observation invalidates the previous ID even if capture fails. Native Hyprland screencopy converts compositor-provided ARGB/XRGB 8888 and 2101010 frames to JPEG RGB; frames larger than 3200 pixels are area-resampled before encoding to reduce aliasing while retaining detail. Oversized 3200-pixel frames retry at lower JPEG qualities; if they still exceed 3 MiB, capture retries at 2560 and then 2048 pixels. The final 2048-pixel fallback also retries at low quality. This bounds the worst-case capture to six attempts while prioritizing resolution; accessibility metadata is also bounded and emitted once as structured content to keep the response size bounded. Identifying text and actions are only exposed for nodes inside the captured image. Short text excerpts are limited to visible, non-editable leaf controls; editable and password-role text is omitted. Accessibility actions and text replacement are limited to controls visible in the captured image.
- `computer_act` performs screenshot capture, click, double-click, pointer move, drag, scroll, keypress, text, clipboard read/write, bounded wait (2 seconds by default), or AT-SPI actions. Click and drag support left, middle, right, back, and forward buttons; `wheel` aliases middle-click. A `screenshot` action must be the only action in its call, needs no prior observation, and returns a fresh image and observation ID. Click, double-click, move, drag, and scroll accept a `keys` array to hold CTRL, SHIFT, ALT, or SUPER during the pointer action when the active backend exposes independent key press and release. `computer_start` reports this as `capabilities.mouse_modifiers` (`click_modifiers` remains as an alias); support is currently reported for X11, Hyprland, and Wayland portal sessions with keyboard input. Clipboard operations handle plain text up to 64 KiB through native Wayland ext-data-control when exposed, `wl-clipboard` on other Wayland routes, and `xclip` or `xsel` on X11. X11 clipboard reads prefer UTF-8 targets and convert the legacy `STRING` target from Latin-1. Clipboard reads expose clipboard contents to the agent; clipboard writes replace them. Pointer, keyboard, and accessibility actions require the `observation_id` from the latest observation, which expires after 30 seconds. Each `computer_act` call has a 40-second budget that includes waiting for another desktop operation; a started action finishes before the server skips the rest of an over-budget batch. Calls consume the current observation, and normally return a fresh screenshot and `observation_id`, including when an executed input action reports an error or times out. If a post-action focus check prevents a fresh snapshot, the result sets `observe_again`; call `computer_observe` before another screen-grounded action. On error, `current_action_started` tells whether the action handler began; if true, the action may have partially affected the desktop, so observe before retrying. Use `actions` for a short ordered batch of up to 8 actions based on the same screenshot, such as click then type. When focus identity is available through AT-SPI, Hyprland, Sway, or X11, each pointer or keyboard action verifies that focus has not changed since observation; if an earlier pointer action changes focus, the batch verifies and adopts the newly focused window before continuing; unrelated focus changes stop the batch before input is sent. Accessibility calls recheck the element name, role, observed bounds, live visibility/enabled state, and the owning process and accessible ID when the toolkit exposes them. Drag accepts a 2–64 point `path` as well as the simple `x`/`y` to `to_x`/`to_y` form. Scroll accepts signed `scroll_x` and `scroll_y` logical-pixel distances from -1000 to 1000, including diagonal scrolling. If `smooth_scroll_input` is true, Linux smooth-scroll events preserve the distance; otherwise each nonzero axis rounds up to 120-pixel wheel steps. The existing direction/amount form remains available for 1-100 discrete steps. `keypress` accepts `keys` as a modifier chord, such as `["CTRL", "A"]` or `["CTRL", "SHIFT", "S"]` for Ctrl+Shift+S; choose this form or `key` with optional `modifiers`, and do not combine them. `CONTROL` aliases `CTRL`, and `META`/`WINDOWS` alias `SUPER`. Common key aliases such as `ENTER`, `ESC`, and `ARROWDOWN` are normalized across backends. AT-SPI actions can be selected by the advertised `action_name`; `set_text` replaces text in a visible editable control when its toolkit exposes AT-SPI EditableText.
- `computer_stop` closes the process-local session.

`cursor_capture` means the pointer is drawn into screenshot pixels. `pointer_position` means the observation can also return its coordinates as structured data; the two capabilities are independent. When embedded portal cursor capture is unavailable, `cursor_metadata_capture` reports whether PipeWire cursor metadata can be requested and extracted through Python GI GStreamer bindings, `GstVideo` ROI metadata support, and a usable JPEG encoder (`jpegenc` or `avenc_mjpeg`). If metadata extraction fails during a session, it is disabled for the rest of that session, and subsequent portal observations report the updated capability. Selected streams with known screen positions and dimensions report pointer coordinates without drawing the pointer into the image; PipeWire frame coordinates are mapped through each stream's logical dimensions into the portal's compositor coordinate space. Multi-stream captures keep their tiled or positioned compositor layout, including overlapping or mirrored monitor regions.

`computer_start` returns `active_backend` after permission selection and fallback (`wayland-portal`, `hyprland-native`, `hyprland-grim`, `sway-grim`, `wayland-grim`, or `x11`) alongside the confirmed session capabilities.

Portal-backed observations include `input_regions`, mapping each selected stream to screenshot-pixel bounds and reporting pointer motion, click, and scroll support per region. In multi-stream sessions with different EIS capabilities across monitors, choose click or drag targets only in regions whose `pointer_click` flag is true; overlapping regions are routed through a stream that supports the requested action. Session-wide capability flags report whether any selected region supports the action.

Portal captures without compositor positions remain viewable, but `computer_start` reports `accessibility_actions: false` because AT-SPI control bounds cannot be mapped safely to the screenshot.

When AT-SPI exposes a focused control, observations retain its accessible identity and bounds. Before keyboard input, the server checks that the control still has focus; after a pointer action in the same batch, it accepts the control focused by that action. This supplements the active-window check and is unavailable when the desktop toolkit does not expose keyboard focus through AT-SPI.

Unicode text entry is advertised when a compositor virtual-keyboard adapter or portal EIS text device is active, and on X11 when `xdotool` is available. A RemoteDesktop session without EIS uses `wtype` for text when the compositor virtual-keyboard protocol is available; otherwise it falls back to keysyms, which do not reliably enter arbitrary Unicode on Wayland. The Hyprland IPC text fallback supports ASCII. Hyprland uses an in-process `zwp_virtual_keyboard_v1` client for keypresses and Unicode text when the compositor exposes it; `wtype` remains an optional fallback. Capability reports distinguish general text input from Unicode text input and keypress input. On X11, `xdotool` is used when installed; otherwise XTEST resolves ASCII against the active XKB group and its first two key levels, accounts for Caps Lock, and requires Shift, Ctrl, Alt, Super, and AltGr to be released. `set_text` supports UTF-8 text for visible controls that expose AT-SPI EditableText, without depending on the active keyboard layout. Hyprland text entered through IPC is validated and delivered in one bounded socket batch. Hyprland smooth and discrete scrolling use the wlroots virtual-pointer protocol when available; without it, compositor-fallback sessions do not support scrolling. The Sway fallback uses `wtype` only when its virtual-keyboard protocol is available; pointer movement, buttons, and scrolling use Sway IPC. On other compositors using the Grim fallback, `wtype` can provide keyboard and Unicode text input while pointer and scroll input remain unavailable. Each server process holds one desktop session and one latest observation, which a screen-grounded action consumes.

## Current Linux backends

Hyprland and Sway Grim captures and X11 captures target a 3200-pixel maximum long edge, with pointer coordinates mapped back to compositor layout or X11 root geometry. A selected Hyprland output uses its native scale up to that image limit, preserving HiDPI detail while keeping screenshot coordinates mapped to logical desktop coordinates. Hyprland observations also report the pointer position when the Hyprland IPC cursor query is available, even if the screenshot does not contain the cursor image. Other Grim-only compositors lack reliable layout geometry, so their first capture is unscaled; oversized frames retry at relative scales, and pointer input is unavailable.

- **Wayland:** starts a ScreenCast portal session and captures PipeWire frames with GStreamer. It requests an embedded cursor when available. If only metadata cursor mode is available and the Python GstVideo binding is installed, selected streams report the pointer position as structured data; the cursor may still not appear in screenshot pixels. Portal captures are scaled to at most 3200 pixels on the longest edge when stream geometry is available, while preserving the logical coordinate mapping. When provided by the portal, PipeWire capture targets its stable stream serial to avoid node-ID reuse after hotplug; older portals fall back to the stream node ID. Portal stream dimensions and positions are optional. Portal `position` and `size` describe compositor coordinates, which can differ from PipeWire frame pixels; absolute pointer input is mapped through the stream logical bounds. A single stream remains capturable if geometry is missing; pointer actions require known dimensions, while desktop-relative pointer coordinates and accessibility bounds require a known stream position. With metadata cursor capture, a single stream with known dimensions can still report pointer coordinates relative to its screenshot when its desktop position is unknown. Multiple streams without positions are tiled and mapped per stream. When `multi_stream_capture` is true, the portal allows selecting multiple sources; when multiple streams are selected, GStreamer composites them using portal monitor positions and scales, and screenshot coordinates route pointer input to the matching stream and EIS mapping ID. If the portal omits positions, selected streams are tiled horizontally and accessibility bounds are withheld because that synthetic arrangement does not match compositor coordinates. Multiple streams require the GStreamer compositor plugin and are limited to 16 streams; `multi_stream_capture` reports whether the plugin is available. If RemoteDesktop is available, it prefers EIS input through `libei` when the portal exposes `ConnectToEIS` and all selected streams have mapping IDs; otherwise it uses portal input methods. Pointer coordinates are mapped from each selected stream’s compositor-space size to the matched EIS region, whose dimensions may differ. Once EIS connects, all RemoteDesktop input goes through its negotiated devices; portal `Notify*` methods are not used as a fallback. EIS availability and supported pointer, keyboard, and scroll devices depend on the compositor and are reported per session. On Hyprland, `grim` can capture the output layout directly when portal capture is unavailable. When portal pointer or keyboard grants are missing, Hyprland IPC can provide pointer and keyboard input for positioned monitor streams; scroll still needs portal input or the wlroots virtual-pointer protocol. On Sway, `grim` captures the output layout and Sway IPC provides pointer movement, clicks, and wheel input when portal capture or RemoteDesktop input is unavailable. On compositor-layout fallback paths without a RemoteDesktop session, the server also uses the wlroots virtual-pointer protocol when the compositor exposes it, providing pointer movement, buttons, and scrolling on Hyprland and Sway. Hyprland uses the native in-process virtual-keyboard client for keypresses and Unicode text when exposed; `wtype` remains an optional Hyprland fallback and the keyboard adapter for experimental generic and Sway routes. Sway marks its cursor IPC commands deprecated, so pointer control uses the virtual-pointer protocol when available and falls back to those commands on older setups. Hyprland and Sway layout geometry is refreshed for each observation, and screen-grounded actions are rejected if it changes before input. The Hyprland smoke test exercises a temporary portrait output at 125% scale alongside the physical display; other physical multi-monitor and scale combinations still need hardware coverage. When the ScreenCast portal or its PipeWire/GStreamer capture pipeline is unavailable, Grim can also provide screenshots on other compositors that support its screencopy protocol; `wtype` can provide keyboard input there when its virtual-keyboard probe succeeds. On generic Grim fallbacks, `wlr-randr` can provide output geometry so virtual-pointer coordinates can be mapped safely; without it, pointer and scroll remain disabled even when the compositor advertises virtual-pointer support. Grim captures include the cursor when supported by the installed version. Scroll is unavailable through the Hyprland adapter unless the virtual-pointer protocol is available.
- **X11:** captures with FFmpeg `x11grab` when available, which includes the cursor, and falls back to ImageMagick `import` otherwise. Uses dynamically loaded X11/XTest libraries for pointer and keyboard input. Observations include root-window pointer coordinates when Xlib can query them, and focused-window checks use Xlib if `xdotool` is unavailable.
- **Accessibility:** uses an in-process Rust AT-SPI2 client over the accessibility D-Bus to query a bounded tree and invoke actions or editable-text operations. It reads child lists through `GetChildren` to handle toolkit differences in `ChildCount`. On Hyprland, `computer_start` and each observation enable the AT-SPI bus so built-in Qt controls can join the tree without a per-application environment setting; custom widgets still depend on the application exposing accessible interfaces and the desktop providing its AT-SPI bridge.

Other Wayland compositors can capture through the portal; input is available when their RemoteDesktop portal supports it. If the ScreenCast portal or its PipeWire/GStreamer capture pipeline is unavailable, Grim can provide screenshots on compositors that support its screencopy protocol. A successful `wtype` probe can provide keyboard input in this general fallback; pointer and scroll require the wlroots virtual-pointer protocol plus compositor layout coordinates, supplied by `wlr-randr` on generic Grim fallbacks. Capability detection is per session, so capture and each input path are reported independently. There is no `/dev/uinput` or root input path.

GTK, Qt, and other toolkits do not need separate mouse drivers: coordinate input is provided by the session backend, while labels, controls, and direct text replacement use AT-SPI when the application exposes it. AT-SPI coverage depends on the installed toolkit bridge and is reported separately from screenshot and input support.

Portal-backed Wayland sessions include built-in X keysyms for common keys, keypad keys, and ASCII typing; `libxkbcommon` adds support for extended named keysyms.

HTTP binds only to loopback and requires the ephemeral bearer token printed to stderr. Do not expose the listener beyond loopback. Actions execute immediately without human confirmation; connect only an agent trusted to operate this desktop. X11 typing resolves ASCII against the active XKB group and its unshifted or shifted key levels.
