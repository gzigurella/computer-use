#!/usr/bin/env python3
"""Exercise MCP Streamable HTTP against the active Hyprland session."""

import base64
import http.client
import json
import os
import selectors
import socket
import subprocess
import sys
import time
from pathlib import Path


class McpHttpProcess:
    def __init__(self, binary):
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        self.port = port
        self.process = subprocess.Popen(
            [binary, "--http", str(port)],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stderr, selectors.EVENT_READ)
        self.token = None
        try:
            self.read_startup()
        except Exception:
            self.close()
            raise

    def read_startup(self):
        lines = []
        buffered = bytearray()
        while len(lines) < 2:
            if not self.selector.select(5):
                raise TimeoutError("HTTP server did not print its loopback URL and bearer token")
            data = os.read(self.process.stderr.fileno(), 4096)
            if not data:
                raise RuntimeError(f"HTTP server exited during startup: {lines}")
            buffered.extend(data)
            while b"\n" in buffered and len(lines) < 2:
                line, _, remainder = buffered.partition(b"\n")
                lines.append(line.decode("utf-8").strip())
                buffered = bytearray(remainder)
                if lines[-1].startswith("Bearer token: "):
                    self.token = lines[-1].removeprefix("Bearer token: ")
        if not any(f"127.0.0.1:{self.port}/mcp" in line for line in lines) or not self.token:
            raise RuntimeError(f"HTTP server startup output was unexpected: {lines}")

    def request(self, payload, extra_headers=None, expected_status=200):
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "Authorization": f"Bearer {self.token}",
            "MCP-Protocol-Version": "2025-11-25",
        }
        headers.update(extra_headers or {})
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=30)
        try:
            connection.request("POST", "/mcp", json.dumps(payload), headers)
            response = connection.getresponse()
            body = response.read()
            if response.status != expected_status:
                raise RuntimeError(f"HTTP status {response.status}, expected {expected_status}: {body[:1000]!r}")
            if expected_status != 200:
                return None
            if response.getheader("Content-Type", "").split(";", 1)[0] != "text/event-stream":
                raise RuntimeError("MCP HTTP response is not an event stream")
        finally:
            connection.close()
        for line in body.decode("utf-8").splitlines():
            if line.startswith("data: "):
                return json.loads(line[6:])
        raise RuntimeError(f"MCP HTTP response contained no message event: {body[:1000]!r}")

    def rpc(self, method, params=None, expected_status=200, extra_headers=None):
        self.request_id = getattr(self, "request_id", 0) + 1
        payload = {"jsonrpc": "2.0", "id": self.request_id, "method": method}
        if params is not None:
            payload["params"] = params
        response = self.request(payload, extra_headers, expected_status)
        if response is None:
            return None
        if "error" in response:
            raise RuntimeError(f"MCP {method} failed: {response['error']}")
        return response.get("result", {})

    def tool(self, name, arguments=None):
        result = self.rpc("tools/call", {"name": name, "arguments": arguments or {}})
        if result.get("isError"):
            raise RuntimeError(f"{name} failed: {result.get('content')}")
        structured = result.get("structuredContent")
        if isinstance(structured, dict):
            return structured
        if "content" not in result and isinstance(result, dict):
            return result
        for item in result.get("content", []):
            if item.get("type") == "text":
                try:
                    value = json.loads(item["text"])
                except (KeyError, TypeError, json.JSONDecodeError):
                    continue
                if isinstance(value, dict):
                    return value
        return {}

    def close(self):
        self.selector.close()
        if self.process.poll() is None:
            self.process.terminate()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()

    def kill(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=10)


def cursor_invisible():
    option = json.loads(subprocess.check_output(
        ["hyprctl", "getoption", "cursor:invisible", "-j"], text=True
    ))
    return option.get("bool")


def wait_for_cursor_visibility(expected, timeout=5):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if cursor_invisible() is expected:
            return
        time.sleep(0.05)
    raise RuntimeError(
        f"Hyprland cursor:invisible did not return to {expected}: {cursor_invisible()}"
    )


def run(binary):
    monitors = json.loads(subprocess.check_output(["hyprctl", "-j", "monitors"], text=True))
    active = [monitor["name"] for monitor in monitors if not monitor.get("disabled", False)]
    if len(active) != 1:
        raise RuntimeError(f"HTTP smoke expects one selected output in its isolated session: {active}")
    original_cursor_invisible = cursor_invisible()
    if not isinstance(original_cursor_invisible, bool):
        raise RuntimeError("Hyprland did not report cursor:invisible as a boolean")

    client = McpHttpProcess(binary)
    session_id = None
    try:
        modern_version = "2026-07-28"
        modern_meta = {
            "io.modelcontextprotocol/protocolVersion": modern_version,
            "io.modelcontextprotocol/clientCapabilities": {},
        }
        modern_discovery = client.rpc(
            "server/discover",
            {"_meta": modern_meta},
            extra_headers={
                "Authorization": f"bEaReR {client.token}",
                "MCP-Protocol-Version": modern_version,
                "MCP-Method": "server/discover",
            },
        )
        if modern_version not in modern_discovery.get("supportedVersions", []):
            raise RuntimeError(f"modern HTTP discovery omitted {modern_version}: {modern_discovery}")
        modern_tools = client.rpc(
            "tools/list",
            {"_meta": modern_meta},
            extra_headers={"MCP-Protocol-Version": modern_version, "MCP-Method": "tools/list"},
        )
        modern_tool_names = {tool.get("name") for tool in modern_tools.get("tools", [])}
        if "computer_start" not in modern_tool_names:
            raise RuntimeError(f"modern HTTP tool discovery omitted computer_start: {modern_tool_names}")
        modern_call_params = {"name": "computer_status", "arguments": {}, "_meta": modern_meta}
        mismatched = client.rpc(
            "tools/call",
            modern_call_params,
            expected_status=400,
            extra_headers={
                "MCP-Protocol-Version": modern_version,
                "MCP-Method": "tools/call",
                "MCP-Name": "wrong-tool-name",
            },
        )
        if mismatched is not None:
            raise RuntimeError("modern HTTP accepted a mismatched MCP-Name header")
        modern_status = client.rpc(
            "tools/call",
            modern_call_params,
            extra_headers={
                "MCP-Protocol-Version": modern_version,
                "MCP-Method": "tools/call",
                "MCP-Name": "computer_status",
            },
        )
        if not isinstance(modern_status.get("content"), list) or (
            modern_status.get("structuredContent", {}).get("recommended_backend") != "hyprland-native"
        ):
            raise RuntimeError(f"modern HTTP tool call returned an invalid status: {modern_status}")

        def modern_tool(name, arguments):
            params = {"name": name, "arguments": arguments, "_meta": modern_meta}
            return client.rpc(
                "tools/call",
                params,
                extra_headers={
                    "MCP-Protocol-Version": modern_version,
                    "MCP-Method": "tools/call",
                    "MCP-Name": name,
                },
            )

        modern_started = modern_tool("computer_start", {"output": active[0]})
        modern_started_metadata = modern_started.get("structuredContent", {})
        modern_session_id = modern_started_metadata.get("session_id")
        if not modern_session_id or modern_started_metadata.get("active_backend") != "hyprland-native":
            raise RuntimeError(f"modern HTTP computer_start failed: {modern_started}")
        if modern_started_metadata.get("capabilities", {}).get("native_cursor_hidden") is not original_cursor_invisible:
            raise RuntimeError("modern HTTP start changed native cursor visibility")
        if cursor_invisible() is not original_cursor_invisible:
            raise RuntimeError("modern HTTP start changed Hyprland's native cursor visibility")
        session_id = modern_session_id
        modern_observed = modern_tool("computer_observe", {"session_id": modern_session_id})
        modern_observation = modern_observed.get("structuredContent", {})
        image_data = next((item.get("data") for item in modern_observed.get("content", []) if item.get("type") == "image"), None)
        if not modern_observation.get("observation_id") or not image_data:
            raise RuntimeError(f"modern HTTP computer_observe omitted its screenshot: {modern_observed}")
        if not base64.b64decode(image_data).startswith(b"\xff\xd8"):
            raise RuntimeError("modern HTTP computer_observe returned a non-JPEG screenshot")
        modern_stopped = modern_tool("computer_stop", {"session_id": modern_session_id})
        if modern_stopped.get("structuredContent", {}).get("closed") is not True:
            raise RuntimeError(f"modern HTTP computer_stop failed: {modern_stopped}")
        session_id = None
        wait_for_cursor_visibility(original_cursor_invisible)

        initialized = client.rpc(
            "initialize",
            {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "hyprland-http-smoke", "version": "1"},
            },
            extra_headers={},
        )
        if initialized.get("protocolVersion") != "2025-11-25":
            raise RuntimeError(f"HTTP MCP initialized with an unexpected protocol: {initialized}")
        client.request({"jsonrpc": "2.0", "method": "notifications/initialized"}, expected_status=202)
        listed = client.rpc("tools/list")
        tool_names = {tool.get("name") for tool in listed.get("tools", [])}
        if not {"computer_status", "computer_start", "computer_observe", "computer_stop"}.issubset(tool_names):
            raise RuntimeError(f"HTTP MCP omitted core desktop tools: {tool_names}")

        status = client.tool("computer_status")
        if status.get("recommended_backend") != "hyprland-native":
            raise RuntimeError(f"HTTP computer_status did not recommend native Hyprland: {status}")
        started = client.tool("computer_start", {"output": active[0]})
        session_id = started.get("session_id")
        capabilities = started.get("capabilities", {})
        if started.get("active_backend") != "hyprland-native" or not started.get("capture_validated"):
            raise RuntimeError(f"HTTP computer_start did not validate native capture: {started}")
        if started.get("capabilities", {}).get("native_cursor_hidden") is not original_cursor_invisible:
            raise RuntimeError("HTTP start changed native cursor visibility")
        if cursor_invisible() is not original_cursor_invisible:
            raise RuntimeError("HTTP start changed Hyprland's native cursor visibility")
        if not capabilities.get("screenshot") or not capabilities.get("pointer_input"):
            raise RuntimeError(f"HTTP computer_start omitted core desktop capabilities: {capabilities}")
        observed = client.tool("computer_observe", {"session_id": session_id})
        if not observed.get("observation_id") or observed.get("capabilities", {}).get("visual_pointer_overlay") is not True:
            raise RuntimeError(f"HTTP computer_observe returned incomplete session metadata: {observed}")
        if observed.get("capabilities", {}).get("native_cursor_hidden") is not original_cursor_invisible:
            raise RuntimeError("HTTP observe reported changed native cursor visibility")
        if client.tool("computer_stop", {"session_id": session_id}).get("closed") is not True:
            raise RuntimeError("HTTP computer_stop did not close the session")
        session_id = None
        wait_for_cursor_visibility(original_cursor_invisible)

        unauthorized = client.request(
            {"jsonrpc": "2.0", "id": 999, "method": "tools/list"},
            {"Authorization": "Bearer invalid"},
            expected_status=401,
        )
        if unauthorized is not None:
            raise RuntimeError("unauthorized HTTP request unexpectedly returned a result")
        rejected_host = client.request(
            {"jsonrpc": "2.0", "id": 1000, "method": "tools/list"},
            {"Host": "attacker.example"},
            expected_status=403,
        )
        rejected_origin = client.request(
            {"jsonrpc": "2.0", "id": 1001, "method": "tools/list"},
            {"Origin": f"http://attacker.example:{client.port}"},
            expected_status=403,
        )
        if rejected_host is not None or rejected_origin is not None:
            raise RuntimeError("HTTP host or origin validation unexpectedly returned a result")
    finally:
        if session_id and client.process.poll() is None:
            try:
                client.tool("computer_stop", {"session_id": session_id})
            except Exception:
                pass
        client.close()

    crash_client = McpHttpProcess(binary)
    try:
        crash_started = crash_client.tool("computer_start", {"output": active[0]})
        if crash_started.get("capabilities", {}).get("native_cursor_hidden") is not original_cursor_invisible:
            raise RuntimeError("crash-recovery start changed native cursor visibility")
        if cursor_invisible() is not original_cursor_invisible:
            raise RuntimeError("crash-recovery start changed Hyprland's native cursor visibility")
        crash_client.kill()
        wait_for_cursor_visibility(original_cursor_invisible)
    finally:
        crash_client.close()
    print("Hyprland MCP HTTP smoke passed: modern discovery/header validation, legacy initialize, native start/capture/observe/stop, cursor restoration after stop and server SIGKILL, and invalid token/host/origin rejection")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {Path(sys.argv[0]).name} path/to/computer-use-linux")
    run(str(Path(sys.argv[1]).resolve()))
