//! Parsing and validating the arguments of `computer_act` Actions, independent of any Backend.

use super::*;

pub(crate) fn same_active_window(expected: &Option<Value>, actual: &Option<Value>) -> bool {
    match (expected, actual) {
        (None, None) => true,
        (Some(expected), Some(actual)) => {
            if expected["window_id"].is_string() && actual["window_id"].is_string() {
                expected["window_id"] == actual["window_id"]
                    && expected["application"] == actual["application"]
                    // A stable window identity is not enough for screenshot-grounded
                    // coordinates: reject observations spanning a move or resize.
                    && expected["bounds"] == actual["bounds"]
            } else {
                expected == actual
            }
        }
        _ => false,
    }
}
pub(crate) fn same_focused_element(expected: &Option<Value>, actual: &Option<Value>) -> bool {
    match (expected, actual) {
        (None, None) => true,
        (Some(expected), Some(actual)) => {
            expected["element_id"] == actual["element_id"]
                && expected["name"] == actual["name"]
                && expected["role"] == actual["role"]
                && expected["process_id"] == actual["process_id"]
                && expected["bounds"] == actual["bounds"]
                && (!expected["accessible_id"].is_string()
                    || !actual["accessible_id"].is_string()
                    || expected["accessible_id"] == actual["accessible_id"])
        }
        _ => false,
    }
}
pub(crate) fn coordinate(action: &Value, name: &str, extent: u32) -> Result<u32, String> {
    let value = action
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{name} must be a non-negative integer"))?;
    let value = u32::try_from(value).map_err(|_| format!("{name} is too large"))?;
    if value >= extent {
        return Err(format!("{name} is outside the observed display"));
    }
    Ok(value)
}
pub(crate) fn scroll_deltas(action: &Value) -> Result<(i32, i32), String> {
    if action.get("scroll_x").is_some() || action.get("scroll_y").is_some() {
        if action.get("direction").is_some() || action.get("amount").is_some() {
            return Err("use scroll_x/scroll_y or direction/amount, not both".into());
        }
        let delta = |name| -> Result<i32, String> {
            let value = action
                .get(name)
                .map_or(Some(0), Value::as_i64)
                .ok_or_else(|| format!("{name} must be an integer from -1000 to 1000"))?;
            i32::try_from(value)
                .ok()
                .filter(|value| (-1000..=1000).contains(value))
                .ok_or_else(|| format!("{name} must be an integer from -1000 to 1000"))
        };
        let (x, y) = (delta("scroll_x")?, delta("scroll_y")?);
        if x == 0 && y == 0 {
            return Err("scroll_x and scroll_y cannot both be zero".into());
        }
        return Ok((x, y));
    }
    let direction = required_string(action, "direction")?;
    let amount = action
        .get("amount")
        .map(|value| {
            value
                .as_u64()
                .ok_or("scroll amount must be an integer from 1 to 100")
        })
        .transpose()?
        .unwrap_or(1);
    if !(1..=100).contains(&amount) {
        return Err("scroll amount must be an integer from 1 to 100".into());
    }
    let amount = amount as i32;
    match direction {
        "up" => Ok((0, -amount)),
        "down" => Ok((0, amount)),
        "left" => Ok((-amount, 0)),
        "right" => Ok((amount, 0)),
        _ => Err("direction must be up, down, left, or right".into()),
    }
}
pub(crate) fn validate_action_text(action: &str, text: &str) -> Result<(), String> {
    match action {
        "type" => {
            if text.is_empty() {
                return Err("text is required".into());
            }
            if text.len() > 4096 {
                return Err("text exceeds 4096 bytes".into());
            }
            if text
                .chars()
                .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
            {
                return Err(
                    "text input does not support control characters other than tab and newline"
                        .into(),
                );
            }
        }
        "clipboard_write" if text.len() > 64 * 1024 => {
            return Err("clipboard text exceeds 64 KiB".into());
        }
        "set_text" if text.chars().count() > 4096 => {
            return Err("text exceeds 4096 characters".into());
        }
        _ => {}
    }
    Ok(())
}
pub(crate) fn wait_duration(action: &Value) -> Result<u64, String> {
    match action.get("duration_ms") {
        None => Ok(2_000),
        Some(value) => value
            .as_u64()
            .filter(|ms| *ms <= 10_000)
            .ok_or_else(|| "duration_ms must be an integer from 0 to 10000".into()),
    }
}
pub(crate) fn drag_path(
    action: &Value,
    observation: &Observation,
) -> Result<Vec<(u32, u32)>, String> {
    if let Some(points) = action.get("path") {
        let points = points.as_array().ok_or("path must be an array")?;
        if !(2..=64).contains(&points.len()) {
            return Err("drag path must contain 2 to 64 points".into());
        }
        points
            .iter()
            .map(|point| {
                Ok((
                    coordinate(point, "x", observation.width)?,
                    coordinate(point, "y", observation.height)?,
                ))
            })
            .collect()
    } else {
        Ok(vec![
            (
                coordinate(action, "x", observation.width)?,
                coordinate(action, "y", observation.height)?,
            ),
            (
                coordinate(action, "to_x", observation.width)?,
                coordinate(action, "to_y", observation.height)?,
            ),
        ])
    }
}
pub(crate) fn key_action(action: &Value) -> Result<(String, Vec<String>), String> {
    if action["type"] == "key" && action.get("keys").is_some() {
        return Err("keys is only supported for keypress; use modifiers with key".into());
    }
    if action["type"] == "keypress" && action.get("keys").is_some() {
        if action.get("key").is_some() || action.get("modifiers").is_some() {
            return Err("use either keys or key with modifiers for keypress, not both".into());
        }
        let keys = action["keys"].as_array().ok_or("keys must be an array")?;
        if !(1..=8).contains(&keys.len()) {
            return Err("keypress keys must contain 1 to 8 entries".into());
        }
        let mut key = None;
        let mut modifiers = Vec::new();
        for value in keys {
            let value = value.as_str().ok_or("each key must be a string")?;
            if value.chars().count() > 32 {
                return Err("key names must be at most 32 characters".into());
            }
            let upper = value.to_ascii_uppercase();
            let modifier = match upper.as_str() {
                "CTRL" | "CONTROL" => Some("CTRL"),
                "SHIFT" => Some("SHIFT"),
                "ALT" => Some("ALT"),
                "SUPER" | "META" | "WINDOWS" => Some("SUPER"),
                _ => None,
            };
            if let Some(modifier) = modifier {
                if !modifiers.iter().any(|existing| existing == modifier) {
                    modifiers.push(modifier.to_owned());
                }
            } else if key.is_none() {
                key = Some(value.to_owned());
            } else {
                return Err("keypress accepts one key and any modifiers".into());
            }
        }
        Ok((
            key.ok_or("keypress keys must include a non-modifier key")?,
            modifiers,
        ))
    } else {
        let key = required_string(action, "key")?;
        if key.chars().count() > 32 {
            return Err("key names must be at most 32 characters".into());
        }
        Ok((key.to_owned(), modifiers(action)?))
    }
}
pub(crate) fn select_accessibility_action(
    element: &AccessibleElement,
    action: &Value,
) -> Result<(String, Option<u64>), String> {
    let requested_name = action
        .get("action_name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty());
    let action_index = action
        .get("action_index")
        .map(|value| {
            value
                .as_u64()
                .ok_or("action_index must be a non-negative integer")
        })
        .transpose()?;
    if let Some(index) = action_index {
        let position = usize::try_from(index).map_err(|_| "action_index is too large")?;
        let advertised = element
            .actions
            .get(position)
            .ok_or("action_index is outside the element's advertised actions")?;
        let name = advertised
            .as_deref()
            .ok_or("AT-SPI did not provide an action name at action_index")?;
        if requested_name.is_some_and(|requested| requested != name) {
            return Err("action_name does not match the action at action_index".into());
        }
        Ok((name.to_owned(), Some(index)))
    } else {
        let name = requested_name.ok_or("action_name or action_index is required")?;
        if !element
            .actions
            .iter()
            .flatten()
            .any(|advertised| advertised == name)
        {
            return Err("action_name was not advertised for this element".into());
        }
        Ok((name.to_owned(), None))
    }
}
pub(crate) fn validate_observed_action(
    action: &Value,
    action_type: &str,
    observation: &Observation,
) -> Result<(), String> {
    match action_type {
        "click" | "double_click" => {
            coordinate(action, "x", observation.width)?;
            coordinate(action, "y", observation.height)?;
            pointer_button(action)?;
        }
        "move" => {
            coordinate(action, "x", observation.width)?;
            coordinate(action, "y", observation.height)?;
        }
        "drag" => {
            drag_path(action, observation)?;
            pointer_button(action)?;
        }
        "scroll" => {
            coordinate(action, "x", observation.width)?;
            coordinate(action, "y", observation.height)?;
        }
        "key" | "keypress" => {
            let (key, _) = key_action(action)?;
            validate_key(&normalize_key(&key))?;
        }
        "accessibility" => {
            let element_id = required_string(action, "element_id")?;
            let element = observation
                .elements
                .get(element_id)
                .ok_or("invalid element_id")?;
            if !element.on_screen {
                return Err("element is not visible in the observed output".into());
            }
            select_accessibility_action(element, action)?;
        }
        "set_text" => {
            let element_id = required_string(action, "element_id")?;
            let element = observation
                .elements
                .get(element_id)
                .ok_or("invalid element_id")?;
            if !element.editable || !element.on_screen {
                return Err("element is not an observed, visible editable text control".into());
            }
        }
        _ => {}
    }
    Ok(())
}
pub(crate) fn scroll_button(direction: &str) -> Result<u32, String> {
    match direction {
        "up" => Ok(4),
        "down" => Ok(5),
        "left" => Ok(6),
        "right" => Ok(7),
        _ => Err("invalid scroll direction".into()),
    }
}
pub(crate) fn pixel_distance_to_steps(distance: i32) -> i32 {
    if distance == 0 {
        return 0;
    }
    let steps = distance.unsigned_abs().div_ceil(120).max(1) as i32;
    if distance < 0 { -steps } else { steps }
}
pub(crate) fn pointer_button(action: &Value) -> Result<u32, String> {
    match action["button"].as_str().unwrap_or("left") {
        "left" => Ok(1),
        "middle" | "wheel" => Ok(2),
        "right" => Ok(3),
        "back" => Ok(8),
        "forward" => Ok(9),
        _ => Err("invalid button".into()),
    }
}
pub(crate) fn button_name(button: u32) -> &'static str {
    match button {
        1 => "left",
        2 => "middle",
        3 => "right",
        8 => "back",
        9 => "forward",
        _ => "unknown",
    }
}
pub(crate) fn modifiers(action: &Value) -> Result<Vec<String>, String> {
    let Some(values) = action.get("modifiers") else {
        return Ok(Vec::new());
    };
    let items = values.as_array().ok_or("modifiers must be an array")?;
    if items.len() > 4 {
        return Err("at most four modifier keys are accepted".into());
    }
    let mut result = Vec::new();
    for item in items {
        let value = item.as_str().ok_or("modifier must be a string")?;
        if value.chars().count() > 32 {
            return Err("modifier names must be at most 32 characters".into());
        }
        let value = value.to_ascii_uppercase();
        let modifier = match value.as_str() {
            "CTRL" | "CONTROL" => "CTRL",
            "SHIFT" => "SHIFT",
            "ALT" => "ALT",
            "SUPER" | "META" | "WINDOWS" => "SUPER",
            _ => return Err("unsupported modifier".into()),
        };
        if !result.iter().any(|existing| existing == modifier) {
            result.push(modifier.to_owned());
        }
    }
    Ok(result)
}
pub(crate) fn mouse_modifiers(action: &Value) -> Result<Vec<String>, String> {
    let Some(values) = action.get("keys") else {
        return Ok(Vec::new());
    };
    let values = values.as_array().ok_or("mouse keys must be an array")?;
    if values.len() > 4 {
        return Err("mouse actions accept at most four modifier keys".into());
    }
    let mut result = Vec::new();
    for item in values {
        let value = item.as_str().ok_or("mouse modifier must be a string")?;
        if value.chars().count() > 32 {
            return Err("mouse modifier names must be at most 32 characters".into());
        }
        let value = value.to_ascii_uppercase();
        let modifier = match value.as_str() {
            "CTRL" | "CONTROL" => "CTRL",
            "SHIFT" => "SHIFT",
            "ALT" => "ALT",
            "SUPER" | "META" | "WINDOWS" => "SUPER",
            _ => return Err("mouse keys may contain only CTRL, SHIFT, ALT, or SUPER".into()),
        };
        if !result.iter().any(|existing| existing == modifier) {
            result.push(modifier.to_owned());
        }
    }
    Ok(result)
}
pub(crate) fn modifier_key(modifier: &str) -> Result<&'static str, String> {
    match modifier {
        "CTRL" => Ok("Control_L"),
        "SHIFT" => Ok("Shift_L"),
        "ALT" => Ok("Alt_L"),
        "SUPER" => Ok("Super_L"),
        "ALTGR" => Ok("ALTGR"),
        _ => Err("unsupported mouse modifier".into()),
    }
}
pub(crate) fn normalize_key(key: &str) -> String {
    match key.to_ascii_uppercase().as_str() {
        "ENTER" | "RETURN" => "Return",
        "ESC" | "ESCAPE" => "Escape",
        "BACKSPACE" | "BACK_SPACE" => "BackSpace",
        "SPACE" => "space",
        "DEL" | "DELETE" => "Delete",
        "INS" | "INSERT" => "Insert",
        "HOME" => "Home",
        "END" => "End",
        "PAGEUP" | "PAGE_UP" | "PRIOR" => "Page_Up",
        "PAGEDOWN" | "PAGE_DOWN" | "NEXT" => "Page_Down",
        "UP" | "ARROWUP" | "ARROW_UP" => "Up",
        "DOWN" | "ARROWDOWN" | "ARROW_DOWN" => "Down",
        "LEFT" | "ARROWLEFT" | "ARROW_LEFT" => "Left",
        "RIGHT" | "ARROWRIGHT" | "ARROW_RIGHT" => "Right",
        "CTRL" | "CONTROL" => "Control_L",
        "SHIFT" => "Shift_L",
        "ALT" => "Alt_L",
        "SUPER" | "META" | "WINDOWS" => "Super_L",
        "CAPSLOCK" | "CAPS_LOCK" => "Caps_Lock",
        "NUMLOCK" | "NUM_LOCK" => "Num_Lock",
        "SCROLLLOCK" | "SCROLL_LOCK" => "Scroll_Lock",
        "PRINTSCREEN" | "PRINT_SCREEN" => "Print",
        "PAUSE" => "Pause",
        "BREAK" => "Break",
        _ => return key.to_owned(),
    }
    .to_owned()
}
pub(crate) fn character_key(character: char) -> Result<(String, bool), String> {
    let result = match character {
        'a'..='z' => (character.to_string(), false),
        'A'..='Z' => (character.to_ascii_lowercase().to_string(), true),
        '0'..='9' => (character.to_string(), false), ' ' => ("space".into(), false),
        '!' => ("1".into(), true), '@'=>("2".into(),true), '#'=>("3".into(),true), '$'=>("4".into(),true), '%'=>("5".into(),true), '^'=>("6".into(),true), '&'=>("7".into(),true), '*'=>("8".into(),true), '('=>("9".into(),true), ')'=>("0".into(),true),
        '-'|'_' => ("minus".into(), character=='_'), '='|'+' => ("equal".into(), character=='+'), '['|'{' => ("bracketleft".into(), character=='{'), ']'|'}' => ("bracketright".into(), character=='}'), '\\'|'|' => ("backslash".into(), character=='|'), ';'|':' => ("semicolon".into(), character==':'), '\''|'"' => ("apostrophe".into(), character=='"'), ','|'<' => ("comma".into(), character=='<'), '.'|'>' => ("period".into(), character=='>'), '/'|'?' => ("slash".into(), character=='?'), '`'|'~' => ("grave".into(), character=='~'),
        '\n'|'\r' => ("Return".into(),false), '\t' => ("Tab".into(),false),
        _ => return Err("text input supports ASCII characters only; use an accessibility action or paste in the target application".into()),
    };
    Ok(result)
}
pub(crate) fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty()
        || key.len() > 32
        || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err("key must be a short alphanumeric keysym name".into());
    }
    Ok(())
}
