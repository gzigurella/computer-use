# computer-use-linux

A local MCP server that lets an agent see and control a Linux desktop through one stable tool API, with a separate implementation for each kind of desktop.

## Desktop

**Target**:
A kind of desktop the server knows how to control, such as Hyprland, Sway, X11, or the XDG portal.
_Avoid_: Platform, compositor (when meaning the Target), desktop environment

**Supported**:
A Target the project commits to making work and verifies end to end. Today only Hyprland.
_Avoid_: Stable, official

**Experimental**:
A Target that exists in the code but has no support commitment or dedicated verification.
_Avoid_: Beta, unofficial

**Backend**:
One concrete way to capture and send input for a Target, selectable at Session start (for example native screencopy vs Grim on Hyprland).
_Avoid_: Route, adapter, implementation, mode

**Available**:
A Backend that is built into this binary and usable in the current Login Session.
_Avoid_: Detected, enabled, supported (when meaning this)

**Output**:
A single display surface managed by the compositor, with its own position, scale and rotation.
_Avoid_: Monitor, display, screen

**Selected Output**:
The one Output a Session captures and controls, and where the AI Pointer is drawn.
_Avoid_: Overlay monitor

**Login Session**:
The user's graphical desktop session that the server runs inside and acts on.
_Avoid_: Desktop session, Hyprland session (when meaning this)

## Interaction

**Session**:
The period between the agent starting and stopping control of the desktop, bound to one Backend and one Selected Output.
_Avoid_: Desktop session, connection

**Screenshot**:
An image of the Selected Output as the agent sees it, including the AI Pointer.
_Avoid_: Capture, frame (when meaning what the agent sees)

**Observation**:
The actionable record of the desktop at one moment: a Screenshot, its geometry, and the Accessible Elements visible in it. Pointer, keyboard and accessibility Actions are checked against it. It is consumed by one Action call, and a newer Observation replaces it.
_Avoid_: Snapshot, capture, state

**Action**:
One input or query step the agent asks the server to perform, such as a click, key chord, text entry, scroll, or accessibility invocation.
_Avoid_: Command, event

**Accessible Element**:
A UI control exposed by an application's accessibility tree, with a name, a role, bounds, and invocable accessibility actions.
_Avoid_: Node, widget, control (when meaning this)

**AI Pointer**:
The styled arrow showing the agent where the shared native pointer is. It is a visual aid only and does not replace the native cursor.
_Avoid_: Overlay, AI cursor, virtual pointer

## Flagged ambiguities

- The `screenshot` Action creates a full Observation, not just a Screenshot. It isn't yet decided whether it should be renamed, or should return a Screenshot without replacing the current Observation.
