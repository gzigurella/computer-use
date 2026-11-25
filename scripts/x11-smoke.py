#!/usr/bin/env python3
"""Exercise X11 capture and input against a GTK test window."""

import base64
import ctypes
import json
import os
import selectors
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from mcp_result import validate_tool_result


class X11Pointer:
    def __init__(self):
        self.lib = ctypes.CDLL("libX11.so.6")
        self.lib.XOpenDisplay.argtypes = [ctypes.c_char_p]
        self.lib.XOpenDisplay.restype = ctypes.c_void_p
        self.lib.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
        self.lib.XDefaultRootWindow.restype = ctypes.c_ulong
        self.lib.XQueryPointer.argtypes = [
            ctypes.c_void_p,
            ctypes.c_ulong,
            ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.c_int),
            ctypes.POINTER(ctypes.c_int),
            ctypes.POINTER(ctypes.c_int),
            ctypes.POINTER(ctypes.c_int),
            ctypes.POINTER(ctypes.c_uint),
        ]
        self.lib.XQueryPointer.restype = ctypes.c_int
        self.lib.XCloseDisplay.argtypes = [ctypes.c_void_p]
        self.display = self.lib.XOpenDisplay(None)
        if not self.display:
            raise RuntimeError("cannot connect to the X11 display")

    def position(self):
        root = self.lib.XDefaultRootWindow(self.display)
        root_return = ctypes.c_ulong()
        child_return = ctypes.c_ulong()
        root_x, root_y = ctypes.c_int(), ctypes.c_int()
        window_x, window_y = ctypes.c_int(), ctypes.c_int()
        mask = ctypes.c_uint()
        if not self.lib.XQueryPointer(
            self.display,
            root,
            ctypes.byref(root_return),
            ctypes.byref(child_return),
            ctypes.byref(root_x),
            ctypes.byref(root_y),
            ctypes.byref(window_x),
            ctypes.byref(window_y),
            ctypes.byref(mask),
        ):
            raise RuntimeError("XQueryPointer failed")
        return root_x.value, root_y.value

    def close(self):
        if self.display:
            self.lib.XCloseDisplay(self.display)
            self.display = None


class McpProcess:
    def __init__(self, binary):
        env = os.environ.copy()
        env["XDG_SESSION_TYPE"] = "x11"
        env.pop("WAYLAND_DISPLAY", None)
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
    return result


def observation_id(result):
    structured = structured_content(result)
    nested = structured.get("observation", {})
    return nested.get("observation_id") or structured.get("observation_id")


def read_events(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines()]


def wait_for_event(path, receiver, predicate, timeout=5):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        events = read_events(path)
        match = next((event for event in events if predicate(event)), None)
        if match:
            return match
        if receiver.poll() is not None:
            raise RuntimeError("GTK X11 input receiver exited before recording the expected event")
        time.sleep(0.05)
    raise AssertionError(f"expected X11 input event did not reach the test window: {read_events(path)}")


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: x11-smoke.py PATH_TO_COMPUTER_USE_LINUX")
    binary = os.path.abspath(sys.argv[1])
    pointer = X11Pointer()
    mcp = McpProcess(binary)
    receiver = None
    receiver_stderr = None
    session_id = None
    original = pointer.position()
    try:
        with tempfile.TemporaryDirectory(prefix="computer-use-x11-") as temp:
            root = Path(temp)
            event_log = root / "input.jsonl"
            receiver_env = os.environ.copy()
            receiver_env["XDG_SESSION_TYPE"] = "x11"
            receiver_env["GDK_BACKEND"] = "x11"
            receiver_env.pop("WAYLAND_DISPLAY", None)
            receiver_stderr = (root / "gtk-x11.log").open("w")
            receiver = subprocess.Popen(
                [sys.executable, str(Path(__file__).with_name("wayland-gtk-receiver.py")), str(event_log)],
                env=receiver_env,
                stdout=subprocess.DEVNULL,
                stderr=receiver_stderr,
            )
            wait_for_event(event_log, receiver, lambda event: event.get("ready"))

            status = structured_content(mcp.call("computer_status", {}))
            assert status["backend"] == "x11", status["backend"]
            for capability in ("screenshot", "pointer_input", "keyboard_input", "scroll_input"):
                assert status["capabilities"][capability], f"X11 lacks {capability}"

            started = mcp.call("computer_start", {})
            metadata = structured_content(started)
            session_id = metadata["session_id"]
            observation = observation_id(started)
            assert metadata["active_backend"] == "x11", metadata["active_backend"]
            image = next(item for item in started["content"] if item.get("type") == "image")
            jpeg = base64.b64decode(image["data"], validate=True)
            assert image["mimeType"] == "image/jpeg" and jpeg.startswith(b"\xff\xd8\xff")

            target_x, target_y = 300, 200
            moved = mcp.call(
                "computer_act",
                {
                    "session_id": session_id,
                    "observation_id": observation,
                    "action": {"type": "move", "x": target_x, "y": target_y},
                },
            )
            observation = observation_id(moved)
            deadline = time.monotonic() + 2
            while pointer.position() != (target_x, target_y) and time.monotonic() < deadline:
                time.sleep(0.02)
            assert pointer.position() == (target_x, target_y), "XTEST pointer motion did not reach the X11 server"

            actions = (
                {"type": "click", "x": 150, "y": target_y},
                {"type": "keypress", "key": "F24"},
                {"type": "type", "text": "clear-this-with-ctrl-a"},
                {"type": "keypress", "keys": ["CTRL", "A"]},
                {"type": "type", "text": "computer-use-linux-x11"},
                {"type": "keypress", "key": "ENTER"},
                {"type": "scroll", "x": 480, "y": target_y, "scroll_y": 120},
                {"type": "drag", "x": 280, "y": target_y, "to_x": 500, "to_y": target_y},
            )
            for action in actions:
                result = mcp.call(
                    "computer_act",
                    {"session_id": session_id, "observation_id": observation, "action": action},
                )
                observation = observation_id(result)
                assert structured_content(result).get("performed") is True, action

            wait_for_event(event_log, receiver, lambda event: event.get("text") == "computer-use-linux-x11")
            wait_for_event(event_log, receiver, lambda event: event.get("key") == "F24")
            wait_for_event(event_log, receiver, lambda event: event.get("key") == "Return")
            wait_for_event(event_log, receiver, lambda event: event.get("scroll", [0, 0])[1] != 0)
            wait_for_event(event_log, receiver, lambda event: bool(event.get("selection")))
            assert any(event.get("click") for event in read_events(event_log))

            restored = mcp.call(
                "computer_act",
                {
                    "session_id": session_id,
                    "observation_id": observation,
                    "action": {"type": "move", "x": original[0], "y": original[1]},
                },
            )
            observation = observation_id(restored)
            deadline = time.monotonic() + 2
            while pointer.position() != original and time.monotonic() < deadline:
                time.sleep(0.02)
            assert pointer.position() == original, "XTEST pointer motion did not restore its original position"

            mcp.call("computer_stop", {"session_id": session_id})
            session_id = None
            print("X11 smoke passed: capture, pointer, click, keyboard, text, scroll, drag selection, and cleanup")
    finally:
        if session_id:
            try:
                mcp.call("computer_stop", {"session_id": session_id})
            except Exception:
                pass
        if receiver and receiver.poll() is None:
            receiver.terminate()
            try:
                receiver.wait(timeout=5)
            except subprocess.TimeoutExpired:
                receiver.kill()
                receiver.wait(timeout=5)
        if receiver_stderr:
            receiver_stderr.close()
        pointer.close()
        mcp.close()


if __name__ == "__main__":
    main()
