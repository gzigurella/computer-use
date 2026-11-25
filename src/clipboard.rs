//! Plain-text clipboard read and write for each Backend.

use super::*;

pub(crate) fn wayland_backend(backend: &Backend) -> bool {
    matches!(
        backend,
        Backend::Wayland { .. }
            | Backend::Hyprland { .. }
            | Backend::Sway { .. }
            | Backend::WaylandGrim { .. }
    )
}

pub(crate) fn clipboard_read(backend: &Backend) -> Result<String, String> {
    let wayland = wayland_backend(backend);
    if wayland && wayland_clipboard::available() {
        return wayland_clipboard::read_text();
    }
    let (bytes, latin1) = if wayland {
        let types = clipboard_command_output("wl-paste", &["--list-types"], 16 * 1024)?;
        let available = String::from_utf8_lossy(&types);
        let target = find_text_target(&available)
            .ok_or("clipboard does not contain a supported text format")?;
        (
            clipboard_command_output("wl-paste", &["--no-newline", "--type", target], 64 * 1024)?,
            false,
        )
    } else if command_exists("xclip") {
        let targets = clipboard_command_output(
            "xclip",
            &["-o", "-selection", "clipboard", "-t", "TARGETS"],
            16 * 1024,
        )?;
        let available = String::from_utf8_lossy(&targets);
        let target = find_text_target(&available)
            .ok_or("clipboard does not contain a supported text format")?;
        (
            clipboard_command_output(
                "xclip",
                &["-o", "-selection", "clipboard", "-t", target],
                64 * 1024,
            )?,
            target.eq_ignore_ascii_case("STRING"),
        )
    } else if command_exists("xsel") {
        (
            clipboard_command_output("xsel", &["--clipboard", "--output"], 64 * 1024)?,
            false,
        )
    } else {
        return Err("clipboard access needs wl-clipboard on Wayland or xclip/xsel on X11".into());
    };
    if latin1 {
        return Ok(bytes.into_iter().map(char::from).collect());
    }
    String::from_utf8(bytes).map_err(|_| "clipboard text is not valid UTF-8".into())
}

pub(crate) fn find_text_target(types: &str) -> Option<&str> {
    let advertised = types
        .split(|character: char| character.is_whitespace() || character == '\0')
        .filter(|target| !target.is_empty())
        .collect::<Vec<_>>();
    [
        "text/plain;charset=utf-8",
        "UTF8_STRING",
        "text/plain",
        "STRING",
    ]
    .iter()
    .find_map(|preferred| {
        advertised
            .iter()
            .copied()
            .find(|target| target.eq_ignore_ascii_case(preferred))
    })
}

pub(crate) fn clipboard_command_output(
    program: &str,
    args: &[&str],
    limit: usize,
) -> Result<Vec<u8>, String> {
    let mut command = limited_command("3s", program);
    command.args(args);
    let (bytes, status) = bounded_command_output(&mut command, limit)
        .map_err(|error| format!("{program} clipboard command failed: {error}"))?;
    if status.success() {
        Ok(bytes)
    } else {
        Err(format!(
            "{program} clipboard command failed with status {status}"
        ))
    }
}

pub(crate) fn clipboard_write(backend: &Backend, text: &str) -> Result<(), String> {
    if wayland_backend(backend) && wayland_clipboard::available() {
        return wayland_clipboard::write_text(text);
    }
    let (program, args): (&str, &[&str]) = if wayland_backend(backend) {
        ("wl-copy", &["--type", "text/plain;charset=utf-8"])
    } else if command_exists("xclip") {
        (
            "xclip",
            &["-selection", "clipboard", "-i", "-t", "UTF8_STRING"],
        )
    } else if command_exists("xsel") {
        ("xsel", &["--clipboard", "--input"])
    } else {
        return Err("clipboard access needs wl-clipboard on Wayland or xclip/xsel on X11".into());
    };
    let mut child = limited_command("3s", program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("starting {program}: {error}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("{program} has no stdin"))?;
    if let Err(error) = stdin.write_all(text.as_bytes()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("writing {program} clipboard input: {error}"));
    }
    drop(stdin);
    let status = child
        .wait()
        .map_err(|error| format!("waiting for {program}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} clipboard write failed with status {status}"
        ))
    }
}
