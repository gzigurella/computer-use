#!/usr/bin/env python3
"""Exercise the Wayland compositor fallback under a headless Sway session."""

import base64
import json
import os
import re
import selectors
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from mcp_result import validate_tool_result


class McpProcess:
    def __init__(self, binary, env):
        self.process = subprocess.Popen(
            [binary, "--stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=env,
        )
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.request_id = 0

    def call(self, name, arguments, timeout=60):
        self.request_id += 1
        request = {
            "jsonrpc": "2.0",
            "id": self.request_id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        }
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()
        if not self.selector.select(timeout):
            raise TimeoutError(f"MCP {name} timed out")
        response = json.loads(self.process.stdout.readline())
        if "error" in response:
            raise RuntimeError(f"MCP {name} failed: {response['error']}")
        result = response["result"]
        if result.get("isError"):
            details = result.get("content", [{}])[0].get("text", "unknown error")
            raise RuntimeError(f"MCP {name} failed: {details}")
        return result

    def close(self):
        self.selector.close()
        if self.process.stdin and not self.process.stdin.closed:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            self.process.wait(timeout=5)


def observation_id(result):
    structured = structured_content(result)
    return structured.get("observation", {}).get("observation_id") or structured.get("observation_id")


def structured_content(result):
    validate_tool_result(result)
    structured = result.get("structuredContent")
    if isinstance(structured, dict):
        return structured
    for item in result.get("content", []):
        if item.get("type") == "text":
            try:
                value = json.loads(item["text"])
            except (KeyError, TypeError, json.JSONDecodeError):
                continue
            if isinstance(value, dict):
                return value
    raise RuntimeError(f"MCP result has no structured content: {result}")


def find_window(node, app_id):
    if node.get("app_id") == app_id:
        return node
    for child in node.get("nodes", []) + node.get("floating_nodes", []):
        found = find_window(child, app_id)
        if found:
            return found
    return None


def focused_app(node):
    if node.get("focused") and node.get("app_id"):
        return node["app_id"]
    for child in node.get("nodes", []) + node.get("floating_nodes", []):
        app_id = focused_app(child)
        if app_id:
            return app_id
    return None


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: wayland-smoke.py PATH_TO_COMPUTER_USE_LINUX")

    binary = os.path.abspath(sys.argv[1])
    with tempfile.TemporaryDirectory(prefix="computer-use-wayland-") as temp:
        root = Path(temp)
        runtime = root / "runtime"
        runtime.mkdir(mode=0o700)
        config = root / "sway.conf"
        config.write_text("output * mode 1280x720\nseat * fallback true\nfocus_follows_mouse no\n")
        env = os.environ.copy()
        for name in (
            "DISPLAY",
            "HYPRLAND_INSTANCE_SIGNATURE",
            "SWAYSOCK",
            "WAYLAND_DISPLAY",
        ):
            env.pop(name, None)
        env.update(
            {
                "XDG_CURRENT_DESKTOP": "sway",
                "XDG_SESSION_TYPE": "wayland",
                "XDG_RUNTIME_DIR": str(runtime),
                "WAYLAND_DISPLAY": "wayland-1",
                "GDK_BACKEND": "wayland",
                "GSK_RENDERER": "cairo",
                "WLR_BACKENDS": "headless",
                "WLR_HEADLESS_OUTPUTS": "2",
                "WLR_LIBINPUT_NO_DEVICES": "1",
                "WLR_RENDERER": "pixman",
                "WLR_RENDERER_ALLOW_SOFTWARE": "1",
            }
        )

        with (root / "sway.log").open("w+") as sway_log:
            sway = subprocess.Popen(
                ["sway", "--unsupported-gpu", "-c", str(config)],
                env=env,
                stdout=subprocess.DEVNULL,
                stderr=sway_log,
            )
            mcp = None
            session_id = None
            receiver = None
            receiver_stderr = None
            blocker = None
            try:
                deadline = time.monotonic() + 20
                sockets = []
                ipc_ready = False
                while time.monotonic() < deadline:
                    if sway.poll() is not None:
                        raise RuntimeError("headless Sway exited before opening its IPC socket")
                    sockets = list(runtime.glob("sway-ipc.*.sock"))
                    if sockets and (runtime / "wayland-1").exists():
                        env["SWAYSOCK"] = str(sockets[0])
                        ready = subprocess.run(
                            ["swaymsg", "-t", "get_version", "-r"],
                            env=env,
                            stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL,
                            check=False,
                        )
                        if ready.returncode == 0:
                            ipc_ready = True
                            break
                    time.sleep(0.1)
                if len(sockets) != 1:
                    raise RuntimeError(f"expected one Sway IPC socket, found {len(sockets)}")
                if not ipc_ready:
                    raise RuntimeError("headless Sway IPC did not become ready")

                typed_text = "computer-use-linux-wayland"
                prefix_text = "clear-this-with-ctrl-a"
                received_input = root / "input.jsonl"
                receiver_stderr = (root / "gtk-wayland.log").open("w")
                receiver_env = env.copy()
                receiver_env["WAYLAND_DEBUG"] = "1"
                receiver = subprocess.Popen(
                    [
                        sys.executable,
                        str(Path(__file__).with_name("wayland-gtk-receiver.py")),
                        str(received_input),
                    ],
                    env=receiver_env,
                    stdout=subprocess.DEVNULL,
                    stderr=receiver_stderr,
                )
                deadline = time.monotonic() + 10
                window = None
                while time.monotonic() < deadline:
                    if receiver.poll() is not None:
                        receiver_stderr.flush()
                        detail = (root / "gtk-wayland.log").read_text(errors="replace")[-4000:]
                        raise RuntimeError(f"Wayland input receiver exited before opening its window: {detail}")
                    tree = subprocess.run(
                        ["swaymsg", "-t", "get_tree", "-r"],
                        env=env,
                        capture_output=True,
                        text=True,
                        check=True,
                    )
                    window = find_window(json.loads(tree.stdout), "org.example.computerusesmoke")
                    if window:
                        break
                    time.sleep(0.1)
                if not window:
                    raise RuntimeError("Wayland text target window did not appear in Sway")
                outputs = subprocess.run(
                    ["swaymsg", "-t", "get_outputs", "-r"],
                    env=env,
                    capture_output=True,
                    text=True,
                    check=True,
                )
                outputs = [output for output in json.loads(outputs.stdout) if output["active"]]
                outputs.sort(key=lambda output: output["rect"]["x"])
                if len(outputs) < 2:
                    raise RuntimeError("headless Sway did not expose two active outputs")
                right_output = outputs[-1]
                subprocess.run(
                    [
                        "swaymsg",
                        f'[app_id="org.example.computerusesmoke"] move container to output {right_output["name"]}',
                    ],
                    env=env,
                    capture_output=True,
                    text=True,
                    check=True,
                )

                blocker = subprocess.Popen(
                    ["foot", "--app-id", "computer-use-blocker", "sh", "-c", "sleep 20"],
                    env=env,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    if blocker.poll() is not None:
                        raise RuntimeError("background Wayland window exited before opening")
                    tree = subprocess.run(
                        ["swaymsg", "-t", "get_tree", "-r"],
                        env=env,
                        capture_output=True,
                        text=True,
                        check=True,
                    )
                    tree = json.loads(tree.stdout)
                    window = find_window(tree, "org.example.computerusesmoke")
                    other_window = find_window(tree, "computer-use-blocker")
                    if window and other_window:
                        break
                    time.sleep(0.1)
                if not window or not other_window:
                    receiver.terminate()
                    receiver.wait(timeout=5)
                    app_ids = []
                    if "tree" in locals():
                        def collect_app_ids(node):
                            if node.get("app_id"):
                                app_ids.append(node["app_id"])
                            for child in node.get("nodes", []) + node.get("floating_nodes", []):
                                collect_app_ids(child)
                        collect_app_ids(tree)
                    receiver_stderr.flush()
                    detail = (root / "gtk-wayland.log").read_text(errors="replace")[-4000:]
                    raise RuntimeError(
                        f"Wayland test windows did not appear in Sway "
                        f"(receiver={bool(window)}, blocker={bool(other_window)}, "
                        f"app_ids={app_ids}, receiver_stderr={detail})"
                    )
                if window["rect"]["x"] < right_output["rect"]["x"]:
                    raise RuntimeError("Sway did not move the text target to the right-hand output")
                if focused_app(tree) != "computer-use-blocker":
                    raise RuntimeError("Sway did not focus the background window before the test")
                rect = window["rect"]
                target_x = rect["x"] + rect["width"] // 2
                target_y = rect["y"] + rect["height"] // 2
                mcp = McpProcess(binary, env)

                status = structured_content(mcp.call("computer_status", {}))
                assert status.get("backend") == "sway-grim", status
                for capability in ("screenshot", "pointer_input", "keyboard_input", "scroll_input"):
                    assert status["capabilities"][capability], f"Sway lacks {capability}"

                started = mcp.call("computer_start", {"wayland_mode": "compositor"})
                metadata = structured_content(started)
                session_id = metadata["session_id"]
                observation = metadata["observation_id"]
                assert metadata["active_backend"] == "sway-grim", metadata["active_backend"]
                assert metadata["accessibility"].get("available") == status["capabilities"]["accessibility_tree"]
                if status["capabilities"]["accessibility_tree"]:
                    assert metadata["accessibility"].get("nodes"), metadata["accessibility"]
                image = next(item for item in started["content"] if item.get("type") == "image")
                jpeg = base64.b64decode(image["data"], validate=True)
                assert image["mimeType"] == "image/jpeg" and jpeg.startswith(b"\xff\xd8\xff")

                for action in (
                    {"type": "move", "x": target_x, "y": target_y},
                    {"type": "click", "x": rect["x"] + 150, "y": rect["y"] + rect["height"] // 2},
                    {"type": "keypress", "key": "F12"},
                    {"type": "type", "text": prefix_text},
                    {"type": "keypress", "keys": ["CTRL", "A"]},
                    {"type": "type", "text": typed_text},
                    {"type": "keypress", "key": "ENTER"},
                    {"type": "scroll", "x": target_x, "y": target_y, "scroll_y": 120},
                    {
                        "type": "drag",
                        "x": rect["x"] + 270,
                        "y": rect["y"] + rect["height"] // 2,
                        "to_x": rect["x"] + 500,
                        "to_y": rect["y"] + rect["height"] // 2,
                    },
                ):
                    result = mcp.call(
                        "computer_act",
                        {
                            "session_id": session_id,
                            "observation_id": observation,
                            "action": action,
                        },
                    )
                    observation = observation_id(result)
                    result_data = structured_content(result)
                    assert result_data.get("performed") is True, action
                    if action["type"] == "click":
                        tree = subprocess.run(
                            ["swaymsg", "-t", "get_tree", "-r"],
                            env=env,
                            capture_output=True,
                            text=True,
                            check=True,
                        )
                        assert focused_app(json.loads(tree.stdout)) == "org.example.computerusesmoke"
                        time.sleep(0.1)

                deadline = time.monotonic() + 5
                events = []
                while time.monotonic() < deadline:
                    if received_input.exists():
                        events = [json.loads(line) for line in received_input.read_text().splitlines()]
                    received_text = any(event.get("text") == typed_text for event in events)
                    received_scroll = any("scroll" in event for event in events)
                    received_key = any("key" in event for event in events)
                    received_function_key = any(event.get("key") == "F12" for event in events)
                    if received_text and received_scroll and received_key and received_function_key:
                        break
                    time.sleep(0.05)
                if not any(event.get("text") == typed_text for event in events):
                    raise RuntimeError(f"typed Wayland text did not reach the focused application: {events}")
                if not any("click" in event for event in events):
                    raise RuntimeError(f"Wayland click did not reach the focused application: {events}")
                if not any(event.get("scroll", [0, 0])[1] != 0 for event in events if "scroll" in event):
                    raise RuntimeError(f"Wayland smooth scroll did not reach the focused application: {events}")
                ctrl_mask = int(1 << 2)
                if not any(event.get("key") == "a" and event.get("modifiers", 0) & ctrl_mask for event in events):
                    raise RuntimeError(f"Wayland Ctrl+A did not reach the focused application: {events}")
                if not any(event.get("key") == "Return" for event in events):
                    raise RuntimeError(f"Wayland Enter did not reach the focused application: {events}")
                if not any(event.get("key") == "F12" for event in events):
                    raise RuntimeError(f"Wayland F12 did not reach the application: {events}")
                selections = [event["selection"] for event in events if event.get("selection")]
                if not any(selection for selection in selections):
                    raise RuntimeError(f"Wayland drag did not select text in GTK: {events}")
                receiver_stderr.flush()
                pointer_events = []
                for line in (root / "gtk-wayland.log").read_text(errors="replace").splitlines():
                    pointer = re.search(r"wl_pointer[#@](\d+)\.button\(\d+, \d+, (\d+), ([01])\)", line)
                    if pointer:
                        pointer_events.append(("button", pointer.group(1), pointer.group(2), pointer.group(3)))
                    else:
                        motion = re.search(r"wl_pointer[#@](\d+)\.motion\(", line)
                        if motion:
                            pointer_events.append(("motion", motion.group(1), None, None))
                drag_press = next((i for i in range(len(pointer_events) - 1, -1, -1)
                                   if pointer_events[i][0] == "button" and pointer_events[i][3] == "1"), None)
                drag_release = next((i for i in range(drag_press + 1, len(pointer_events))
                                     if pointer_events[i][0] == "button"
                                     and pointer_events[i][1:3] == pointer_events[drag_press][1:3]
                                     and pointer_events[i][3] == "0"), None) if drag_press is not None else None
                if (
                    drag_press is None
                    or drag_release is None
                    or not any(event[0] == "motion" and event[1] == pointer_events[drag_press][1]
                               for event in pointer_events[drag_press + 1:drag_release])
                ):
                    raise RuntimeError(f"Wayland drag motion was not delivered between button press and release: {pointer_events}")

                mcp.call("computer_stop", {"session_id": session_id})
                session_id = None
                print("Wayland smoke passed: Sway capture, GTK input, Enter, Ctrl+A, F12, smooth scroll, drag, and cleanup")
            except Exception as error:
                sway_log.flush()
                sway_log.seek(0)
                log = sway_log.read()[-4000:]
                raise RuntimeError(f"Wayland smoke failed: {error}\nSway log:\n{log}") from error
            finally:
                if mcp:
                    if session_id:
                        try:
                            mcp.call("computer_stop", {"session_id": session_id})
                        except Exception:
                            pass
                    mcp.close()
                if receiver and receiver.poll() is None:
                    receiver.terminate()
                    try:
                        receiver.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        receiver.kill()
                        receiver.wait(timeout=5)
                if receiver_stderr:
                    receiver_stderr.close()
                if blocker and blocker.poll() is None:
                    blocker.terminate()
                    try:
                        blocker.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        blocker.kill()
                        blocker.wait(timeout=5)
                sway.terminate()
                try:
                    sway.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    sway.kill()
                    sway.wait(timeout=5)


if __name__ == "__main__":
    main()
