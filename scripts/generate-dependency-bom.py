#!/usr/bin/env python3
"""Generate the Linux-target Cargo dependency graph as CycloneDX JSON."""

import argparse
import json
import hashlib
import re
import subprocess
import uuid
from datetime import datetime, timezone
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-unknown-linux-gnu"
OUTPUT = ROOT / "dependency-bom.cdx.json"

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--check", action="store_true", help="fail if the checked-in BOM differs from Cargo.lock")
args = parser.parse_args()


def run(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True)


metadata = json.loads(run("cargo", "metadata", "--locked", "--format-version", "1"))
lock_hash = hashlib.sha256((ROOT / "Cargo.lock").read_bytes()).hexdigest()
tree = run(
    "cargo", "tree", "--locked", "--target", TARGET,
    "--edges", "normal,build", "--prefix", "depth", "--format", "{p}",
)
packages = {
    (package["name"], package["version"]): package
    for package in metadata["packages"]
}
lines = [line for line in tree.splitlines() if re.match(r"^\d+", line)]
name_version = re.compile(r"^(\d+)([^ ]+) v([^ ]+)(?: \(.*\))?$")
stack = []
active = set()
edges = {}
for line in lines:
    match = name_version.match(line)
    if not match:
        raise SystemExit(f"cannot parse cargo tree line: {line}")
    depth, name, version = int(match.group(1)), match.group(2), match.group(3)
    key = (name, version)
    if key not in packages:
        raise SystemExit(f"cargo metadata omitted {name} {version}")
    active.add(key)
    if depth:
        parent = stack[depth - 1]
        edges.setdefault(parent, set()).add(key)
    stack = stack[:depth]
    if not line.endswith(" (*)"):
        stack.append(key)


def bom_ref(key):
    return f"pkg:cargo/{key[0]}@{key[1]}"


def component(package):
    license_expression = package.get("license")
    result = {
        "type": "library",
        "bom-ref": bom_ref((package["name"], package["version"])),
        "name": package["name"],
        "version": package["version"],
        "purl": bom_ref((package["name"], package["version"])),
        "scope": "required",
    }
    if license_expression:
        result["licenses"] = [{"license": {"expression": license_expression}}]
    return result


root = metadata["resolve"]["root"]
root_package = next(package for package in metadata["packages"] if package["id"] == root)
root_key = (root_package["name"], root_package["version"])
root_node = next(node for node in metadata["resolve"]["nodes"] if node["id"] == root)
default_features = sorted(root_package.get("features", {}).get("default", []))
resolved_root_features = sorted(set(root_node.get("features", [])) - {"default"})
root_component = component(root_package)
root_component["type"] = "application"
root_component["purl"] = bom_ref(root_key)

package_ids = {
    (package["name"], package["version"]): package["id"]
    for package in metadata["packages"]
}
id_to_key = {package_id: key for key, package_id in package_ids.items()}
resolve_nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
active_ids = {package_ids[key] for key in active}
dependency_graph = []
for key in sorted(active):
    package_id = package_ids[key]
    node = resolve_nodes.get(package_id, {})
    dependencies = sorted({
        bom_ref(id_to_key[dependency["pkg"]])
        for dependency in node.get("deps", [])
        if dependency["pkg"] in active_ids and dependency["pkg"] in id_to_key
    })
    dependency_graph.append({"ref": bom_ref(key), "dependsOn": dependencies})

document = {
    "bomFormat": "CycloneDX",
    "specVersion": "1.6",
    "serialNumber": f"urn:uuid:{uuid.uuid5(uuid.NAMESPACE_URL, f'{TARGET}:{lock_hash}')}",
    "version": 1,
    "metadata": {
        "timestamp": datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z"),
        "tools": {"components": [{"type": "application", "name": "Cargo metadata", "version": "1"}]},
        "component": root_component,
        "properties": [
            {"name": "cargo.target", "value": TARGET},
            {"name": "cargo.lock.sha256", "value": lock_hash},
            {"name": "cargo.default-features", "value": json.dumps(default_features, separators=(",", ":"))},
            {"name": "cargo.resolved-root-features", "value": json.dumps(resolved_root_features, separators=(",", ":"))},
        ],
    },
    "components": [component(packages[key]) for key in sorted(active) if key != root_key],
    "dependencies": dependency_graph,
}
if args.check:
    try:
        checked_in = json.loads(OUTPUT.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SystemExit(f"cannot read checked-in BOM {OUTPUT}: {error}")
    for bom in (document, checked_in):
        bom.get("metadata", {}).pop("timestamp", None)
    if checked_in != document:
        raise SystemExit(f"{OUTPUT.relative_to(ROOT)} is stale; run scripts/generate-dependency-bom.py")
    print(f"{OUTPUT.relative_to(ROOT)} matches the locked Cargo dependency graph")
    raise SystemExit(0)

OUTPUT.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
print(f"wrote {OUTPUT.relative_to(ROOT)} with {len(document['components'])} Cargo libraries for {TARGET}")
