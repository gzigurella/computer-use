use super::*;

pub(super) fn sway_layout() -> Result<(i32, i32, u32, u32), String> {
    let output = limited_command("2s", "swaymsg")
        .args(["-t", "get_outputs", "-r"])
        .output()
        .map_err(|error| format!("reading Sway outputs: {error}"))?;
    if !output.status.success() {
        return Err("swaymsg get_outputs failed; check that this process can access the active Sway session".into());
    }
    if output.stdout.len() > 1024 * 1024 {
        return Err("Sway output list exceeds 1 MiB".into());
    }
    let outputs: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid Sway output list: {error}"))?;
    let rects = outputs
        .as_array()
        .ok_or("Sway output list is not an array")?
        .iter()
        .filter(|output| output["active"] == true)
        .map(|output| {
            Ok::<_, String>((
                output["rect"]["x"]
                    .as_i64()
                    .ok_or("Sway output omitted its x position")?,
                output["rect"]["y"]
                    .as_i64()
                    .ok_or("Sway output omitted its y position")?,
                output["rect"]["width"]
                    .as_i64()
                    .filter(|width| *width > 0)
                    .ok_or("Sway output omitted its logical width")?,
                output["rect"]["height"]
                    .as_i64()
                    .filter(|height| *height > 0)
                    .ok_or("Sway output omitted its logical height")?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let origin_x = rects
        .iter()
        .map(|(x, _, _, _)| *x)
        .min()
        .ok_or("Sway returned no active outputs")?;
    let origin_y = rects
        .iter()
        .map(|(_, y, _, _)| *y)
        .min()
        .ok_or("Sway returned no active outputs")?;
    let right = rects
        .iter()
        .map(|(x, _, width, _)| x.checked_add(*width).ok_or("Sway output x bounds overflow"))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .ok_or("Sway returned no active outputs")?;
    let bottom = rects
        .iter()
        .map(|(_, y, _, height)| {
            y.checked_add(*height)
                .ok_or("Sway output y bounds overflow")
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .ok_or("Sway returned no active outputs")?;
    let width = right
        .checked_sub(origin_x)
        .ok_or("Sway logical layout width overflow")?;
    let height = bottom
        .checked_sub(origin_y)
        .ok_or("Sway logical layout height overflow")?;
    Ok((
        i32::try_from(origin_x).map_err(|_| "Sway output x position is out of range")?,
        i32::try_from(origin_y).map_err(|_| "Sway output y position is out of range")?,
        u32::try_from(width).map_err(|_| "Sway logical layout width is out of range")?,
        u32::try_from(height).map_err(|_| "Sway logical layout height is out of range")?,
    ))
}

pub(super) fn sway_active_window() -> Result<Option<Value>, String> {
    let output = limited_command("2s", "swaymsg")
        .args(["-t", "get_tree", "-r"])
        .output()
        .map_err(|error| format!("reading the active Sway window: {error}"))?;
    if !output.status.success() {
        return Err("swaymsg get_tree failed".into());
    }
    if output.stdout.len() > 4 * 1024 * 1024 {
        return Err("Sway tree exceeds 4 MiB".into());
    }
    let tree: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid Sway tree: {error}"))?;
    Ok(focused_sway_window(&tree, 0, &mut 10_000))
}

fn focused_sway_window(node: &Value, depth: usize, budget: &mut usize) -> Option<Value> {
    if depth > 64 || *budget == 0 {
        return None;
    }
    *budget -= 1;
    let mut children = node["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(node["floating_nodes"].as_array().into_iter().flatten())
        .collect::<Vec<_>>();
    children.sort_by_key(|child| child["focused"] != true);
    for child in children {
        if let Some(window) = focused_sway_window(child, depth + 1, budget) {
            return Some(window);
        }
    }
    if node["focused"] != true {
        return None;
    }
    let application = node["app_id"]
        .as_str()
        .or_else(|| node["window_properties"]["class"].as_str())
        .or_else(|| node["window_properties"]["instance"].as_str())
        .unwrap_or_default();
    let name = node["name"].as_str().unwrap_or_default();
    if application.is_empty() && name.is_empty() {
        return None;
    }
    Some(json!({"application":application,"name":name,"role":"window","window_id":node["id"]}))
}

pub(super) fn sway_move(observation: &Observation, x: u32, y: u32) -> Result<(), String> {
    let (x, y) = rounded_pointer_coordinates(observation, x, y);
    let global_x = i64::from(observation.origin_x) + i64::from(x);
    let global_y = i64::from(observation.origin_y) + i64::from(y);
    // Sway treats zero as "leave this axis unchanged" for cursor set.
    let global_x = i32::try_from(if global_x == 0 { 1 } else { global_x })
        .map_err(|_| "Sway cursor x coordinate is out of range")?;
    let global_y = i32::try_from(if global_y == 0 { 1 } else { global_y })
        .map_err(|_| "Sway cursor y coordinate is out of range")?;
    sway_command(&[
        "seat",
        "-",
        "cursor",
        "set",
        &global_x.to_string(),
        &global_y.to_string(),
    ])
}

pub(super) fn sway_pointer_button(button: u32, pressed: bool) -> Result<(), String> {
    let button = match button {
        1 => "button1",
        2 => "button2",
        3 => "button3",
        4 => "button4",
        5 => "button5",
        6 => "button6",
        7 => "button7",
        8 => "button8",
        9 => "button9",
        _ => return Err("unsupported Sway pointer button".into()),
    };
    sway_command(&[
        "seat",
        "-",
        "cursor",
        if pressed { "press" } else { "release" },
        button,
    ])
}

pub(super) fn sway_scroll(button: u32, amount: usize) -> Result<(), String> {
    let button = match button {
        4 => "button4",
        5 => "button5",
        6 => "button6",
        7 => "button7",
        _ => return Err("unsupported Sway scroll button".into()),
    };
    let commands = std::iter::repeat_n(
        format!("seat - cursor press {button}; seat - cursor release {button}"),
        amount,
    )
    .collect::<Vec<_>>()
    .join("; ");
    sway_command(&[&commands])
}

fn sway_command(args: &[&str]) -> Result<(), String> {
    let output = limited_command("3s", "swaymsg")
        .arg("--")
        .args(args)
        .output()
        .map_err(|error| format!("starting Sway IPC command: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Sway IPC command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let results: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid Sway IPC response: {error}"))?;
    let results = results
        .as_array()
        .ok_or("Sway IPC command returned no result list")?;
    if let Some(error) = results.iter().find(|result| result["success"] != true) {
        return Err(format!(
            "Sway rejected input command: {}",
            error["error"].as_str().unwrap_or("unknown Sway IPC error")
        ));
    }
    if results.is_empty() {
        return Err("Sway accepted no input command".into());
    }
    Ok(())
}
