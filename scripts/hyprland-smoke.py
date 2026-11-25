#!/usr/bin/env python3
"""Exercise default computer use on the currently logged-in Hyprland desktop."""

import base64
import argparse
import json
import os
import selectors
import signal
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

POINTER_OVERLAY_SIZE = 12
POINTER_HOTSPOT = (1, 1)


def structured(result):
    if "content" not in result and "structuredContent" not in result:
        return result
    value = result.get("structuredContent")
    if isinstance(value, dict):
        return value
    for item in result.get("content", []):
        if item.get("type") == "text":
            try:
                value = json.loads(item["text"])
            except (KeyError, TypeError, json.JSONDecodeError):
                continue
            if isinstance(value, dict):
                return value
    raise RuntimeError(f"MCP result has no structured content: {result}")


class McpProcess:
    def __init__(self, binary, env=None):
        self.env = (os.environ if env is None else env).copy()
        self.env.pop("XKB_CONFIG_ROOT", None)
        self.process = subprocess.Popen(
            [binary, "--stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=self.env,
        )
        self.xkb_bundle_path = None
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.request_id = 0
        self.responses = {}
        try:
            self.require_xkb_bundle()
        except Exception:
            self.close()
            raise

    def require_xkb_bundle(self):
        prefix = f"computer-use-xkb-{self.process.pid}-"
        temp_root = Path(self.env.get("TMPDIR", tempfile.gettempdir()))
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            bundles = [path for path in temp_root.glob(prefix + "*") if path.is_dir()]
            complete = [
                bundle for bundle in bundles
                if all((bundle / path).is_file() for path in (
                    "rules/evdev",
                    "symbols/us",
                    "licenses/xkeyboard-config-COPYING",
                ))
            ]
            if complete:
                if len(complete) != 1:
                    raise RuntimeError(f"MCP server created multiple XKB bundles: {complete}")
                bundle = complete[0]
                if bundle.stat().st_mode & 0o777 != 0o700:
                    raise RuntimeError(f"MCP XKB bundle is not private: {bundle}")
                self.xkb_bundle_path = bundle
                return
            if self.process.poll() is not None:
                raise RuntimeError(f"MCP server exited before unpacking bundled XKB data: {self.process.stderr.read()[-2000:]}")
            time.sleep(0.05)
        raise TimeoutError("MCP server did not unpack bundled XKB data")

    def rpc(self, method, params=None, timeout=30, notification=False):
        request_id = None
        if not notification:
            self.request_id += 1
            request_id = self.request_id
        message = {"jsonrpc": "2.0", "method": method}
        if not notification:
            message["id"] = request_id
        if params is not None:
            message["params"] = params
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()
        if notification:
            return {}
        return self.receive(request_id, method, timeout)

    def send(self, method, params):
        self.request_id += 1
        request_id = self.request_id
        message = {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()
        return request_id

    def cancel(self, request_id):
        message = {"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": request_id}}
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def receive(self, request_id, method, timeout):
        key = str(request_id)
        if key in self.responses:
            response = self.responses.pop(key)
        else:
            deadline = time.monotonic() + timeout
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not self.selector.select(remaining):
                    raise TimeoutError(f"MCP {method} timed out")
                response = json.loads(self.process.stdout.readline())
                response_key = str(response.get("id"))
                if response_key == key:
                    break
                self.responses[response_key] = response
        if "error" in response:
            raise RuntimeError(f"MCP {method} failed: {response['error']}")
        return response.get("result", {})

    def call(self, name, arguments):
        result = self.rpc("tools/call", {"name": name, "arguments": arguments})
        if result.get("isError"):
            details = result.get("content", [{}])[0].get("text", "unknown error")
            raise RuntimeError(f"{name} failed: {details}")
        return result, structured(result)

    def close(self):
        self.selector.close()
        if not self.process.stdin.closed:
            self.process.stdin.close()
        graceful = self.process.poll() is not None and self.process.returncode >= 0
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            graceful = False
            self.process.terminate()
            self.process.wait(timeout=5)
        if graceful and self.xkb_bundle_path is not None and self.xkb_bundle_path.exists():
            raise RuntimeError(f"MCP server left its XKB bundle after graceful shutdown: {self.xkb_bundle_path}")


def image_point(x, y, display):
    return (
        round((x - display["origin_x"]) * display["width"] / display["logical_width"]),
        round((y - display["origin_y"]) * display["height"] / display["logical_height"]),
    )


def check_agent_pointer_image(mcp, session_id, monitor, temp_dir):
    magick = shutil.which("magick")
    compare = [magick, "compare"] if magick else [shutil.which("compare")]
    convert = [magick] if magick else [shutil.which("convert")]
    if not magick and (not compare[0] or not convert[0]):
        raise RuntimeError("Hyprland pointer-image smoke requires ImageMagick")
    baseline = Path(temp_dir) / "pointer-screen.png"
    subprocess.run(
        ["grim", "-o", monitor, str(baseline)],
        check=True,
        capture_output=True,
        text=True,
        timeout=5,
    )
    result, data = mcp.call("computer_observe", {"session_id": session_id})
    image = next((item for item in result.get("content", []) if item.get("type") == "image"), None)
    pointer = data.get("display", {}).get("pointer")
    width, height = data.get("display", {}).get("width"), data.get("display", {}).get("height")
    if image is None or pointer is None or not isinstance(width, int) or not isinstance(height, int):
        raise RuntimeError(f"computer_observe omitted the image or pointer coordinates: {data}")
    capture = Path(temp_dir) / "pointer-agent.jpg"
    capture.write_bytes(base64.b64decode(image["data"], validate=True))
    crop_width, crop_height = min(24, width), min(24, height)
    left = max(0, min(pointer["x"] - crop_width // 2, width - crop_width))
    top = max(0, min(pointer["y"] - crop_height // 2, height - crop_height))
    baseline_crop = Path(temp_dir) / "pointer-screen-crop.png"
    capture_crop = Path(temp_dir) / "pointer-agent-crop.png"
    geometry = f"{crop_width}x{crop_height}+{left}+{top}"
    subprocess.run(
        convert + [str(baseline), "-resize", f"{width}x{height}!", "-crop", geometry, "+repage", str(baseline_crop)],
        check=True,
        capture_output=True,
        text=True,
        timeout=5,
    )
    subprocess.run(
        convert + [str(capture), "-crop", geometry, "+repage", str(capture_crop)],
        check=True,
        capture_output=True,
        text=True,
        timeout=5,
    )
    compared = subprocess.run(
        compare + ["-metric", "AE", "-fuzz", "8%", str(baseline_crop), str(capture_crop), "null:"],
        capture_output=True,
        text=True,
        timeout=5,
    )
    try:
        changed_pixels = float(compared.stderr.split()[0])
    except (IndexError, ValueError) as error:
        raise RuntimeError(f"ImageMagick did not report pointer-image differences: {compared.stderr}") from error
    if compared.returncode not in (0, 1) or changed_pixels < 3.0:
        raise RuntimeError(
            f"computer_observe did not include the AI pointer over the cursor-free screen capture: "
            f"changed pixels={changed_pixels}, metric={compared.stderr}, pointer={pointer}"
        )
    return data


def logical_output_size(output):
    width, height = output["width"], output["height"]
    if output.get("transform", 0) % 2:
        width, height = height, width
    return round(width / output["scale"]), round(height / output["scale"])


def output_contains(output, point):
    width, height = logical_output_size(output)
    return output["x"] <= point[0] < output["x"] + width and output["y"] <= point[1] < output["y"] + height


def cursor_invisible():
    state = json.loads(subprocess.check_output(["hyprctl", "getoption", "cursor:invisible", "-j"], text=True))
    return state.get("bool")


def wait_cursor_visibility(expected, timeout=3):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if cursor_invisible() is expected:
            return
        time.sleep(0.025)
    raise RuntimeError(f"Hyprland cursor:invisible did not become {expected}: {cursor_invisible()}")


def move_compositor_pointer(x, y):
    result = subprocess.run(
        ["hyprctl", "eval", f"hl.dispatch(hl.dsp.cursor.move({{ x = {x}, y = {y} }}))"],
        capture_output=True,
        text=True,
        timeout=3,
    )
    if result.returncode or "error" in result.stdout.lower():
        raise RuntimeError(f"could not move the test pointer to {(x, y)}: {result.stderr or result.stdout}")


def window_image_bounds(window, display, bounds):
    x, y, width, height = bounds
    image_x, image_y = image_point(window["at"][0] + x, window["at"][1] + y, display)
    return {
        "x": image_x,
        "y": image_y,
        "width": round(width * display["width"] / display["logical_width"]),
        "height": round(height * display["height"] / display["logical_height"]),
    }


def pointer_overlay():
    layers = json.loads(subprocess.check_output(["hyprctl", "layers", "-j"], text=True))
    for monitor, data in layers.items():
        for entries in data.get("levels", {}).values():
            for entry in entries:
                if entry.get("namespace") == "computer-use-pointer":
                    return monitor, entry
    return None, None


def wait_pointer_overlay(monitor, pointer=None):
    monitors = json.loads(subprocess.check_output(["hyprctl", "monitors", "-j"], text=True))
    output = next((item for item in monitors if item.get("name") == monitor), None)
    if not output or not isinstance(output.get("scale"), (int, float)) or output["scale"] <= 0:
        raise RuntimeError(f"cannot verify pointer overlay scale on {monitor}: {output}")
    scale = output["scale"]
    expected_size = round(POINTER_OVERLAY_SIZE / scale)
    deadline = time.monotonic() + 2
    active_monitor, entry = None, None
    while time.monotonic() < deadline:
        active_monitor, entry = pointer_overlay()
        if active_monitor == monitor and entry:
            follows_pointer = pointer is None or (
                entry["x"] == pointer[0] - POINTER_HOTSPOT[0]
                and entry["y"] == pointer[1] - POINTER_HOTSPOT[1]
            )
            logical_size = (entry["w"], entry["h"])
            physical_size = (round(entry["w"] * scale), round(entry["h"] * scale))
            if follows_pointer and logical_size == (expected_size, expected_size) and physical_size == (POINTER_OVERLAY_SIZE, POINTER_OVERLAY_SIZE):
                return entry
        time.sleep(0.025)
    cursor = subprocess.run(
        ["hyprctl", "cursorpos", "-j"], capture_output=True, text=True, timeout=3
    )
    monitors = subprocess.run(
        ["hyprctl", "monitors", "-j"], capture_output=True, text=True, timeout=3
    )
    raise RuntimeError(
        f"native mouse-arrow overlay did not follow pointer {pointer} on {monitor}; "
        f"last layer was {active_monitor}: {entry}; cursor={cursor.stdout.strip()}; "
        f"monitor={next((item for item in json.loads(monitors.stdout) if item.get('name') == monitor), None) if monitors.returncode == 0 else monitors.stderr.strip()}"
    )


def wait_pointer_overlay_offscreen(output, timeout=2):
    width, height = logical_output_size(output)
    left, top = output["x"], output["y"]
    right, bottom = left + width, top + height
    deadline = time.monotonic() + timeout
    active_monitor, entry = None, None
    while time.monotonic() < deadline:
        active_monitor, entry = pointer_overlay()
        if active_monitor == output["name"] and entry and (
            entry["x"] + entry["w"] <= left or entry["x"] >= right
            or entry["y"] + entry["h"] <= top or entry["y"] >= bottom
        ):
            return
        time.sleep(0.025)
    raise RuntimeError(f"pointer overlay was not parked outside {output['name']}: {active_monitor} {entry}")


def wait_overlay_closed():
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline:
        if pointer_overlay() == (None, None):
            return
        time.sleep(0.025)
    raise RuntimeError("native mouse-arrow overlay remained mapped after computer_stop")


def check_overlay_parent_death(binary, monitor):
    client = McpProcess(binary)
    overlay_pid = None
    bundle_path = client.xkb_bundle_path
    previous_cursor_invisible = cursor_invisible()
    try:
        client.rpc(
            "initialize",
            {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "overlay-lifecycle-smoke", "version": "1"},
            },
        )
        client.rpc("notifications/initialized", notification=True)
        _, metadata = client.call("computer_start", {"output": monitor})
        if not metadata.get("capabilities", {}).get("visual_pointer_overlay"):
            raise RuntimeError("overlay lifecycle smoke could not start the visual pointer")
        active_monitor, entry = pointer_overlay()
        if active_monitor != monitor or not entry:
            raise RuntimeError(f"overlay lifecycle smoke found no pointer on {monitor}: {active_monitor}, {entry}")
        overlay_pid = entry.get("pid")
        os.kill(client.process.pid, signal.SIGKILL)
        client.process.wait(timeout=5)
        try:
            wait_overlay_closed()
        except RuntimeError as error:
            if overlay_pid:
                try:
                    os.kill(overlay_pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                wait_overlay_closed()
            raise RuntimeError("pointer overlay survived an abrupt MCP server exit") from error
        wait_cursor_visibility(previous_cursor_invisible)
        if not bundle_path.exists():
            raise RuntimeError("SIGKILL unexpectedly removed the MCP server's XKB bundle")
        return bundle_path
    finally:
        client.close()


def check_overlay_child_death(binary, monitor):
    client = McpProcess(binary)
    session_id = None
    previous_cursor_invisible = cursor_invisible()
    try:
        client.rpc(
            "initialize",
            {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "overlay-health-smoke", "version": "1"},
            },
        )
        client.rpc("notifications/initialized", notification=True)
        _, metadata = client.call("computer_start", {"output": monitor})
        session_id = metadata["session_id"]
        active_monitor, entry = pointer_overlay()
        if active_monitor != monitor or not entry:
            raise RuntimeError(f"overlay health smoke found no pointer on {monitor}: {active_monitor}, {entry}")
        os.kill(entry["pid"], signal.SIGTERM)
        wait_overlay_closed()
        wait_cursor_visibility(previous_cursor_invisible)
        _, observation = client.call("computer_observe", {"session_id": session_id})
        if observation.get("capabilities", {}).get("visual_pointer_overlay") is not True:
            raise RuntimeError(f"computer_observe did not recover the exited overlay: {observation}")
        if not observation.get("observation_id"):
            raise RuntimeError(f"overlay loss incorrectly prevented a fresh screen observation: {observation}")
        if not any("exited and was restarted" in note for note in observation.get("notes", [])):
            raise RuntimeError(f"computer_observe omitted the overlay recovery note: {observation}")
        selected_monitor = next(
            item for item in json.loads(subprocess.check_output(["hyprctl", "monitors", "-j"], text=True))
            if item["name"] == monitor
        )
        pointer = json.loads(subprocess.check_output(["hyprctl", "cursorpos", "-j"], text=True))
        expected_hidden = previous_cursor_invisible
        if observation.get("capabilities", {}).get("native_cursor_hidden") is not expected_hidden:
            raise RuntimeError(
                f"restarted overlay reported the wrong native cursor visibility: "
                f"prior={previous_cursor_invisible}, expected={expected_hidden}, pointer={pointer}, "
                f"monitor={selected_monitor}, capabilities={observation['capabilities']}"
            )
        wait_cursor_visibility(expected_hidden)
        wait_pointer_overlay(monitor)
        client.call("computer_stop", {"session_id": session_id})
        session_id = None
        wait_cursor_visibility(previous_cursor_invisible)
    finally:
        if session_id and client.process.poll() is None:
            try:
                client.call("computer_stop", {"session_id": session_id})
            except Exception:
                pass
        client.close()


def check_native_runtime_without_path(binary, monitor, stale_bundle):
    environment = os.environ.copy()
    environment["PATH"] = ""
    client = McpProcess(binary, environment)
    try:
        if stale_bundle.exists():
            raise RuntimeError(f"new server did not reap the killed server's XKB bundle: {stale_bundle}")
        client.rpc(
            "initialize",
            {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "empty-path-smoke", "version": "1"},
            },
        )
        client.rpc("notifications/initialized", notification=True)
        _, metadata = client.call("computer_start", {"output": monitor})
        capabilities = metadata.get("capabilities", {})
        if metadata.get("active_backend") != "hyprland-native" or not metadata.get("capture_validated"):
            raise RuntimeError(f"native Hyprland did not start and capture with an empty PATH: {metadata}")
        if not all(capabilities.get(name) for name in ("screenshot", "pointer_input", "keyboard_input", "visual_pointer_overlay")):
            raise RuntimeError(f"native Hyprland lost core capabilities with an empty PATH: {capabilities}")
        client.call("computer_stop", {"session_id": metadata["session_id"]})
    finally:
        client.close()


def receiver_window(clients):
    return next((client for client in clients if client.get("title") == "Computer use input test"), None)


def set_runtime_xkb_layout(layout):
    result = subprocess.run(
        ["hyprctl", "eval", f"hl.config({{ input = {{ kb_layout = {json.dumps(layout)} }} }})"],
        capture_output=True,
        text=True,
        timeout=5,
    )
    if result.returncode or "error" in result.stdout.lower():
        raise RuntimeError(f"could not set temporary XKB layout to {layout}: {result.stderr or result.stdout}")


def run(binary, headless_output=False, qt_receiver=False, xkb_fallback=False, high_resolution_output=False, hyprland_only_build=False):
    if "wayland" not in os.environ.get("XDG_SESSION_TYPE", "").lower():
        raise RuntimeError("Hyprland smoke requires a Wayland session")
    if not os.environ.get("HYPRLAND_INSTANCE_SIGNATURE"):
        raise RuntimeError("Hyprland smoke requires the active Hyprland environment")
    original = json.loads(subprocess.check_output(["hyprctl", "cursorpos", "-j"], text=True))
    original_cursor_invisible = cursor_invisible()
    if not isinstance(original_cursor_invisible, bool):
        raise RuntimeError("Hyprland did not report its initial cursor visibility")
    initial_monitors = json.loads(subprocess.check_output(["hyprctl", "monitors", "-j"], text=True))
    original_monitors = {monitor["name"] for monitor in initial_monitors}
    original_focused = next((monitor for monitor in initial_monitors if monitor.get("focused")), None)
    original_focused_monitor = original_focused.get("name") if original_focused else None
    original_focused_workspace = original_focused.get("activeWorkspace", {}).get("id") if original_focused else None
    with tempfile.TemporaryDirectory(prefix="computer-use-hyprland-") as temp:
        event_log = Path(temp) / "events.jsonl"
        mcp_environment = None
        if xkb_fallback:
            missing_library_dir = Path(temp) / "without-xkbcommon"
            missing_library_dir.mkdir()
            (missing_library_dir / "libxkbcommon.so.0").write_bytes(b"intentionally invalid shared object\n")
            mcp_environment = os.environ.copy()
            mcp_environment["LD_LIBRARY_PATH"] = os.pathsep.join(filter(None, (str(missing_library_dir), mcp_environment.get("LD_LIBRARY_PATH", ""))))
        receiver_script = "wayland-qt-receiver.py" if qt_receiver else "wayland-gtk-receiver.py"
        receiver_env = os.environ.copy()
        if qt_receiver:
            receiver_env["QT_QPA_PLATFORM"] = "wayland"
        receiver = subprocess.Popen(
            [sys.executable, str(Path(__file__).with_name(receiver_script)), str(event_log)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
            env=receiver_env,
        )
        mcp = McpProcess(binary, mcp_environment)
        session_id = None
        virtual_output = None
        clipboard_checked = False
        clipboard_touched = False
        clipboard_was_empty = False
        clipboard_backup = None
        clipboard_mime = None
        clipboard_restore_error = None
        try:
            mcp.rpc(
                "initialize",
                {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "hyprland-smoke", "version": "1"},
                },
            )
            mcp.rpc("notifications/initialized", notification=True)

            deadline = time.monotonic() + 10
            window = None
            receiver_ready = False
            while time.monotonic() < deadline:
                clients = json.loads(subprocess.check_output(["hyprctl", "clients", "-j"], text=True))
                window = receiver_window(clients)
                if event_log.exists():
                    receiver_ready = any(json.loads(line).get("ready") for line in event_log.read_text().splitlines())
                if window and receiver_ready:
                    break
                if receiver.poll() is not None:
                    raise RuntimeError(f"GTK receiver exited: {receiver.stderr.read()[-2000:]}")
                time.sleep(0.1)
            if not window or not receiver_ready:
                raise RuntimeError("GTK receiver did not become ready in Hyprland")
            if qt_receiver and window.get("xwayland"):
                raise RuntimeError(f"Qt smoke receiver unexpectedly runs through XWayland: {window}")
            time.sleep(0.2)

            _, status = mcp.call("computer_status", {})
            if status.get("session_type") != "wayland" or "hyprland" not in status.get("desktop", "").lower():
                raise RuntimeError(f"unexpected desktop session: {status}")
            if hyprland_only_build:
                build_features = status.get("capabilities", {}).get("build_features", {})
                optional_features = ("backend_x11", "backend_sway", "backend_portal")
                if any(build_features.get(feature) is not False for feature in optional_features):
                    raise RuntimeError(f"default Hyprland build includes optional backend features: {build_features}")
            status_notes = "\n".join(status.get("notes", []))
            if xkb_fallback and "libxkbcommon is unavailable" not in status_notes:
                raise RuntimeError(f"blocking libxkbcommon was not reported: {status}")
            portal_note = "This Hyprland session exposes ScreenCast but not the RemoteDesktop portal."
            if portal_note in status_notes:
                capabilities = status.get("capabilities", {})
                expected_route = (
                    "Auto and compositor routes use native Hyprland screencopy"
                    if capabilities.get("hyprland_native_screencopy")
                    else "Auto and compositor routes use Grim with Hyprland input"
                    if capabilities.get("grim_capture")
                    else "No native or Grim compositor capture fallback is available"
                )
                if expected_route not in status_notes:
                    raise RuntimeError(f"computer_status gave a stale Hyprland capture-route note; expected {expected_route!r}: {status_notes}")
            backend_status = {item["name"]: item for item in status.get("available_backends", [])}
            if backend_status.get("hyprland-grim", {}).get("available") is not True or backend_status["hyprland-grim"].get("supported") is not True:
                raise RuntimeError(f"Hyprland backend is not advertised as available: {status.get('available_backends')}")
            status_outputs = {output["name"] for output in status.get("outputs", [])}
            active_outputs = {
                monitor["name"]
                for monitor in json.loads(subprocess.check_output(["hyprctl", "monitors", "-j"], text=True))
                if not monitor.get("disabled", False)
            }
            if status_outputs != active_outputs:
                raise RuntimeError(f"computer_status outputs do not match active Hyprland outputs: {status_outputs} != {active_outputs}")
            selection_required = len(active_outputs) != 1 or status.get("output_discovery_error") is not None
            if status.get("output_selection_required") is not selection_required:
                raise RuntimeError(
                    f"computer_status gave incorrect output-selection guidance: {status}"
                )
            recommended_output = (
                next(iter(active_outputs))
                if len(active_outputs) == 1 and not status.get("output_discovery_error")
                else None
            )
            if status.get("recommended_output") != recommended_output:
                raise RuntimeError(
                    "computer_status recommended an unsafe output: "
                    f"{status.get('recommended_output')!r}, expected {recommended_output!r}"
                )
            monitor_info = {
                monitor["name"]: monitor
                for monitor in json.loads(subprocess.check_output(["hyprctl", "monitors", "-j"], text=True))
                if not monitor.get("disabled", False)
            }
            for output in status.get("outputs", []):
                expected = monitor_info[output["name"]]
                if (output.get("width"), output.get("height"), output.get("scale")) != (expected["width"], expected["height"], expected["scale"]):
                    raise RuntimeError(f"computer_status output geometry differs from Hyprland: {output} != {expected}")
            for capability in ("screenshot", "pointer_input", "keyboard_input", "scroll_input"):
                if not status.get("capabilities", {}).get(capability):
                    raise RuntimeError(f"Hyprland does not report {capability}: {status}")
            if status["capabilities"].get("virtual_keyboard_protocol") and not status["capabilities"].get("unicode_text_input"):
                raise RuntimeError(f"preflight failed to report the working virtual-keyboard text path: {status}")
            if status["capabilities"].get("clipboard_read") and status["capabilities"].get("clipboard_write"):
                types = subprocess.run(
                    ["wl-paste", "--list-types"],
                    capture_output=True,
                    timeout=3,
                    env={**os.environ, "LC_ALL": "C"},
                )
                offered = types.stdout.decode(errors="replace").splitlines()
                empty_clipboard = types.returncode == 1 and types.stderr.strip() == b"Nothing is copied"
                if empty_clipboard or types.returncode == 0 and (not offered or set(offered) <= {"text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "TEXT", "STRING"}):
                    clipboard_was_empty = empty_clipboard or types.returncode == 0 and not offered
                    clipboard_mime = next((mime for mime in ("text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "TEXT", "STRING") if mime in offered), None)
                    if clipboard_mime:
                        saved = subprocess.run(["wl-paste", "--no-newline", "--type", clipboard_mime], capture_output=True, timeout=3)
                        if saved.returncode == 0:
                            clipboard_backup = saved.stdout
                            clipboard_checked = True
                    else:
                        clipboard_checked = True
            if os.environ.get("COMPUTER_USE_REQUIRE_CLIPBOARD") == "1" and not clipboard_checked:
                raise RuntimeError("CI requires an available plain-text Wayland clipboard")
            if headless_output:
                result = subprocess.run(["hyprctl", "output", "create", "headless"], capture_output=True, text=True, timeout=5)
                if result.returncode:
                    raise RuntimeError(f"could not create a temporary Hyprland output: {result.stderr or result.stdout}")
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    monitors = json.loads(subprocess.check_output(["hyprctl", "monitors", "-j"], text=True))
                    virtual_output = next((monitor for monitor in monitors if monitor["name"] not in original_monitors), None)
                    if virtual_output:
                        break
                    time.sleep(0.1)
                if not virtual_output:
                    raise RuntimeError("Hyprland did not expose the temporary headless output")
                output = virtual_output
                output_mode = "3840x2160@60" if high_resolution_output else "preferred"
                output_bitdepth = ", bitdepth = 10" if high_resolution_output else ""
                current_monitors = json.loads(subprocess.check_output(["hyprctl", "monitors", "-j"], text=True))
                position_x = round(max(
                    monitor["x"] + logical_output_size(monitor)[0]
                    for monitor in current_monitors if monitor["name"] != output["name"]
                ))
                result = subprocess.run(
                    [
                        "hyprctl", "eval",
                        f'hl.monitor({{ output = "{output["name"]}", mode = {json.dumps(output_mode)}, position = "{position_x}x0", scale = 1.25, transform = 1{output_bitdepth} }})',
                    ],
                    capture_output=True,
                    text=True,
                    timeout=5,
                )
                if result.returncode or "error" in result.stdout.lower():
                    raise RuntimeError(f"could not configure a 125% headless output: {result.stderr or result.stdout}")
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    virtual_output = next(
                        monitor for monitor in json.loads(subprocess.check_output(["hyprctl", "monitors", "-j"], text=True))
                        if monitor["name"] == output["name"]
                    )
                    resolution_ready = not high_resolution_output or max(virtual_output["width"], virtual_output["height"]) > 3200
                    if abs(virtual_output["scale"] - 1.25) < 0.01 and virtual_output.get("transform") == 1 and resolution_ready:
                        break
                    time.sleep(0.1)
                if abs(virtual_output["scale"] - 1.25) >= 0.01 or virtual_output.get("transform") != 1 or high_resolution_output and max(virtual_output["width"], virtual_output["height"]) <= 3200:
                    raise RuntimeError(f"Hyprland did not apply the temporary output mode and transform: {virtual_output}")
                receiver.terminate()
                receiver.wait(timeout=5)
                event_log.unlink(missing_ok=True)
                focused = subprocess.run(
                    ["hyprctl", "eval", f'hl.dispatch(hl.dsp.focus({{ monitor = "{virtual_output["name"]}" }}))'],
                    capture_output=True,
                    text=True,
                    timeout=5,
                )
                if focused.returncode or "error" in focused.stdout.lower():
                    raise RuntimeError(f"could not focus {virtual_output['name']} for the test receiver: {focused.stderr or focused.stdout}")
                receiver = subprocess.Popen(
                    [sys.executable, str(Path(__file__).with_name(receiver_script)), str(event_log)],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.PIPE,
                    text=True,
                    env=receiver_env,
                )
                deadline = time.monotonic() + 5
                receiver_ready = False
                window = None
                while time.monotonic() < deadline:
                    clients = json.loads(subprocess.check_output(["hyprctl", "clients", "-j"], text=True))
                    window = receiver_window(clients)
                    if event_log.exists():
                        receiver_ready = any(json.loads(line).get("ready") for line in event_log.read_text().splitlines())
                    if window and receiver_ready and window.get("monitor") == virtual_output.get("id"):
                        break
                    if receiver.poll() is not None:
                        raise RuntimeError(f"GTK receiver exited on the headless output: {receiver.stderr.read()[-2000:]}")
                    time.sleep(0.05)
                if not window or not receiver_ready or window.get("monitor") != virtual_output.get("id"):
                    raise RuntimeError(f"test receiver did not move to {virtual_output['name']}: {window}")

            all_monitors = json.loads(subprocess.check_output(["hyprctl", "monitors", "-j"], text=True))
            target_monitor = next(
                (monitor for monitor in all_monitors if monitor.get("id") == window.get("monitor")),
                None,
            )
            if not target_monitor:
                raise RuntimeError(f"could not identify the GTK receiver's output: {window}")
            if headless_output and (target_monitor["name"] != virtual_output["name"] or abs(target_monitor["scale"] - 1.25) >= 0.01):
                raise RuntimeError(f"full input smoke is not running on the 125% headless output: {target_monitor}")
            focused_window = subprocess.run(
                ["hyprctl", "eval", f'hl.dispatch(hl.dsp.focus({{ window = "address:{window["address"]}" }}))'],
                capture_output=True,
                text=True,
                timeout=5,
            )
            if focused_window.returncode or "error" in focused_window.stdout.lower():
                raise RuntimeError(f"could not focus the test receiver: {focused_window.stderr or focused_window.stdout}")
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                active = json.loads(subprocess.check_output(["hyprctl", "activewindow", "-j"], text=True))
                if active.get("address") == window.get("address"):
                    break
                time.sleep(0.05)
            if active.get("address") != window.get("address"):
                raise RuntimeError(f"Hyprland did not focus the test receiver: {active}")
            _, refreshed_status = mcp.call("computer_status", {})
            refreshed_outputs = {output["name"] for output in refreshed_status.get("outputs", [])}
            if refreshed_outputs != {monitor["name"] for monitor in all_monitors if not monitor.get("disabled", False)}:
                raise RuntimeError(f"computer_status did not refresh the active output list: {refreshed_outputs}")
            if len(all_monitors) > 1:
                try:
                    _, unselected_metadata = mcp.call("computer_start", {})
                except RuntimeError as error:
                    if "multiple active monitors" not in str(error):
                        raise RuntimeError(f"monitor selection returned the wrong error: {error}") from error
                else:
                    if unselected_metadata.get("session_id"):
                        mcp.call("computer_stop", {"session_id": unselected_metadata["session_id"]})
                    raise RuntimeError("computer_start did not require monitor selection for multiple outputs")
            start_arguments = {"output": target_monitor["name"]} if len(all_monitors) > 1 else {}
            started, metadata = mcp.call("computer_start", start_arguments)
            session_id = metadata["session_id"]
            forced_fallback_map = metadata.get("capabilities", {}).get("keyboard_layout", {})
            if xkb_fallback and forced_fallback_map.get("source") not in ("hyprland-rust-xkb", "embedded-us-fallback"):
                raise RuntimeError(f"Hyprland did not select the Rust XKB backend or embedded US fallback with libxkbcommon blocked: {metadata}")
            if metadata.get("active_backend") != "hyprland-native":
                raise RuntimeError(f"default route opened {metadata.get('active_backend')}, expected hyprland-native")
            if metadata.get("wayland_mode") != "auto":
                raise RuntimeError(f"default mode was not auto: {metadata.get('wayland_mode')}")
            display = metadata["display"]
            expected_logical_width, expected_logical_height = logical_output_size(target_monitor)
            if (display["origin_x"], display["origin_y"], display["logical_width"], display["logical_height"]) != (
                target_monitor["x"], target_monitor["y"], expected_logical_width, expected_logical_height
            ):
                raise RuntimeError(f"capture was not scoped to selected output {target_monitor['name']}: {display}")
            if high_resolution_output and max(display["width"], display["height"]) != 3200:
                raise RuntimeError(f"high-resolution capture was not downsampled to 3200 pixels: monitor={target_monitor}; display={display}")
            if not metadata.get("capabilities", {}).get("visual_pointer_overlay"):
                raise RuntimeError(f"native mouse-arrow overlay is unavailable: {metadata.get('capabilities')}")
            initial_pointer = json.loads(subprocess.check_output(["hyprctl", "cursorpos", "-j"], text=True))
            pointer_on_selected_output = output_contains(target_monitor, (initial_pointer["x"], initial_pointer["y"]))
            expected_hidden = original_cursor_invisible
            if metadata.get("capabilities", {}).get("native_cursor_hidden") is not expected_hidden:
                raise RuntimeError(f"native cursor state does not match the selected output and initial pointer: {metadata['capabilities']}")
            wait_cursor_visibility(expected_hidden)
            if pointer_on_selected_output:
                wait_pointer_overlay(target_monitor["name"], (initial_pointer["x"], initial_pointer["y"]))
            else:
                wait_pointer_overlay_offscreen(target_monitor)
            selected_point = (
                target_monitor["x"] + expected_logical_width // 2,
                target_monitor["y"] + expected_logical_height // 2,
            )
            other_monitor = next((monitor for monitor in all_monitors if monitor["name"] != target_monitor["name"]), None)
            if other_monitor:
                other_width, other_height = logical_output_size(other_monitor)
                other_point = (
                    other_monitor["x"] + other_width // 2,
                    other_monitor["y"] + other_height // 2,
                )
                move_compositor_pointer(*other_point)
                wait_cursor_visibility(original_cursor_invisible)
                wait_pointer_overlay_offscreen(target_monitor)
                move_compositor_pointer(*selected_point)
                selected_cursor = json.loads(subprocess.check_output(["hyprctl", "cursorpos", "-j"], text=True))
                if abs(selected_cursor["x"] - selected_point[0]) > 1 or abs(selected_cursor["y"] - selected_point[1]) > 1:
                    raise RuntimeError(f"test pointer did not enter selected output {target_monitor['name']}: requested={selected_point}, actual={selected_cursor}, output={target_monitor}")
                wait_pointer_overlay(target_monitor["name"], selected_point)
                wait_cursor_visibility(True)
            if not metadata.get("capabilities", {}).get("unicode_text_input"):
                raise RuntimeError(f"Unicode text input is unavailable: {metadata.get('capabilities')}")
            if not metadata.get("capabilities", {}).get("mouse_modifiers"):
                raise RuntimeError(f"mouse modifier input is unavailable: {metadata.get('capabilities')}")
            accessibility_available = status.get("capabilities", {}).get("accessibility_tree", False)
            accessibility = metadata.get("accessibility", {})
            ready_event = next(
                (json.loads(line) for line in event_log.read_text().splitlines() if json.loads(line).get("ready")),
                {},
            )
            click_image_bounds = next((
                node.get("image_bounds") for node in accessibility.get("nodes", [])
                if node.get("process_id") == window.get("pid") and (
                    node.get("name") == "Input receiver" if qt_receiver
                    else "editable" in node.get("states", [])
                ) and node.get("image_bounds")
            ), None)
            editor_image_bounds = next((
                node.get("image_bounds") for node in accessibility.get("nodes", [])
                if node.get("process_id") == window.get("pid")
                and node.get("name") == "Selectable sample text"
                and node.get("image_bounds")
            ), None)
            button_image_bounds = next((
                node.get("image_bounds") for node in accessibility.get("nodes", [])
                if node.get("process_id") == window.get("pid")
                and node.get("name") == "Click receiver"
                and node.get("image_bounds")
            ), None)
            if accessibility.get("available", False) != accessibility_available:
                raise RuntimeError(f"AT-SPI capability changed during startup: {accessibility}")
            if accessibility_available and not accessibility.get("nodes"):
                raise RuntimeError(f"AT-SPI returned no controls for the test receiver: {accessibility}")
            observation = metadata["observation_id"]
            xkb_layout_matrix = []
            xkb_layout_result = ""
            keyboard_layout = metadata.get("capabilities", {}).get("keyboard_layout", {})
            runtime_xkb_fallback = keyboard_layout.get("source") == "embedded-us-fallback"
            if runtime_xkb_fallback and "embedded US keymap" not in status_notes:
                raise RuntimeError(f"server used the embedded US map without reporting the missing XKB support: {status_notes}")
            if os.environ.get("COMPUTER_USE_XKB_LAYOUT_MATRIX") == "1":
                if runtime_xkb_fallback:
                    xkb_layout_result = ", active layout mapping unavailable; embedded US fallback verified"
                elif keyboard_layout.get("source") in ("hyprland-xkb", "hyprland-rust-xkb") and keyboard_layout.get("layout"):
                    original_layout = keyboard_layout["layout"]
                    expected_source = keyboard_layout["source"]
                    layouts = [layout for layout in ("us", "de", "fr", "it") if layout != original_layout]
                    layouts.append(original_layout)
                    try:
                        for layout in layouts:
                            events_before = event_log.read_text().splitlines() if event_log.exists() else []
                            set_runtime_xkb_layout(layout)
                            result, data = mcp.call(
                                "computer_act",
                                {
                                    "session_id": session_id,
                                    "observation_id": observation,
                                    "action": {"type": "keypress", "keys": ["CTRL", "SHIFT", "F35"]},
                                },
                            )
                            reported_keymap = data.get("keyboard_layout")
                            if data.get("performed") is not True or reported_keymap != {"layout": layout, "source": expected_source}:
                                raise RuntimeError(f"keyboard map did not refresh to {layout}: {data}")
                            observation = data.get("observation", {}).get("observation_id") or structured(result).get("observation", {}).get("observation_id")
                            if not observation:
                                raise RuntimeError(f"keyboard action for {layout} returned no fresh observation ID")
                            deadline = time.monotonic() + 3
                            while time.monotonic() < deadline:
                                events = [json.loads(line) for line in event_log.read_text().splitlines()]
                                delivered = next(
                                    (event for event in events[len(events_before):]
                                     if event.get("key") == "F35" and event.get("control") and event.get("shift")),
                                    None,
                                )
                                if delivered:
                                    break
                                time.sleep(0.05)
                            else:
                                raise RuntimeError(f"Qt/GTK receiver did not get Ctrl+Shift+F35 under {layout}: {events}")
                            xkb_layout_matrix.append(layout)
                    finally:
                        set_runtime_xkb_layout(original_layout)
                else:
                    raise RuntimeError(f"CI could not determine the keyboard layout mode: {keyboard_layout}")
            accessibility_mapping_available = metadata.get("accessibility_actions_available") is True
            accessibility_observation = metadata
            if accessibility_available:
                if not accessibility_mapping_available:
                    _, refreshed = mcp.call("computer_observe", {"session_id": session_id})
                    accessibility_observation = refreshed
                    accessibility = refreshed.get("accessibility", {})
                    accessibility_mapping_available = refreshed.get("accessibility_actions_available") is True
                    observation = refreshed["observation_id"]
                if not accessibility_mapping_available and not accessibility_observation.get("accessibility_actions_reason"):
                    raise RuntimeError(f"AT-SPI actions unavailable without a diagnostic reason: {accessibility_observation}")
                if os.environ.get("COMPUTER_USE_DEBUG_ATSPI") == "1" and not accessibility_mapping_available:
                    print(json.dumps({
                        "active_window": accessibility_observation.get("active_window"),
                        "display": accessibility_observation.get("display"),
                        "accessibility_actions_reason": accessibility_observation.get("accessibility_actions_reason"),
                        "test_window": {key: window.get(key) for key in ("class", "title", "monitor", "at", "size")},
                        "outputs": [{key: output.get(key) for key in ("name", "x", "y", "width", "height", "scale")} for output in all_monitors],
                        "nodes": [
                            {key: node.get(key) for key in ("element_id", "name", "role", "process_id", "bounds", "image_bounds")}
                            for node in accessibility.get("nodes", [])[:8]
                        ],
                    }, indent=2))
                if accessibility_mapping_available:
                    editable = next(
                        (
                            node for node in accessibility.get("nodes", [])
                            if node.get("process_id") == window.get("pid")
                            and "editable" in node.get("states", [])
                            and node.get("image_bounds")
                        ),
                        None,
                    )
                    if not editable:
                        raise RuntimeError(f"AT-SPI exposed no visible editable control: {accessibility}")
                    result, data = mcp.call(
                        "computer_act",
                        {
                            "session_id": session_id,
                            "observation_id": observation,
                            "action": {"type": "set_text", "element_id": editable["element_id"], "text": "AT-SPI text replacement"},
                        },
                    )
                    if data.get("performed") is not True:
                        raise RuntimeError(f"AT-SPI set_text failed: {data}")
                    observation = data.get("observation_id") or data.get("observation", {}).get("observation_id")
                    if not observation:
                        observation = structured(result).get("observation_id")
                    if not observation:
                        raise RuntimeError("AT-SPI set_text returned no fresh observation ID")
                    _, refreshed = mcp.call("computer_observe", {"session_id": session_id})
                    observation = refreshed["observation_id"]
                    accessibility = refreshed.get("accessibility", {})
                    button = next(
                        (
                            node for node in accessibility.get("nodes", [])
                            if node.get("process_id") == window.get("pid")
                            and (node.get("role") == "push button" if qt_receiver else node.get("name") == "Click receiver")
                            and any(isinstance(name, str) and name for name in node.get("actions", []))
                            and node.get("image_bounds")
                        ),
                        None,
                    )
                    if not button:
                        raise RuntimeError(f"AT-SPI exposed no visible actionable button: {accessibility}")
                    if button:
                        action_index = next(
                            index for index, name in enumerate(button["actions"])
                            if isinstance(name, str) and name
                        )
                        _, data = mcp.call(
                            "computer_act",
                            {
                                "session_id": session_id,
                                "observation_id": observation,
                                "action": {
                                    "type": "accessibility",
                                    "element_id": button["element_id"],
                                    "action_index": action_index,
                                },
                            },
                        )
                        if data.get("performed") is not True or data.get("action_index") != action_index:
                            raise RuntimeError(f"AT-SPI indexed action failed: {data}")
                        observation = data.get("observation_id") or data.get("observation", {}).get("observation_id")
                        if not observation:
                            raise RuntimeError("AT-SPI action returned no fresh observation ID")
                        deadline = time.monotonic() + 2
                        while time.monotonic() < deadline:
                            if event_log.exists() and any(
                                json.loads(line).get("button_click")
                                for line in event_log.read_text().splitlines()
                            ):
                                break
                            time.sleep(0.05)
                        else:
                            raise RuntimeError("AT-SPI action did not activate the button")
            if os.environ.get("COMPUTER_USE_REQUIRE_ATSPI_ACTIONS") == "1" and not accessibility_mapping_available:
                raise RuntimeError(f"CI requires mapped AT-SPI actions: {accessibility_observation.get('accessibility_actions_reason')}; {accessibility_observation}")
            if clipboard_checked:
                clipboard_text = "Hyprland clipboard: àèñ€Ω"
                clipboard_touched = True
                result, data = mcp.call(
                    "computer_act",
                    {"session_id": session_id, "action": {"type": "clipboard_write", "text": clipboard_text}},
                )
                if data.get("performed") is not True:
                    raise RuntimeError(f"clipboard write failed: {data}")
                external_clipboard = subprocess.run(
                    ["wl-paste", "--no-newline", "--type", "text/plain;charset=utf-8"],
                    capture_output=True,
                    timeout=3,
                )
                if external_clipboard.returncode or external_clipboard.stdout.decode("utf-8") != clipboard_text:
                    raise RuntimeError(f"native clipboard was not served to another Wayland client: {external_clipboard}")
                observation = data.get("observation_id") or data.get("observation", {}).get("observation_id") or structured(result).get("observation_id")
                result, data = mcp.call(
                    "computer_act",
                    {"session_id": session_id, "action": {"type": "clipboard_read"}},
                )
                if data.get("text") != clipboard_text:
                    raise RuntimeError(f"clipboard read returned unexpected text: {data}")
                observation = data.get("observation_id") or data.get("observation", {}).get("observation_id") or structured(result).get("observation_id")
                external_text = "external clipboard: café ✨"
                subprocess.run(
                    ["wl-copy", "--type", "text/plain;charset=utf-8"],
                    input=external_text.encode("utf-8"),
                    check=True,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    timeout=3,
                )
                result, data = mcp.call(
                    "computer_act",
                    {"session_id": session_id, "action": {"type": "clipboard_read"}},
                )
                if data.get("text") != external_text:
                    raise RuntimeError(f"native clipboard did not read another Wayland client's data: {data}")
                observation = data.get("observation_id") or data.get("observation", {}).get("observation_id") or structured(result).get("observation_id")
            image = next(item for item in started.get("content", []) if item.get("type") == "image")
            frame = base64.b64decode(image["data"], validate=True)
            if image.get("mimeType") != "image/jpeg" or not frame.startswith(b"\xff\xd8\xff"):
                raise RuntimeError("Hyprland did not return a valid JPEG screenshot")

            x, y = window["at"]
            width, height = window["size"]
            display = metadata["display"]
            move_x, move_y = x + width // 2, y + height // 2
            move_px, move_py = image_point(move_x, move_y, display)
            if qt_receiver and not click_image_bounds and ready_event.get("entry"):
                click_image_bounds = window_image_bounds(window, display, ready_event["entry"])
            if qt_receiver and not editor_image_bounds and ready_event.get("editor"):
                editor_image_bounds = window_image_bounds(window, display, ready_event["editor"])
            if qt_receiver and not button_image_bounds and ready_event.get("button"):
                button_image_bounds = window_image_bounds(window, display, ready_event["button"])
            if not click_image_bounds:
                raise RuntimeError(f"AT-SPI did not map the editable input to the screenshot: {accessibility}")
            if click_image_bounds:
                click_x = click_image_bounds["x"] + click_image_bounds["width"] // 2
                click_y = click_image_bounds["y"] + click_image_bounds["height"] // 2
            else:
                click_x, click_y = image_point(x + 300, y + height // 2, display)
            if qt_receiver and editor_image_bounds:
                scroll_x = editor_image_bounds["x"] + editor_image_bounds["width"] // 2
                scroll_y = editor_image_bounds["y"] + editor_image_bounds["height"] // 2
            else:
                scroll_x, scroll_y = image_point(x + width - 80, y + height - 50, display)
            if qt_receiver and editor_image_bounds:
                drag_x = editor_image_bounds["x"] + min(40, editor_image_bounds["width"] // 4)
                drag_to_x = editor_image_bounds["x"] + min(220, editor_image_bounds["width"] - 1)
                drag_y = drag_to_y = editor_image_bounds["y"] + editor_image_bounds["height"] // 2
                double_click_x = editor_image_bounds["x"] + min(60, editor_image_bounds["width"] // 4)
                double_click_y = editor_image_bounds["y"] + 20
            else:
                drag_x, drag_y = image_point(x + 280, y + height // 2, display)
                drag_to_x, drag_to_y = image_point(x + 500, y + height // 2, display)
                double_click_x, double_click_y = image_point(x + 280, y + 20, display)
            context_x, context_y = (
                (button_image_bounds["x"] + button_image_bounds["width"] // 2,
                 button_image_bounds["y"] + button_image_bounds["height"] // 2)
                if qt_receiver and button_image_bounds else (click_x, click_y)
            )
            stale_observation = observation
            actions = [
                {"type": "move", "x": move_px, "y": move_py},
                {"type": "click", "x": click_x, "y": click_y},
                {"type": "keypress", "keys": ["CTRL", "A"]},
                {"type": "keypress", "keys": ["CTRL", "SHIFT", "A"]},
                {"type": "keypress", "keys": ["SHIFT", "A"]},
                {"type": "keypress", "keys": ["ALT", "F24"]},
                {"type": "keypress", "keys": ["SUPER", "F24"]},
                {"type": "keypress", "key": "F35"},
                {"type": "keypress", "key": "KP_7"},
                {"type": "keypress", "key": "KP_ENTER"},
                {"type": "type", "text": "computer-use-linux-hyprland"},
                {"type": "keypress", "keys": ["CTRL", "A"]},
                {"type": "type", "text": "Hyprland Unicode: àèñ€Ω"},
                {"type": "keypress", "key": "ENTER"},
                *([{"type": "click", "x": button_image_bounds["x"] + button_image_bounds["width"] // 2, "y": button_image_bounds["y"] + button_image_bounds["height"] // 2}] if qt_receiver and button_image_bounds else []),
                {"type": "click", "x": context_x, "y": context_y, "keys": ["CTRL"]},
                {"type": "scroll", "x": scroll_x, "y": scroll_y, "scroll_y": 120},
                {"type": "scroll", "x": scroll_x, "y": scroll_y, "direction": "down", "amount": 1},
                {"type": "drag", "x": drag_x, "y": drag_y, "to_x": drag_to_x, "to_y": drag_y},
                {"type": "double_click", "x": double_click_x, "y": double_click_y},
                *[
                    {"type": "click", "x": context_x, "y": context_y, "button": button}
                    for button in ("middle", "back", "forward")
                ],
                {"type": "click", "x": context_x, "y": context_y, "button": "right"},
            ]
            for action in actions:
                try:
                    result, data = mcp.call(
                        "computer_act",
                        {"session_id": session_id, "observation_id": observation, "action": action},
                    )
                except Exception as error:
                    raise RuntimeError(
                        f"action {action} failed: {error}; display={display}; window={window}; "
                        f"editor_image_bounds={editor_image_bounds}; screenshot={display['width']}x{display['height']}"
                    ) from error
                if data.get("performed") is not True:
                    raise RuntimeError(f"action failed: {action}: {data}")
                observation = data.get("observation_id") or data.get("observation", {}).get("observation_id")
                if not observation:
                    observation = structured(result).get("observation_id")
                observation_error = None
                if not observation and structured(result).get("observe_again"):
                    observation_error = structured(result).get("observation_error")
                    for _ in range(8):
                        try:
                            _, fresh = mcp.call("computer_observe", {"session_id": session_id})
                        except RuntimeError as error:
                            observation_error = str(error)
                            if "observe again" not in observation_error:
                                raise
                            time.sleep(0.25)
                            continue
                        observation = fresh.get("observation_id")
                        if observation:
                            break
                if not observation:
                    raise RuntimeError(f"action returned no fresh observation ID: {action}: {structured(result)}; re-observe error={observation_error}")
                if action["type"] == "move":
                    pointer = json.loads(subprocess.check_output(["hyprctl", "cursorpos", "-j"], text=True))
                    expected = action.get("expected_pointer", [move_x, move_y])
                    if abs(pointer["x"] - expected[0]) > 1 or abs(pointer["y"] - expected[1]) > 1:
                        raise RuntimeError(f"Hyprland pointer did not reach requested position {expected}: {pointer}; window={window}; selected_monitor={target_monitor}")
                    if action.get("expected_pointer") is None:
                        wait_pointer_overlay(target_monitor["name"], (pointer["x"], pointer["y"]))
                    pointer_observation = check_agent_pointer_image(
                        mcp, session_id, target_monitor["name"], temp
                    )
                    observation = pointer_observation.get("observation_id")
                    if not observation:
                        raise RuntimeError("pointer-image observation returned no fresh observation ID")
                    events_before_stale_batch = [
                        json.loads(line) for line in event_log.read_text().splitlines()
                    ]
                    click_count_before = sum("click" in event for event in events_before_stale_batch)
                    ctrl_a_count_before = sum(event.get("key", "").lower() in ("a", "u+0061") and event.get("control") for event in events_before_stale_batch)
                    try:
                        mcp.call(
                            "computer_act",
                            {
                                "session_id": session_id,
                                "observation_id": stale_observation,
                                "actions": [
                                    {"type": "click", "x": click_x, "y": click_y},
                                    {"type": "keypress", "keys": ["CTRL", "A"]},
                                ],
                            },
                        )
                    except RuntimeError as error:
                        if "observation is stale or expired" not in str(error):
                            raise RuntimeError(f"stale observation was rejected for the wrong reason: {error}") from error
                    else:
                        raise RuntimeError("computer_act accepted an action based on a consumed observation")
                    after_rejection = json.loads(subprocess.check_output(["hyprctl", "cursorpos", "-j"], text=True))
                    if abs(after_rejection["x"] - pointer["x"]) > 1 or abs(after_rejection["y"] - pointer["y"]) > 1:
                        raise RuntimeError(f"stale action moved the pointer: before={pointer}; after={after_rejection}")
                    events_after_stale_batch = [
                        json.loads(line) for line in event_log.read_text().splitlines()
                    ]
                    if sum("click" in event for event in events_after_stale_batch) != click_count_before:
                        raise RuntimeError("stale batch delivered a click to the test application")
                    if sum(event.get("key", "").lower() in ("a", "u+0061") and event.get("control") for event in events_after_stale_batch) != ctrl_a_count_before:
                        raise RuntimeError("stale batch delivered a keypress to the test application")
                if action["type"] == "click":
                    pointer = json.loads(subprocess.check_output(["hyprctl", "cursorpos", "-j"], text=True))
                    expected = (
                        display["origin_x"] + round(action["x"] * display["logical_width"] / display["width"]),
                        display["origin_y"] + round(action["y"] * display["logical_height"] / display["height"]),
                    )
                    if abs(pointer["x"] - expected[0]) > 1 or abs(pointer["y"] - expected[1]) > 1:
                        raise RuntimeError(f"click pointer did not reach target {expected}: {pointer}; window={window}; display={display}")
                if action["type"] == "scroll":
                    expected_mode = "smooth" if "scroll_y" in action else "discrete"
                    if data.get("scroll_mode") != expected_mode:
                        raise RuntimeError(f"scroll mode mismatch for {action}: {data}")

            # Dismiss the context menu opened by right-click coverage before the cancellation drag.
            result, data = mcp.call(
                "computer_act",
                {
                    "session_id": session_id,
                    "observation_id": observation,
                    "action": {"type": "keypress", "key": "ESC"},
                },
            )
            if data.get("performed") is not True:
                raise RuntimeError(f"could not dismiss the test context menu: {data}")
            observation = data.get("observation_id") or data.get("observation", {}).get("observation_id")
            if not observation:
                observation = structured(result).get("observation_id")
            if not observation:
                raise RuntimeError("Escape returned no fresh observation ID")

            cancel_path = [
                {
                    "x": round(drag_x + (drag_to_x - drag_x) * index / 63),
                    "y": round(drag_y + (drag_to_y - drag_y) * index / 63),
                }
                for index in range(64)
            ]
            cancel_events_before = (
                [json.loads(line) for line in event_log.read_text().splitlines()]
                if event_log.exists() else []
            )
            cancel_request_id = mcp.send(
                "tools/call",
                {
                    "name": "computer_act",
                    "arguments": {
                        "session_id": session_id,
                        "observation_id": observation,
                        "action": {"type": "drag", "path": cancel_path},
                    },
                },
            )
            deadline = time.monotonic() + 3
            press_seen = False
            while time.monotonic() < deadline:
                cancel_events = (
                    [json.loads(line) for line in event_log.read_text().splitlines()]
                    if event_log.exists() else []
                )
                new_events = cancel_events[len(cancel_events_before):]
                press_seen = any(
                    event.get("button_press") == 1
                    for event in new_events
                )
                if press_seen:
                    break
                time.sleep(0.01)
            if not press_seen:
                response = mcp.responses.get(str(cancel_request_id))
                response_summary = None
                if response:
                    response_summary = {"jsonrpc_error": response.get("error")}
                    content = response.get("result", {}).get("content", [])
                    if content and content[0].get("text"):
                        try:
                            action_result = json.loads(content[0]["text"])
                            response_summary["action_result"] = {
                                key: action_result.get(key)
                                for key in ("performed", "action", "error")
                                if key in action_result
                            }
                        except json.JSONDecodeError:
                            response_summary["text"] = content[0]["text"][:300]
                mcp.cancel(cancel_request_id)
                raise RuntimeError(
                    "cancellation drag never delivered its button press: "
                    f"completion={response_summary}; events={cancel_events[len(cancel_events_before):]}"
                )
            mcp.cancel(cancel_request_id)
            _, refreshed = mcp.call("computer_observe", {"session_id": session_id})
            observation = refreshed.get("observation_id")
            if not observation:
                raise RuntimeError("computer_observe returned no observation after cancelling a drag")
            if str(cancel_request_id) in mcp.responses:
                raise RuntimeError("cancelled drag unexpectedly returned a normal completion response")
            deadline = time.monotonic() + 3
            release_seen = False
            while time.monotonic() < deadline:
                cancel_events = [json.loads(line) for line in event_log.read_text().splitlines()]
                new_events = cancel_events[len(cancel_events_before):]
                release_seen = any(
                    event.get("button_release") == 1
                    for event in new_events
                )
                if release_seen:
                    break
                time.sleep(0.01)
            if not release_seen:
                raise RuntimeError("cancelled drag left the primary pointer button pressed")

            deadline = time.monotonic() + 5
            events = []
            while time.monotonic() < deadline:
                if event_log.exists():
                    events = [json.loads(line) for line in event_log.read_text().splitlines()]
                if (
                    any(event.get("text") == "Hyprland Unicode: àèñ€Ω" for event in events)
                    and any("scroll" in event for event in events)
                    and any(event.get("selection") for event in events)
                ):
                    break
                time.sleep(0.05)

            received_buttons = {
                (event.get("click") or [None])[0] if qt_receiver else event.get("button")
                for event in events
            }
            checks = {
                "pointer and click": (
                    any(event.get("click") for event in events)
                    and any(event.get("cursor_position") != 20 for event in events)
                    and any(event.get("button_click") for event in events)
                ) if qt_receiver else any(event.get("click") for event in events),
                "accessibility action": not accessibility_mapping_available or (
                    any(event.get("text") == "AT-SPI text replacement" for event in events)
                    and any(event.get("button_click") for event in events)
                ),
                "Unicode text": any(event.get("text") == "Hyprland Unicode: àèñ€Ω" for event in events),
                "Ctrl+A": any(event.get("key", "").lower() in ("a", "u+0061") and event.get("control") for event in events),
                "Ctrl+Shift+A": any(event.get("key", "").lower() in ("a", "u+0061", "u+0041") and event.get("control") and event.get("shift") for event in events),
                "Shift+A": any(event.get("key_text") == "A" and event.get("shift") for event in events),
                "Alt+F24": any(event.get("key") == "F24" and event.get("alt") for event in events),
                "Super+F24": any(event.get("key") == "F24" and event.get("super") for event in events),
                "F35": any(event.get("key") == "F35" for event in events),
                "numpad 7": any(event.get("key") == "KP_7" for event in events),
                "numpad Enter": any(event.get("key") == "KP_Enter" for event in events),
                "Enter": any(event.get("key") == "Return" for event in events),
                "Ctrl+click": any(event.get("click") and event.get("control") for event in events),
                "middle/back/forward buttons": ({4, 8, 16} if qt_receiver else {2, 8, 9}) <= received_buttons,
                "scroll": sum("scroll" in event and event["scroll"][1] != 0 for event in events) >= 2,
                "drag selection": any(event.get("selection") for event in events),
                "double/context click": (
                    any(event.get("double_click") for event in events)
                    and any(event.get("context_menu") for event in events)
                ) if qt_receiver else (
                    any(event.get("click_count", 0) >= 2 for event in events)
                    and any(event.get("button") == 3 for event in events)
                ),
            }
            failed_checks = [name for name, passed in checks.items() if not passed]
            if failed_checks:
                raise RuntimeError(f"{('Qt' if qt_receiver else 'GTK')} input checks failed: {', '.join(failed_checks)}; events={events}; window={window}; click_target={(click_x, click_y)}; display={display}")
            clients = json.loads(subprocess.check_output(["hyprctl", "clients", "-j"], text=True))
            if any(client.get("class") == "hyprland-share-picker" for client in clients):
                raise RuntimeError("default Hyprland route unexpectedly opened the portal consent picker")

            mcp.call("computer_stop", {"session_id": session_id})
            session_id = None
            wait_overlay_closed()
            wait_cursor_visibility(original_cursor_invisible)

            move_compositor_pointer(*selected_point)
            _, fallback_metadata = mcp.call(
                "computer_start",
                {
                    "backend": "hyprland-grim",
                    "output": target_monitor["name"],
                },
            )
            session_id = fallback_metadata["session_id"]
            if fallback_metadata.get("active_backend") != "hyprland-grim":
                raise RuntimeError(
                    f"forced Grim session selected {fallback_metadata.get('active_backend')}: {fallback_metadata}"
                )
            wait_pointer_overlay(target_monitor["name"], selected_point)
            check_agent_pointer_image(mcp, session_id, target_monitor["name"], temp)
            mcp.call("computer_stop", {"session_id": session_id})
            session_id = None
            wait_overlay_closed()
            wait_cursor_visibility(original_cursor_invisible)

            if virtual_output:
                _, second_metadata = mcp.call(
                    "computer_start", {"output": virtual_output["name"]}
                )
                session_id = second_metadata["session_id"]
                second_display = second_metadata["display"]
                expected_logical_width, expected_logical_height = logical_output_size(virtual_output)
                if (second_display["origin_x"], second_display["origin_y"], second_display["logical_width"], second_display["logical_height"]) != (
                    virtual_output["x"], virtual_output["y"], expected_logical_width, expected_logical_height
                ):
                    raise RuntimeError(f"capture was not scoped to selected output {virtual_output['name']}: {second_display}")
                if headless_output:
                    wait_pointer_overlay(virtual_output["name"])
                    for scale in (1.5, virtual_output["scale"]):
                        scale_command = (
                            f'hl.monitor({{ output = {json.dumps(virtual_output["name"])}, mode = {json.dumps(output_mode)}, '
                            f'position = "{virtual_output["x"]}x{virtual_output["y"]}", scale = {scale}, '
                            f'transform = {virtual_output.get("transform", 0)}{output_bitdepth} }})'
                        )
                        changed = subprocess.run(
                            ["hyprctl", "eval", scale_command], capture_output=True, text=True, timeout=5
                        )
                        if changed.returncode or "error" in changed.stdout.lower():
                            raise RuntimeError(f"could not change selected output scale to {scale}: {changed.stderr or changed.stdout}")
                        deadline = time.monotonic() + 5
                        while time.monotonic() < deadline:
                            output_state = next(
                                monitor for monitor in json.loads(subprocess.check_output(["hyprctl", "monitors", "-j"], text=True))
                                if monitor["name"] == virtual_output["name"]
                            )
                            if abs(output_state["scale"] - scale) < 0.01:
                                break
                            time.sleep(0.05)
                        else:
                            raise RuntimeError(f"Hyprland did not apply output scale {scale}: {output_state}")
                        wait_pointer_overlay(virtual_output["name"])
                    _, second_metadata = mcp.call("computer_observe", {"session_id": session_id})
                    second_display = second_metadata["display"]
                    expected_logical_width, expected_logical_height = logical_output_size(virtual_output)
                    if (second_display["logical_width"], second_display["logical_height"]) != (
                        expected_logical_width, expected_logical_height
                    ):
                        raise RuntimeError(f"computer_observe did not refresh the restored output scale: {second_display}")
                old_observation_id = second_metadata["observation_id"]
                old_cursor = json.loads(subprocess.check_output(["hyprctl", "cursorpos", "-j"], text=True))
                relocated_x = virtual_output["x"] + 96
                monitor_command = (
                    f'hl.monitor({{ output = {json.dumps(virtual_output["name"])}, mode = {json.dumps(output_mode)}, '
                    f'position = "{relocated_x}x{virtual_output["y"]}", scale = {virtual_output["scale"]}, '
                    f'transform = {virtual_output.get("transform", 0)}{output_bitdepth} }})'
                )
                relocated = subprocess.run(
                    ["hyprctl", "eval", monitor_command], capture_output=True, text=True, timeout=5
                )
                if relocated.returncode or "error" in relocated.stdout.lower():
                    raise RuntimeError(f"could not move selected output during the session: {relocated.stderr or relocated.stdout}")
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    virtual_output = next(
                        monitor for monitor in json.loads(subprocess.check_output(["hyprctl", "monitors", "-j"], text=True))
                        if monitor["name"] == second_metadata["selected_output"]
                    )
                    if virtual_output["x"] == relocated_x:
                        break
                    time.sleep(0.05)
                if virtual_output["x"] != relocated_x:
                    raise RuntimeError(f"Hyprland did not move the selected output to x={relocated_x}: {virtual_output}")
                old_virtual_x = second_display["origin_x"] + round(second_display["logical_width"] / 2)
                old_virtual_y = second_display["origin_y"] + round(second_display["logical_height"] / 2)
                virtual_px, virtual_py = image_point(old_virtual_x, old_virtual_y, second_display)
                _, stale_action = mcp.call(
                    "computer_act",
                    {
                        "session_id": session_id,
                        "observation_id": old_observation_id,
                        "action": {"type": "move", "x": virtual_px, "y": virtual_py},
                    },
                )
                if stale_action.get("performed") is not False or "display layout changed since observation" not in stale_action.get("error", ""):
                    raise RuntimeError(f"action using the pre-relocation layout was not rejected: {stale_action}")
                cursor_after_stale_action = json.loads(subprocess.check_output(["hyprctl", "cursorpos", "-j"], text=True))
                if cursor_after_stale_action != old_cursor:
                    raise RuntimeError(f"stale layout action moved the pointer: {old_cursor} -> {cursor_after_stale_action}")
                _, second_metadata = mcp.call("computer_observe", {"session_id": session_id})
                second_display = second_metadata["display"]
                expected_logical_width, expected_logical_height = logical_output_size(virtual_output)
                if (second_display["origin_x"], second_display["origin_y"], second_display["logical_width"], second_display["logical_height"]) != (
                    virtual_output["x"], virtual_output["y"], expected_logical_width, expected_logical_height
                ):
                    raise RuntimeError(f"computer_observe did not refresh the moved output geometry: {second_display}")
                inset = 32
                targets = [
                    (virtual_output["x"] + inset, virtual_output["y"] + inset),
                    (virtual_output["x"] + expected_logical_width - inset - 1, virtual_output["y"] + inset),
                    (virtual_output["x"] + expected_logical_width - inset - 1, virtual_output["y"] + expected_logical_height - inset - 1),
                    (virtual_output["x"] + inset, virtual_output["y"] + expected_logical_height - inset - 1),
                    (virtual_output["x"] + round(expected_logical_width / 2), virtual_output["y"] + round(expected_logical_height / 2)),
                ]
                observation_id = second_metadata["observation_id"]
                for virtual_x, virtual_y in targets:
                    virtual_px, virtual_py = image_point(virtual_x, virtual_y, second_display)
                    result, data = mcp.call(
                        "computer_act",
                        {
                            "session_id": session_id,
                            "observation_id": observation_id,
                            "action": {
                                "type": "move",
                                "x": virtual_px,
                                "y": virtual_py,
                                "expected_pointer": [virtual_x, virtual_y],
                            },
                        },
                    )
                    if data.get("performed") is not True:
                        raise RuntimeError(f"pointer move to {(virtual_x, virtual_y)} on selected output failed: {data}")
                    pointer = json.loads(
                        subprocess.check_output(["hyprctl", "cursorpos", "-j"], text=True)
                    )
                    if abs(pointer["x"] - virtual_x) > 1 or abs(pointer["y"] - virtual_y) > 1:
                        raise RuntimeError(
                            f"pointer move on rotated output reached {(pointer['x'], pointer['y'])}, "
                            f"expected {(virtual_x, virtual_y)}; output={virtual_output}; "
                            f"display={second_display}"
                        )
                    wait_pointer_overlay(
                        virtual_output["name"],
                        (pointer["x"], pointer["y"]),
                    )
                    observation_id = data.get("observation_id") or data.get("observation", {}).get("observation_id")
                    if not observation_id:
                        observation_id = structured(result).get("observation_id")
                    if not observation_id:
                        raise RuntimeError(f"pointer move returned no fresh observation ID: {data}")
                mcp.call("computer_stop", {"session_id": session_id})
                session_id = None
                wait_overlay_closed()
            stale_bundle = check_overlay_parent_death(binary, target_monitor["name"])
            check_overlay_child_death(binary, target_monitor["name"])
            check_native_runtime_without_path(binary, target_monitor["name"], stale_bundle)
            accessibility_result = (
                f"AT-SPI ({len(accessibility.get('nodes', []))} nodes, bounds {'mapped' if accessibility_mapping_available else 'unmapped'})"
                if accessibility_available else "visual-only"
            )
            clipboard_result = "clipboard read/write" if clipboard_checked else "clipboard skipped (unavailable or non-text clipboard content)"
            layout_result = (
                f", mixed-scale rotated mapping to {virtual_output['name']}, live output relocation and stale-layout rejection"
                if virtual_output else ""
            )
            if headless_output:
                layout_result += ", live output scale changes"
            if high_resolution_output:
                layout_result += f", 3840-pixel capture downsampled to 3200, output format {virtual_output.get('currentFormat', 'unknown')}"
            if xkb_layout_matrix:
                layout_result += f", live XKB layout refresh {'→'.join(xkb_layout_matrix)}"
            layout_result += xkb_layout_result
            toolkit = "Qt 6" if qt_receiver else "GTK 4"
            scroll_result = "smooth/discrete scroll"
            keyboard_result = "embedded-US keyboard fallback" if runtime_xkb_fallback else "Rust XKB keyboard" if keyboard_layout.get("source") == "hyprland-rust-xkb" else "keyboard"
            print(f"Hyprland {toolkit} smoke passed without portal consent: native and Grim screenshots with the styled AI pointer, pointer movement, left/middle/right/back/forward/double click, Ctrl+click, {keyboard_result}, Unicode text, stale-observation rejection, {scroll_result}, drag selection and cancellation release, {accessibility_result}, {clipboard_result}{layout_result}, graceful and abrupt cleanup, overlay child recovery and parent-death cleanup, stale XKB bundle reap, empty-PATH native startup and capture")
        finally:
            if session_id:
                try:
                    mcp.call("computer_stop", {"session_id": session_id})
                except Exception:
                    pass
            mcp.close()
            if receiver.poll() is None:
                receiver.terminate()
            try:
                receiver.wait(timeout=5)
            except subprocess.TimeoutExpired:
                receiver.kill()
                receiver.wait(timeout=5)
            if clipboard_touched:
                try:
                    if clipboard_backup is not None and clipboard_mime:
                        subprocess.run(["wl-copy", "--type", clipboard_mime], input=clipboard_backup, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5)
                        restored = subprocess.run(["wl-paste", "--no-newline", "--type", clipboard_mime], capture_output=True, timeout=5)
                        if restored.returncode or restored.stdout != clipboard_backup:
                            raise RuntimeError("original clipboard text did not survive smoke cleanup")
                    elif clipboard_was_empty:
                        subprocess.run(["wl-copy", "--clear"], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5)
                        remaining = subprocess.run(["wl-paste", "--list-types"], capture_output=True, timeout=5)
                        empty = remaining.returncode == 1 and remaining.stderr.strip() == b"Nothing is copied"
                        empty |= remaining.returncode == 0 and not remaining.stdout.strip()
                        if not empty:
                            raise RuntimeError("smoke cleanup did not restore the initially empty clipboard")
                except Exception as error:
                    clipboard_restore_error = error
            try:
                created_outputs = []
                if headless_output:
                    monitors = json.loads(subprocess.check_output(["hyprctl", "monitors", "-j"], text=True, timeout=5))
                    created_outputs = [monitor["name"] for monitor in monitors if monitor["name"] not in original_monitors]
                for output_name in created_outputs:
                    subprocess.run(["hyprctl", "output", "remove", output_name], check=True, capture_output=True, text=True, timeout=5)
                if created_outputs:
                    if original_focused_monitor and original_focused_monitor in original_monitors:
                        subprocess.run(
                            ["hyprctl", "eval", f'hl.dispatch(hl.dsp.focus({{ monitor = "{original_focused_monitor}" }}))'],
                            check=True,
                            capture_output=True,
                            text=True,
                            timeout=5,
                        )
                    if original_focused_workspace is not None:
                        subprocess.run(
                            ["hyprctl", "eval", f'hl.dispatch(hl.dsp.focus({{ workspace = {original_focused_workspace} }}))'],
                            check=True,
                            capture_output=True,
                            text=True,
                            timeout=5,
                        )
            finally:
                subprocess.run(
                    ["hyprctl", "eval", f"hl.dispatch(hl.dsp.cursor.move({{ x = {original['x']}, y = {original['y']} }}))"],
                    check=True,
                    capture_output=True,
                    text=True,
                    timeout=3,
                )
            if clipboard_restore_error is not None:
                raise RuntimeError(f"could not restore the original Wayland clipboard state: {clipboard_restore_error}") from clipboard_restore_error


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("server", help="path to computer-use-linux")
    parser.add_argument("--headless-output", action="store_true", help="create and remove a temporary 125%% portrait output to verify mixed-scale, rotated layout mapping")
    parser.add_argument("--high-resolution-output", action="store_true", help="set the temporary headless output to 3840x2160 and verify capture downsampling (use in an isolated Hyprland session)")
    parser.add_argument("--qt", action="store_true", help="run the Qt 6 Wayland receiver instead of the GTK 4 receiver")
    parser.add_argument("--xkb-fallback", action="store_true", help="hide libxkbcommon and verify the Rust XKB backend or embedded US fallback")
    parser.add_argument("--hyprland-only-build", action="store_true", help="require the binary to exclude experimental backends")
    args = parser.parse_args()
    run(
        str(Path(args.server).resolve()),
        args.headless_output or args.high_resolution_output,
        args.qt,
        args.xkb_fallback,
        args.high_resolution_output,
        args.hyprland_only_build,
    )
