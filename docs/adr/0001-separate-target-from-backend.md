---
status: accepted
---

# Separate Target from Backend

The project uses two separate concepts. A **Target** is the kind of desktop being controlled (Hyprland, Sway, X11, portal). A **Backend** is one concrete way to capture and send input for a Target (for example, native screencopy vs Grim on Hyprland). Before this, one flat "backend" list mixed the two, alongside the synonyms route, adapter and implementation. That hid the fact that `hyprland-native` and `hyprland-grim` control the same desktop, while `sway-grim` and `x11` control different ones. Support is decided per Target (only Hyprland is supported; the rest are experimental). Availability is decided per Backend in the current Login Session. A single term cannot express both.

## Considered Options

- **One flat Backend term.** Simplest, and it matches the current API. But support status would have to be repeated on every capture variant, and adding a new capture method for Hyprland would look like adding a new desktop.
- **Backend for the desktop, plus a "capture method" property.** This leaves out the input side. Backends differ in how they send pointer and keyboard input as well as how they capture, so "capture method" is too narrow.

## Consequences

- The public tool API takes both: `computer_start` accepts a `target`, which alone picks that Target's first Available Backend, and a `backend`, which must belong to the given Target. Backend IDs stay globally unique and Target-qualified (`hyprland-native`, `hyprland-grim`), so existing clients that pass only `backend` keep working. `computer_status` reports `targets`, `recommended_target`, and the `target` of each backend.
- Contributor docs for new desktops should describe adding a Target. New ways of capturing or sending input on an existing Target are Backends.
