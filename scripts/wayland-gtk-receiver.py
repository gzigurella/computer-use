#!/usr/bin/env python3
"""Record input delivered to a GTK4 Wayland window."""

import json
import sys

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gdk, Gio, Gtk


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: wayland-gtk-receiver.py EVENT_LOG")

    log_path = sys.argv[1]

    def record(**event):
        with open(log_path, "a", encoding="utf-8") as output:
            output.write(json.dumps(event) + "\n")

    application = Gtk.Application(
        application_id="org.example.computerusesmoke",
        flags=Gio.ApplicationFlags.NON_UNIQUE,
    )

    def activate(app):
        window = Gtk.ApplicationWindow(application=app, title="Computer use input test")
        window.set_default_size(600, 400)
        entry = Gtk.Entry(placeholder_text="Input receiver")
        entry.set_size_request(220, 48)
        entry.set_vexpand(True)
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        controls = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        controls.append(entry)
        button = Gtk.Button(label="Click receiver")
        button.set_accessible_role(Gtk.AccessibleRole.BUTTON)
        button.connect("clicked", lambda *_args: record(button_click=True))
        controls.append(button)
        box.append(controls)
        text_view = Gtk.TextView()
        text_view.set_monospace(True)
        text_view.set_wrap_mode(Gtk.WrapMode.NONE)
        buffer = text_view.get_buffer()
        buffer.set_text(("0123456789" * 20 + "\n") * 30)
        buffer.connect(
            "mark-set",
            lambda text_buffer, _where, _mark: record(
                selection=text_buffer.get_text(*text_buffer.get_selection_bounds(), include_hidden_chars=True)
            )
            if text_buffer.get_selection_bounds()
            else None,
        )
        buffer.connect(
            "changed",
            lambda text_buffer: record(
                text=text_buffer.get_text(*text_buffer.get_bounds(), include_hidden_chars=True)
            ),
        )
        scrolled = Gtk.ScrolledWindow()
        scrolled.set_hexpand(True)
        scrolled.set_vexpand(True)
        scrolled.set_child(text_view)
        box.append(scrolled)
        window.set_child(box)
        entry.connect("changed", lambda widget: record(text=widget.get_text()))

        click = Gtk.GestureClick()
        click.set_button(0)
        click.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        def click_pressed(gesture, count, x, y):
            state = gesture.get_current_event_state()
            record(
                click=[x, y],
                button=gesture.get_current_button(),
                click_count=count,
                control=bool(state & Gdk.ModifierType.CONTROL_MASK),
                shift=bool(state & Gdk.ModifierType.SHIFT_MASK),
                alt=bool(state & Gdk.ModifierType.ALT_MASK),
                super=bool(state & Gdk.ModifierType.SUPER_MASK),
            )
        click.connect("pressed", click_pressed)
        click.connect(
            "released",
            lambda gesture, _count, x, y: record(
                release=[x, y], button=gesture.get_current_button()
            ),
        )
        window.add_controller(click)

        drag = Gtk.GestureDrag()
        drag.set_button(1)
        drag.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        drag.connect(
            "drag-begin",
            lambda gesture, _x, _y: record(button_press=gesture.get_current_button()),
        )
        drag.connect(
            "drag-end",
            lambda gesture, _x, _y: record(button_release=gesture.get_current_button()),
        )
        window.add_controller(drag)

        scroll = Gtk.EventControllerScroll.new(
            Gtk.EventControllerScrollFlags.HORIZONTAL
            | Gtk.EventControllerScrollFlags.VERTICAL
        )
        scroll.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        scroll.connect("scroll", lambda _controller, dx, dy: (record(scroll=[dx, dy]), False)[1])
        window.add_controller(scroll)

        keyboard = Gtk.EventControllerKey()
        keyboard.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        def key_pressed(_controller, keyval, _keycode, state):
            unicode_value = Gdk.keyval_to_unicode(keyval)
            record(
                key=Gdk.keyval_name(keyval),
                key_text=chr(unicode_value) if unicode_value else "",
                modifiers=int(state),
                control=bool(state & Gdk.ModifierType.CONTROL_MASK),
                shift=bool(state & Gdk.ModifierType.SHIFT_MASK),
                alt=bool(state & Gdk.ModifierType.ALT_MASK),
                super=bool(state & Gdk.ModifierType.SUPER_MASK),
            )
            return False
        keyboard.connect(
            "key-pressed",
            key_pressed,
        )
        window.add_controller(keyboard)

        window.connect("map", lambda *_args: record(ready=True))
        window.present()

    application.connect("activate", activate)
    raise SystemExit(application.run([]))


if __name__ == "__main__":
    main()
