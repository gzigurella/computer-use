#!/usr/bin/env python3
"""Record desktop input delivered to a small Qt 6 Wayland window."""

import json
import sys

from PySide6.QtCore import QEvent, QObject, Qt, QTimer
from PySide6.QtWidgets import QApplication, QHBoxLayout, QLineEdit, QMainWindow, QPlainTextEdit, QPushButton, QWidget


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: wayland-qt-receiver.py EVENT_LOG")

    log_path = sys.argv[1]

    def record(**event):
        with open(log_path, "a", encoding="utf-8") as output:
            output.write(json.dumps(event) + "\n")

    def enum_value(value):
        return value.value if hasattr(value, "value") else int(value)

    class RecordingLineEdit(QLineEdit):
        def mousePressEvent(self, event):
            record(click=[enum_value(event.button()), round(event.position().x()), round(event.position().y())])
            super().mousePressEvent(event)

    class RecordingPlainTextEdit(QPlainTextEdit):
        def mousePressEvent(self, event):
            record(click=[enum_value(event.button()), round(event.position().x()), round(event.position().y())])
            super().mousePressEvent(event)

    class InputRecorder(QObject):
        def eventFilter(self, watched, event):
            if event.type() == QEvent.Type.MouseButtonPress:
                record(button_press=enum_value(event.button()))
                record(
                    click=[enum_value(event.button()), round(event.position().x()), round(event.position().y())],
                    control=bool(event.modifiers() & Qt.KeyboardModifier.ControlModifier),
                    shift=bool(event.modifiers() & Qt.KeyboardModifier.ShiftModifier),
                    alt=bool(event.modifiers() & Qt.KeyboardModifier.AltModifier),
                    super=bool(event.modifiers() & Qt.KeyboardModifier.MetaModifier),
                )
            elif event.type() == QEvent.Type.MouseButtonRelease:
                record(button_release=enum_value(event.button()))
                record(release=[enum_value(event.button()), round(event.position().x()), round(event.position().y())])
            elif event.type() == QEvent.Type.MouseButtonDblClick:
                record(double_click=True)
            elif event.type() == QEvent.Type.KeyPress:
                key = {
                    Qt.Key.Key_F12: "F12",
                    Qt.Key.Key_F24: "F24",
                    Qt.Key.Key_F35: "F35",
                    Qt.Key.Key_Return: "Return",
                }.get(event.key())
                if event.modifiers() & Qt.KeyboardModifier.KeypadModifier:
                    key = {
                        Qt.Key.Key_0: "KP_0", Qt.Key.Key_1: "KP_1", Qt.Key.Key_2: "KP_2",
                        Qt.Key.Key_3: "KP_3", Qt.Key.Key_4: "KP_4", Qt.Key.Key_5: "KP_5",
                        Qt.Key.Key_6: "KP_6", Qt.Key.Key_7: "KP_7", Qt.Key.Key_8: "KP_8",
                        Qt.Key.Key_9: "KP_9", Qt.Key.Key_Enter: "KP_Enter",
                    }.get(event.key(), key)
                elif event.key() == Qt.Key.Key_Enter:
                    key = "Return"
                record(
                    key=key or (chr(event.key()) if 0 <= event.key() < 128 else event.text() or str(event.key())),
                    key_text=event.text(),
                    control=bool(event.modifiers() & Qt.KeyboardModifier.ControlModifier),
                    shift=bool(event.modifiers() & Qt.KeyboardModifier.ShiftModifier),
                    alt=bool(event.modifiers() & Qt.KeyboardModifier.AltModifier),
                    super=bool(event.modifiers() & Qt.KeyboardModifier.MetaModifier),
                )
            elif event.type() == QEvent.Type.Wheel:
                delta = event.angleDelta()
                pixels = event.pixelDelta()
                record(scroll=[pixels.x() or delta.x(), pixels.y() or delta.y()])
            return False

    app = QApplication(sys.argv[:1])
    app.setApplicationName("org.example.computerusesmoke")
    app.setDesktopFileName("org.example.computerusesmoke")
    window = QMainWindow()
    window.setWindowTitle("Computer use input test")
    row = QWidget()
    layout = QHBoxLayout(row)
    entry = RecordingLineEdit()
    entry.setPlaceholderText("Input receiver")
    entry.setAccessibleName("Input receiver")
    entry.setMinimumWidth(220)
    entry.setText("01234567890123456789")
    editor = RecordingPlainTextEdit()
    editor.setAccessibleName("Selectable sample text")
    editor.setLineWrapMode(QPlainTextEdit.LineWrapMode.NoWrap)
    editor.setPlainText(("0123456789" * 20 + "\n") * 30)
    button = QPushButton("Click receiver")
    button.setAccessibleName("Click receiver")
    button.setContextMenuPolicy(Qt.ContextMenuPolicy.CustomContextMenu)
    button.clicked.connect(lambda: record(button_click=True))
    button.customContextMenuRequested.connect(lambda _position: record(context_menu=True))
    layout.addWidget(entry)
    layout.addWidget(button)
    layout.addWidget(editor, 1)
    window.setCentralWidget(row)
    window.resize(600, 400)
    recorder = InputRecorder(app)
    entry.textChanged.connect(lambda text: record(text=text))
    entry.cursorPositionChanged.connect(
        lambda _old, new: record(cursor_position=new)
    )

    def selection_changed():
        cursor = editor.textCursor()
        if cursor.hasSelection():
            record(selection=cursor.selectedText())

    def window_bounds(widget):
        position = widget.mapTo(window, widget.rect().topLeft())
        return [position.x(), position.y(), widget.width(), widget.height()]

    editor.cursorPositionChanged.connect(selection_changed)
    window.show()
    app.processEvents()
    app.installEventFilter(recorder)
    entry.installEventFilter(recorder)
    editor.viewport().installEventFilter(recorder)
    QTimer.singleShot(100, lambda: record(
        ready=True,
        window=[window.mapToGlobal(window.rect().topLeft()).x(), window.mapToGlobal(window.rect().topLeft()).y(), window.width(), window.height()],
        entry=window_bounds(entry),
        button=window_bounds(button),
        editor=window_bounds(editor.viewport()),
    ))
    raise SystemExit(app.exec())


if __name__ == "__main__":
    main()
