#!/usr/bin/env python3
"""Exercise an explicitly consented XDG ScreenCast session on the current Wayland desktop."""

import argparse
import base64
import json
import selectors
import subprocess
import time


class McpProcess:
    def __init__(self, binary):
        self.process = subprocess.Popen(
            [binary, "--stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.request_id = 0

    def rpc(self, method, params=None, timeout=280):
        self.request_id += 1
        request_id = self.request_id
        request = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            request["params"] = params
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if not self.selector.select(max(0, deadline - time.monotonic())):
                break
            response = json.loads(self.process.stdout.readline())
            if response.get("id") == request_id:
                if "error" in response:
                    raise RuntimeError(f"MCP {method} failed: {response['error']}")
                return response.get("result", {})
        raise TimeoutError(f"MCP {method} timed out")

    def notify(self, method, params=None):
        message = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def call(self, name, arguments, timeout=280):
        result = self.rpc(
            "tools/call", {"name": name, "arguments": arguments}, timeout=timeout
        )
        if result.get("isError"):
            detail = result.get("content", [{}])[0].get("text", "unknown error")
            raise RuntimeError(f"{name} failed: {detail}")
        if "content" not in result and "structuredContent" not in result:
            return result, result
        structured = result.get("structuredContent")
        if isinstance(structured, dict):
            return result, structured
        for item in result.get("content", []):
            if item.get("type") == "text":
                try:
                    value = json.loads(item["text"])
                except (KeyError, TypeError, json.JSONDecodeError):
                    continue
                if isinstance(value, dict):
                    return result, value
        raise RuntimeError(f"{name} returned no structured metadata")

    def close(self):
        self.selector.close()
        if not self.process.stdin.closed:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            self.process.wait(timeout=5)


def check_jpeg(result):
    image = next(item for item in result.get("content", []) if item.get("type") == "image")
    jpeg = base64.b64decode(image["data"], validate=True)
    if image.get("mimeType") != "image/jpeg" or not jpeg.startswith(b"\xff\xd8\xff"):
        raise AssertionError("ScreenCast did not return a valid JPEG frame")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("server", help="path to computer-use-linux")
    args = parser.parse_args()

    mcp = McpProcess(args.server)
    session_id = None
    try:
        mcp.rpc(
            "initialize",
            {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "linux-portal-smoke", "version": "1"},
            },
        )
        mcp.notify("notifications/initialized")
        _, status = mcp.call("computer_status", {})
        if status.get("session_type") != "wayland":
            raise RuntimeError("portal smoke requires a Wayland desktop session")
        if not status.get("capabilities", {}).get("screencast_portal"):
            raise RuntimeError("the XDG ScreenCast portal is not available")

        print("Select a display source in the system ScreenCast dialog if it appears.", flush=True)
        started, metadata = mcp.call(
            "computer_start", {"wayland_mode": "portal"}, timeout=280
        )
        session_id = metadata["session_id"]
        if metadata.get("active_backend") != "wayland-portal":
            raise AssertionError(f"unexpected backend: {metadata.get('active_backend')}")
        if not metadata.get("capture_validated"):
            raise AssertionError("portal startup did not validate a captured frame")
        check_jpeg(started)

        observed, observation = mcp.call("computer_observe", {"session_id": session_id})
        if observation.get("observation_id") == metadata.get("observation_id"):
            raise AssertionError("computer_observe did not issue a fresh observation ID")
        check_jpeg(observed)

        _, stopped = mcp.call("computer_stop", {"session_id": session_id})
        session_id = None
        if stopped.get("closed") is not True:
            raise AssertionError(f"portal session did not close cleanly: {stopped}")
        caps = metadata.get("capabilities", {})
        print(
            "Portal smoke passed: "
            f"source={metadata.get('capture_source', 'unspecified')}, "
            f"pointer={caps.get('pointer_input', False)}, "
            f"keyboard={caps.get('keyboard_input', False)}, "
            f"scroll={caps.get('scroll_input', False)}"
        )
    finally:
        if session_id:
            try:
                mcp.call("computer_stop", {"session_id": session_id}, timeout=10)
            except Exception:
                pass
        mcp.close()


if __name__ == "__main__":
    main()
