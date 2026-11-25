//! Output geometry, pointer position and active-window queries for each Target.

use super::*;

pub(crate) fn hyprland_layout() -> Result<(i32, i32, u32, u32), String> {
    let output = hyprland_json("monitors", 1024 * 1024, Duration::from_secs(2))?;
    let monitors: Value = serde_json::from_slice(&output)
        .map_err(|error| format!("invalid Hyprland monitor list: {error}"))?;
    let monitors = monitors
        .as_array()
        .ok_or("Hyprland monitor list is not an array")?;
    let rects = monitors
        .iter()
        .filter(|monitor| monitor["disabled"] != true)
        .map(|monitor| {
            let x = monitor["x"]
                .as_i64()
                .ok_or("Hyprland monitor omitted its x position")?;
            let y = monitor["y"]
                .as_i64()
                .ok_or("Hyprland monitor omitted its y position")?;
            let (width, height, _) = hyprland_monitor_logical_size(monitor)?;
            Ok::<_, String>((x, y, i64::from(width), i64::from(height)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let origin_x = rects
        .iter()
        .map(|(x, _, _, _)| *x)
        .min()
        .ok_or("Hyprland returned no active monitors")?;
    let origin_y = rects
        .iter()
        .map(|(_, y, _, _)| *y)
        .min()
        .ok_or("Hyprland returned no active monitors")?;
    let right = rects
        .iter()
        .map(|(x, _, width, _)| {
            x.checked_add(*width)
                .ok_or("Hyprland monitor x bounds overflow")
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .ok_or("Hyprland returned no active monitors")?;
    let bottom = rects
        .iter()
        .map(|(_, y, _, height)| {
            y.checked_add(*height)
                .ok_or("Hyprland monitor y bounds overflow")
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .ok_or("Hyprland returned no active monitors")?;
    let width = right
        .checked_sub(origin_x)
        .ok_or("Hyprland layout width overflow")?;
    let height = bottom
        .checked_sub(origin_y)
        .ok_or("Hyprland layout height overflow")?;
    Ok((
        i32::try_from(origin_x).map_err(|_| "Hyprland monitor x position is out of range")?,
        i32::try_from(origin_y).map_err(|_| "Hyprland monitor y position is out of range")?,
        u32::try_from(width).map_err(|_| "Hyprland layout width is out of range")?,
        u32::try_from(height).map_err(|_| "Hyprland layout height is out of range")?,
    ))
}

pub(crate) fn hyprland_monitor_geometry(name: &str) -> Result<(i32, i32, u32, u32, f64), String> {
    let output = hyprland_json("monitors", 1024 * 1024, Duration::from_secs(2))?;
    let monitors: Value = serde_json::from_slice(&output)
        .map_err(|error| format!("invalid Hyprland monitor list: {error}"))?;
    let monitor = monitors
        .as_array()
        .and_then(|monitors| {
            monitors
                .iter()
                .find(|monitor| monitor["name"] == name && monitor["disabled"] != true)
        })
        .ok_or_else(|| format!("Hyprland output {name:?} is no longer active"))?;
    let x = i32::try_from(
        monitor["x"]
            .as_i64()
            .ok_or("Hyprland monitor omitted its x position")?,
    )
    .map_err(|_| "Hyprland monitor x position is out of range")?;
    let y = i32::try_from(
        monitor["y"]
            .as_i64()
            .ok_or("Hyprland monitor omitted its y position")?,
    )
    .map_err(|_| "Hyprland monitor y position is out of range")?;
    let (width, height, scale) = hyprland_monitor_logical_size(monitor)?;
    Ok((x, y, width, height, scale))
}

pub(crate) fn hyprland_monitor_layout(name: &str) -> Result<(i32, i32, u32, u32), String> {
    let (x, y, width, height, _) = hyprland_monitor_geometry(name)?;
    Ok((x, y, width, height))
}

pub(crate) fn hyprland_cursor_position() -> Option<(i32, i32)> {
    let output = hyprland_json("cursorpos", 64 * 1024, Duration::from_secs(2)).ok()?;
    let position: Value = serde_json::from_slice(&output).ok()?;
    Some((
        i32::try_from(position["x"].as_i64()?).ok()?,
        i32::try_from(position["y"].as_i64()?).ok()?,
    ))
}

pub(crate) fn captured_pointer_position(
    position: (i32, i32),
    origin_x: i32,
    origin_y: i32,
    logical_width: u32,
    logical_height: u32,
    image: &[u8],
) -> Option<(i32, i32)> {
    let (width, height) = jpeg_dimensions(image)?;
    if logical_width == 0 || logical_height == 0 {
        return None;
    }
    let (x, y) = (
        i64::from(position.0) - i64::from(origin_x),
        i64::from(position.1) - i64::from(origin_y),
    );
    if x < 0 || y < 0 || x >= i64::from(logical_width) || y >= i64::from(logical_height) {
        return None;
    }
    Some((
        (x * i64::from(width) / i64::from(logical_width)) as i32,
        (y * i64::from(height) / i64::from(logical_height)) as i32,
    ))
}

pub(crate) fn wlroots_layout() -> Result<(i32, i32, u32, u32), String> {
    let output = limited_command("2s", "wlr-randr")
        .output()
        .map_err(|error| format!("reading wlroots outputs: {error}"))?;
    if !output.status.success() {
        return Err("wlr-randr could not read the active output layout".into());
    }
    if output.stdout.len() > 1024 * 1024 {
        return Err("wlroots output list exceeds 1 MiB".into());
    }
    parse_wlr_layout(
        std::str::from_utf8(&output.stdout).map_err(|_| "wlr-randr returned invalid UTF-8")?,
    )
}

pub(crate) fn parse_wlr_layout(text: &str) -> Result<(i32, i32, u32, u32), String> {
    #[derive(Default)]
    struct Output {
        enabled: bool,
        position: Option<(i64, i64)>,
        mode: Option<(u32, u32)>,
        scale: Option<f64>,
        transform: Option<String>,
    }
    impl Output {
        fn rect(self) -> Result<Option<(i64, i64, i64, i64)>, String> {
            if !self.enabled {
                return Ok(None);
            }
            let (x, y) = self
                .position
                .ok_or("wlr-randr omitted an enabled output position")?;
            let (mut width, mut height) = self
                .mode
                .ok_or("wlr-randr omitted an enabled output's current mode")?;
            let scale = self
                .scale
                .filter(|scale| scale.is_finite() && *scale > 0.0)
                .ok_or("wlr-randr omitted an enabled output scale")?;
            let transform = self
                .transform
                .ok_or("wlr-randr omitted an enabled output transform")?;
            if matches!(
                transform.as_str(),
                "90" | "270" | "flipped-90" | "flipped-270"
            ) {
                std::mem::swap(&mut width, &mut height);
            }
            let (width, height) = (
                (f64::from(width) / scale).round(),
                (f64::from(height) / scale).round(),
            );
            let valid_dimension =
                |value: f64| value.is_finite() && (1.0..=f64::from(u32::MAX)).contains(&value);
            if !valid_dimension(width) || !valid_dimension(height) {
                return Err("wlr-randr reported an invalid logical output size".into());
            }
            Ok(Some((x, y, width as i64, height as i64)))
        }
    }
    let mut current: Option<Output> = None;
    let mut rects = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if !line.as_bytes().first().is_some_and(u8::is_ascii_whitespace) {
            if let Some(output) = current.take()
                && let Some(rect) = output.rect()?
            {
                rects.push(rect);
            }
            current = Some(Output::default());
            continue;
        }
        let Some(output) = current.as_mut() else {
            continue;
        };
        let trimmed = line.trim();
        if trimmed.contains("current") && trimmed.contains(" px") {
            if let Some(dimensions) = trimmed.split_whitespace().next()
                && let Some((width, height)) = dimensions.split_once('x')
            {
                output.mode = Some((
                    width.parse().map_err(|_| "invalid current mode width")?,
                    height.parse().map_err(|_| "invalid current mode height")?,
                ));
            }
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key {
            "Enabled" => output.enabled = value == "yes",
            "Position" => {
                let (x, y) = value
                    .split_once(',')
                    .ok_or("wlr-randr returned an invalid output position")?;
                output.position = Some((
                    x.trim().parse().map_err(|_| "invalid output x position")?,
                    y.trim().parse().map_err(|_| "invalid output y position")?,
                ));
            }
            "Scale" => output.scale = value.parse().ok(),
            "Transform" => output.transform = Some(value.to_owned()),
            _ => {}
        }
    }
    if let Some(output) = current
        && let Some(rect) = output.rect()?
    {
        rects.push(rect);
    }
    let origin_x = rects
        .iter()
        .map(|rect| rect.0)
        .min()
        .ok_or("wlr-randr reported no enabled outputs")?;
    let origin_y = rects
        .iter()
        .map(|rect| rect.1)
        .min()
        .ok_or("wlr-randr reported no enabled outputs")?;
    let right = rects
        .iter()
        .map(|rect| rect.0.checked_add(rect.2).ok_or("output x bounds overflow"))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .ok_or("wlr-randr reported no enabled outputs")?;
    let bottom = rects
        .iter()
        .map(|rect| rect.1.checked_add(rect.3).ok_or("output y bounds overflow"))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .ok_or("wlr-randr reported no enabled outputs")?;
    let width = right
        .checked_sub(origin_x)
        .ok_or("wlroots layout width overflow")?;
    let height = bottom
        .checked_sub(origin_y)
        .ok_or("wlroots layout height overflow")?;
    Ok((
        i32::try_from(origin_x).map_err(|_| "wlroots output x position is out of range")?,
        i32::try_from(origin_y).map_err(|_| "wlroots output y position is out of range")?,
        u32::try_from(width).map_err(|_| "wlroots layout width is out of range")?,
        u32::try_from(height).map_err(|_| "wlroots layout height is out of range")?,
    ))
}

pub(crate) fn compositor_active_window(backend: &Backend) -> Result<Option<Value>, String> {
    match backend {
        Backend::Hyprland { .. } | Backend::Wayland { hyprland: true, .. } => {
            hyprland_active_window()
        }
        Backend::Sway { .. } => sway::sway_active_window(),
        Backend::X11 { .. } => x11::x11_active_window(),
        _ => Ok(None),
    }
}

pub(crate) fn hyprland_active_window() -> Result<Option<Value>, String> {
    let output = hyprland_json("activewindow", 64 * 1024, Duration::from_secs(2))?;
    let window: Value = serde_json::from_slice(&output)
        .map_err(|error| format!("invalid Hyprland active window response: {error}"))?;
    let application = window["class"].as_str().unwrap_or_default();
    let name = window["title"].as_str().unwrap_or_default();
    if application.is_empty() && name.is_empty() {
        return Ok(None);
    }
    let mut active = json!({"application":application,"name":name,"role":"window","window_id":window["address"],"process_id":window["pid"]});
    if let (Some(x), Some(y), Some(width), Some(height)) = (
        window["at"][0].as_i64(),
        window["at"][1].as_i64(),
        window["size"][0].as_u64(),
        window["size"][1].as_u64(),
    ) && width > 0
        && height > 0
    {
        active["bounds"] =
            json!({"x":x,"y":y,"width":width,"height":height,"coordinate_space":"screen"});
    }
    Ok(Some(active))
}
