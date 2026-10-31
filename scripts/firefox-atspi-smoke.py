#!/usr/bin/env python3
"""Check Firefox page accessibility and AT-SPI actions on Hyprland."""

import argparse
import json
import os
import pathlib
import select
import shutil
import signal
import subprocess
import tempfile
import time


class Client:
    def __init__(self, executable):
        self.process = subprocess.Popen(
            [executable, "--stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            bufsize=0,
            start_new_session=True,
        )
        self.buffer = bytearray()
        self.next_id = 1

    def call(self, method, params):
        request_id = self.next_id
        self.next_id += 1
        request = {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        os.write(self.process.stdin.fileno(), json.dumps(request).encode() + b"\n")
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            newline = self.buffer.find(b"\n")
            if newline >= 0:
                line = bytes(self.buffer[:newline])
                del self.buffer[: newline + 1]
                response = json.loads(line)
                if response.get("id") == request_id:
                    if "error" in response:
                        raise RuntimeError(response["error"].get("message", "MCP request failed"))
                    result = response.get("result", {})
                    if result.get("isError"):
                        details = result.get("structuredContent", {})
                        raise RuntimeError(str(details or result.get("content")))
                    return result.get("structuredContent", {})
                continue
            ready, _, _ = select.select([self.process.stdout], [], [], deadline - time.monotonic())
            if not ready:
                break
            chunk = os.read(self.process.stdout.fileno(), 65536)
            if not chunk:
                break
            self.buffer.extend(chunk)
        raise TimeoutError(f"MCP request {method!r} timed out")

    def start(self):
        self.call(
            "initialize",
            {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "firefox-atspi-smoke", "version": "1"},
            },
        )
        os.write(self.process.stdin.fileno(), b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')

    def close(self, session_id):
        try:
            if session_id:
                self.call("tools/call", {"name": "computer_stop", "arguments": {"session_id": session_id}})
        finally:
            self.process.stdin.close()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("executable", help="path to the computer-use-linux MCP server")
    parser.add_argument("--monitor", help="Hyprland output; required when more than one is active")
    args = parser.parse_args()
    if not os.environ.get("HYPRLAND_INSTANCE_SIGNATURE"):
        parser.error("run this smoke test inside a Hyprland session")
    firefox = shutil.which("firefox")
    if not firefox:
        parser.error("Firefox is required for this optional smoke test")
    monitors = json.loads(subprocess.check_output(["hyprctl", "-j", "monitors"], text=True))
    if args.monitor:
        if args.monitor not in {monitor["name"] for monitor in monitors}:
            parser.error(f"unknown active Hyprland output {args.monitor!r}")
        monitor = args.monitor
    elif len(monitors) == 1:
        monitor = monitors[0]["name"]
    else:
        parser.error("multiple outputs are active; select one with --monitor")

    title = "Computer-use Firefox AT-SPI smoke"
    with tempfile.TemporaryDirectory(prefix="computer-use-firefox-") as temporary:
        root = pathlib.Path(temporary)
        profile = root / "profile"
        profile.mkdir()
        (profile / "user.js").write_text(
            'user_pref("browser.startup.homepage", "about:blank");\n'
            'user_pref("browser.startup.page", 0);\n'
            'user_pref("browser.aboutwelcome.enabled", false);\n'
            'user_pref("browser.shell.checkDefaultBrowser", false);\n'
            'user_pref("browser.shell.skipDefaultBrowserCheck", true);\n',
            encoding="utf-8",
        )
        page = root / "page.html"
        page.write_text(
            """<!doctype html><meta charset="utf-8"><title>Computer-use Firefox AT-SPI smoke</title>
<main><h1>Browser accessibility</h1><label for="name">Name</label>
<input id="name" value="Ada"><button id="save" type="button"
onclick="document.title='Computer-use Firefox AT-SPI smoke — Saved'; document.getElementById('status').textContent='Saved'">Save profile</button>
<output id="status">Not saved</output><a href="#details">Details</a>
<p id="details">Accessible local test content.</p></main>""",
            encoding="utf-8",
        )
        firefox_process = subprocess.Popen(
            [firefox, "--no-remote", "--new-instance", "--profile", str(profile), page.as_uri()],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
            env={**os.environ, "MOZ_ENABLE_WAYLAND": "1"},
        )
        mcp = None
        session_id = None
        try:
            deadline = time.monotonic() + 30
            focus_key = None
            focus_stable_since = None
            while time.monotonic() < deadline:
                active = json.loads(subprocess.check_output(["hyprctl", "-j", "activewindow"], text=True))
                if active.get("class", "").lower() == "firefox" and title in active.get("title", ""):
                    current_focus = (active.get("address"), active.get("title"))
                    if current_focus != focus_key:
                        focus_key = current_focus
                        focus_stable_since = time.monotonic()
                    elif time.monotonic() - focus_stable_since >= 2:
                        break
                else:
                    focus_key = focus_stable_since = None
                if firefox_process.poll() is not None:
                    raise RuntimeError("Firefox exited before opening the local test page")
                time.sleep(0.25)
            else:
                raise TimeoutError("Firefox did not hold focus on the local test page")

            mcp = Client(os.path.abspath(args.executable))
            mcp.start()
            for attempt in range(3):
                try:
                    start_arguments = {"backend": "hyprland-native", "overlay_monitor": monitor}
                    started = mcp.call("tools/call", {"name": "computer_start", "arguments": start_arguments})
                    break
                except RuntimeError as error:
                    if "keyboard focus changed" not in str(error) or attempt == 2:
                        raise
                    time.sleep(1)
            session_id = started["session_id"]
            observation_id = started["observation_id"]
            nodes = started.get("accessibility", {}).get("nodes", [])
            by_name = {node.get("name"): node for node in nodes if node.get("name")}
            for required in ("Name", "Save profile", "Details"):
                if required not in by_name:
                    raise RuntimeError(f"Firefox AT-SPI tree omitted {required!r}; returned {len(nodes)} nodes")
            button = by_name["Save profile"]
            if "Press" not in button.get("actions", []):
                raise RuntimeError(f"Firefox button did not expose a Press action: {button}")
            if not button.get("image_bounds"):
                raise RuntimeError(f"Firefox button bounds were not mapped to the screenshot: {button}")
            depth = max((node["element_id"].count(".") for node in nodes), default=0)
            output_size = len(json.dumps(started, ensure_ascii=False, separators=(",", ":")).encode())
            print(f"Firefox AT-SPI controls: {len(nodes)} nodes at depth {depth}, {output_size} structured bytes (tree truncated={started['accessibility']['truncated']})")

            action = {
                "session_id": session_id,
                "observation_id": observation_id,
                "action": {"type": "accessibility", "element_id": button["element_id"], "action_name": "Press"},
            }
            mcp.call("tools/call", {"name": "computer_act", "arguments": action})
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                active = json.loads(subprocess.check_output(["hyprctl", "-j", "activewindow"], text=True))
                if "Saved" in active.get("title", ""):
                    break
                time.sleep(0.1)
            else:
                raise RuntimeError("Firefox AT-SPI Press action did not update the page title")
            print("Firefox AT-SPI button action updated the page")
        finally:
            try:
                if mcp is not None:
                    mcp.close(session_id)
            finally:
                try:
                    os.killpg(firefox_process.pid, signal.SIGTERM)
                    firefox_process.wait(timeout=10)
                except (ProcessLookupError, subprocess.TimeoutExpired):
                    try:
                        os.killpg(firefox_process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    firefox_process.wait()


if __name__ == "__main__":
    main()
