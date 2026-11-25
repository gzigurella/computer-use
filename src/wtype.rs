//! Keyboard input through the external `wtype` helper.

use super::*;

pub(crate) fn wtype_text(text: &str) -> Result<(), String> {
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(
            "text input does not support control characters other than tab and newline".into(),
        );
    }

    let mut child = limited_command("30s", "wtype")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("starting wtype: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("wtype has no stdin")?;
    if let Err(error) = stdin.write_all(text.as_bytes()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("writing wtype input: {error}"));
    }
    drop(stdin);
    let output = child
        .wait_with_output()
        .map_err(|error| format!("waiting for wtype: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "wtype text input failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

pub(crate) fn wtype_key(key: &str, modifiers: &[String]) -> Result<(), String> {
    if modifiers.is_empty() {
        // Sway can drop a simple named-key event; literal input preserves it in the GTK receiver.
        if let Some(text) = wtype_text_for_key(key) {
            return wtype_text(text);
        }
    }
    let mut command = limited_command("3s", "wtype");
    if modifiers.is_empty() && wtype_uses_explicit_key_state(key) {
        // Let the target process wtype's temporary keymap before sending this keysym.
        command.args(["-s", "100", "-P", key, "-s", "100", "-p", key]);
    } else {
        for modifier in modifiers {
            let name = match modifier.as_str() {
                "CTRL" => "ctrl",
                "SHIFT" => "shift",
                "ALT" => "alt",
                "SUPER" => "logo",
                _ => return Err("unsupported modifier".into()),
            };
            command.args(["-M", name]);
        }
        if !modifiers.is_empty() {
            command.args(["-s", "20"]);
        }
        command.args(["-k", key]);
        if !modifiers.is_empty() {
            command.args(["-s", "20"]);
        }
        for modifier in modifiers.iter().rev() {
            let name = match modifier.as_str() {
                "CTRL" => "ctrl",
                "SHIFT" => "shift",
                "ALT" => "alt",
                "SUPER" => "logo",
                _ => return Err("unsupported modifier".into()),
            };
            command.args(["-m", name]);
        }
    }
    let output = command
        .output()
        .map_err(|error| format!("starting wtype: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "wtype key input failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

pub(crate) fn wtype_uses_explicit_key_state(key: &str) -> bool {
    key.strip_prefix('F')
        .and_then(|number| number.parse::<u8>().ok())
        .is_some_and(|number| (1..=24).contains(&number))
}

pub(crate) fn wtype_text_for_key(key: &str) -> Option<&str> {
    match key {
        "Return" => Some("\n"),
        "Tab" => Some("\t"),
        "space" => Some(" "),
        _ if key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric() => Some(key),
        _ => None,
    }
}
