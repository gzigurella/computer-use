//! Pointer and keyboard input on each Backend (pointer motion and buttons, key state, typing) and key name resolution.

use super::*;

pub(crate) async fn type_text_native(
    text: String,
    cancelled: Option<&mut tokio::sync::watch::Receiver<bool>>,
) -> Result<(), String> {
    let cancellation = Arc::new(AtomicBool::new(false));
    let worker_cancellation = cancellation.clone();
    let mut worker = tokio::task::spawn_blocking(move || {
        virtual_keyboard::type_text_cancellable(&text, &worker_cancellation)
    });
    if let Some(cancelled) = cancelled {
        tokio::select! {
            biased;
            result = &mut worker => result.map_err(|error| format!("native Wayland text worker failed: {error}"))?,
            _ = cancelled.changed() => {
                cancellation.store(true, Ordering::Release);
                let result = worker.await.map_err(|error| format!("native Wayland text worker failed during cleanup: {error}"))?;
                match result {
                    Ok(()) => Err("native Wayland text input cancelled after the current key was released".into()),
                    Err(error) => Err(error),
                }
            }
        }
    } else {
        worker
            .await
            .map_err(|error| format!("native Wayland text worker failed: {error}"))?
    }
}
pub(crate) fn smooth_scroll_available(backend: &Backend) -> bool {
    if direct_scroll_pointer(backend).is_some() {
        return true;
    }
    matches!(backend, Backend::Wayland { portal, .. } if portal.smooth_scroll_available())
}
pub(crate) fn hyprland_portal_scroll_available(
    hyprland: bool,
    portal_scroll: bool,
    positioned_monitor: bool,
    virtual_pointer: bool,
) -> bool {
    portal_scroll || (hyprland && positioned_monitor && virtual_pointer)
}
pub(crate) async fn move_pointer(
    backend: &Backend,
    observation: &Observation,
    x: u32,
    y: u32,
    click_target: bool,
) -> Result<(), String> {
    if let Backend::Hyprland {
        direct_pointer: Some(pointer),
        ..
    } = backend
    {
        let (x, y) = rounded_pointer_coordinates(observation, x, y);
        let layout = hyprland_layout()?;
        let (x, y, width, height) = pointer_coordinates_in_layout(
            x,
            y,
            (observation.origin_x, observation.origin_y),
            layout,
        )?;
        return pointer
            .lock()
            .map_err(|_| "virtual-pointer lock is unavailable")?
            .pointer(f64::from(x), f64::from(y), width, height);
    }
    if let Some(pointer) = direct_pointer(backend) {
        let (x, y) = portal_coordinates(observation, x, y);
        return pointer
            .lock()
            .map_err(|_| "virtual-pointer lock is unavailable")?
            .pointer(x, y, observation.logical_width, observation.logical_height);
    }
    match backend {
        Backend::Wayland {
            portal,
            hyprland: true,
            ..
        } if !portal.pointer_available() => {
            if !portal.monitor_geometry_available() || !observation.origin_known {
                return Err("Hyprland pointer fallback needs positioned monitor streams; use portal input or select a positioned monitor".into());
            }
            let (x, y) = rounded_pointer_coordinates(observation, x, y);
            let coordinates = format!(
                "{} {}",
                x as i32 + observation.origin_x,
                y as i32 + observation.origin_y
            );
            run_blocking_result("Hyprland pointer", move || {
                hypr("dispatch", &["movecursor", &coordinates])
            })
            .await
        }
        Backend::Wayland { portal, .. } => {
            let (x, y) = portal_coordinates(observation, x, y);
            if click_target {
                portal.pointer_for_click(x, y).await
            } else {
                portal.pointer(x, y).await
            }
        }
        Backend::Hyprland { .. } => {
            let (x, y) = rounded_pointer_coordinates(observation, x, y);
            let coordinates = format!(
                "{} {}",
                x as i32 + observation.origin_x,
                y as i32 + observation.origin_y
            );
            run_blocking_result("Hyprland pointer", move || {
                hypr("dispatch", &["movecursor", &coordinates])
            })
            .await
        }
        Backend::Sway { .. } => {
            let observation = observation.clone();
            run_blocking_result("Sway pointer", move || sway::sway_move(&observation, x, y)).await
        }
        Backend::WaylandGrim { .. } => {
            Err("Wayland input is unavailable in the Grim capture-only session".into())
        }
        Backend::X11 { .. } => {
            let (x, y) = rounded_pointer_coordinates(observation, x, y);
            run_blocking_result("X11 pointer", move || x11::x11_move(x as i32, y as i32)).await
        }
        Backend::Unsupported => Err("pointer input is unsupported".into()),
    }
}
pub(crate) fn pointer_coordinates_in_layout(
    x: u32,
    y: u32,
    monitor_origin: (i32, i32),
    layout: (i32, i32, u32, u32),
) -> Result<(u32, u32, u32, u32), String> {
    let x = i64::from(monitor_origin.0) + i64::from(x) - i64::from(layout.0);
    let y = i64::from(monitor_origin.1) + i64::from(y) - i64::from(layout.1);
    let width = layout.2;
    let height = layout.3;
    if x < 0 || y < 0 || x >= i64::from(width) || y >= i64::from(height) {
        return Err(
            "selected Hyprland output is outside the virtual-pointer desktop layout".into(),
        );
    }
    Ok((x as u32, y as u32, width, height))
}
pub(crate) async fn pointer_button_state(
    backend: &Backend,
    button: u32,
    pressed: bool,
) -> Result<(), String> {
    if let Some(pointer) = direct_pointer(backend) {
        return pointer
            .lock()
            .map_err(|_| "virtual-pointer lock is unavailable")?
            .button(evdev_button_code(button), pressed);
    }
    match backend {
        Backend::Wayland {
            portal,
            hyprland: true,
            ..
        } if !portal.pointer_available() => {
            if !portal.monitor_geometry_available() {
                return Err("Hyprland pointer fallback needs positioned monitor streams; use portal input or select a positioned monitor".into());
            }
            let state = if pressed { "down" } else { "up" };
            let key = format!(",{},{},activewindow", hypr_mouse_button(button), state);
            run_blocking_result("Hyprland pointer button", move || {
                hypr("dispatch", &["sendkeystate", &key])
            })
            .await
        }
        Backend::Wayland { portal, .. } => portal.button(button, pressed).await,
        Backend::Hyprland { .. } => {
            let state = if pressed { "down" } else { "up" };
            let key = format!(",{},{},activewindow", hypr_mouse_button(button), state);
            run_blocking_result("Hyprland pointer button", move || {
                hypr("dispatch", &["sendkeystate", &key])
            })
            .await
        }
        Backend::Sway { .. } => {
            run_blocking_result("Sway pointer button", move || {
                sway::sway_pointer_button(button, pressed)
            })
            .await
        }
        Backend::WaylandGrim { .. } => {
            Err("Wayland input is unavailable in the Grim capture-only session".into())
        }
        Backend::X11 { .. } => x11::x11_button_state(button, pressed),
        Backend::Unsupported => Err("pointer input is unsupported".into()),
    }
}
pub(crate) async fn release_pointer_button(backend: &Backend, button: u32) -> Result<(), String> {
    match pointer_button_state(backend, button, false).await {
        Ok(()) => Ok(()),
        Err(first) => pointer_button_state(backend, button, false)
            .await
            .map_err(|second| format!("button release failed ({first}); retry failed ({second})")),
    }
}
pub(crate) fn direct_pointer(
    backend: &Backend,
) -> Option<&Arc<std::sync::Mutex<virtual_pointer::VirtualPointer>>> {
    match backend {
        Backend::Hyprland { direct_pointer, .. }
        | Backend::Sway { direct_pointer, .. }
        | Backend::WaylandGrim { direct_pointer, .. } => direct_pointer.as_ref(),
        _ => None,
    }
}
pub(crate) fn direct_scroll_pointer(
    backend: &Backend,
) -> Option<&Arc<std::sync::Mutex<virtual_pointer::VirtualPointer>>> {
    match backend {
        Backend::Wayland { direct_pointer, .. } => direct_pointer.as_ref(),
        _ => direct_pointer(backend),
    }
}
pub(crate) fn direct_keyboard(
    backend: &Backend,
) -> Option<&Arc<std::sync::Mutex<virtual_keyboard::VirtualKeyboard>>> {
    match backend {
        Backend::Wayland {
            direct_keyboard, ..
        }
        | Backend::Hyprland {
            direct_keyboard, ..
        } => direct_keyboard.as_ref(),
        _ => None,
    }
}
pub(crate) fn wtype_keyboard_available(backend: &Backend) -> bool {
    matches!(
        backend,
        Backend::Hyprland {
            wtype_keyboard: true,
            ..
        }
    ) || matches!(
        backend,
        Backend::Wayland {
            virtual_keyboard: true,
            ..
        }
    )
}
pub(crate) fn uses_hyprland_keyboard(backend: &Backend) -> bool {
    matches!(backend, Backend::Hyprland { .. })
        || matches!(backend, Backend::Wayland { portal, hyprland: true, .. } if !portal.keyboard_available())
}
pub(crate) fn hypr_mouse_button(button: u32) -> &'static str {
    match button {
        1 => "mouse:272",
        2 => "mouse:274",
        3 => "mouse:273",
        8 => "mouse:275",
        9 => "mouse:276",
        _ => "mouse:273",
    }
}
pub(crate) fn evdev_button_code(button: u32) -> u32 {
    match button {
        1 => 272,
        2 => 274,
        3 => 273,
        8 => 275,
        9 => 276,
        _ => 273,
    }
}
pub(crate) async fn modifier_state(
    backend: &Backend,
    modifier: &str,
    pressed: bool,
) -> Result<(), String> {
    key_state(backend, modifier_key(modifier)?, pressed).await
}
pub(crate) async fn press_mouse_modifiers(
    backend: &Backend,
    modifiers: &[String],
    held: &mut HeldInput,
) -> Result<(), String> {
    for modifier in modifiers {
        let key = modifier_key(modifier)?;
        held.add_key(key);
        modifier_state(backend, modifier, true).await?;
    }
    Ok(())
}
pub(crate) async fn key_state(backend: &Backend, key: &str, pressed: bool) -> Result<(), String> {
    validate_key(key)?;
    if uses_hyprland_keyboard(backend) {
        let keyboard = direct_keyboard(backend)
            .cloned()
            .ok_or("held modifiers require Hyprland virtual-keyboard support")?;
        let key = key.to_owned();
        return run_blocking_result("Hyprland modifier", move || {
            keyboard
                .lock()
                .map_err(|_| "virtual keyboard lock was poisoned".to_owned())?
                .key_state(&key, pressed)
        })
        .await;
    }
    match backend {
        Backend::Wayland { portal, .. } if portal.keyboard_available() => {
            portal.key(portal_keysym(key)?, pressed).await
        }
        Backend::X11 { .. } => x11::x11_key_state(key, pressed),
        _ => Err("held key input is not supported by the active input backend".into()),
    }
}
pub(crate) async fn release_key_state(backend: &Backend, key: &str) -> Result<(), String> {
    match key_state(backend, key, false).await {
        Ok(()) => Ok(()),
        Err(first) => key_state(backend, key, false)
            .await
            .map_err(|second| format!("key release failed ({first}); retry failed ({second})")),
    }
}
pub(crate) async fn portal_key_state(
    backend: &Backend,
    keysym: u32,
    pressed: bool,
) -> Result<(), String> {
    match backend {
        Backend::Wayland { portal, .. } => portal.key(keysym, pressed).await,
        _ => Err("portal keysym input is unavailable on the active backend".into()),
    }
}
pub(crate) async fn release_portal_keysym(backend: &Backend, keysym: u32) -> Result<(), String> {
    match portal_key_state(backend, keysym, false).await {
        Ok(()) => Ok(()),
        Err(first) => portal_key_state(backend, keysym, false)
            .await
            .map_err(|second| format!("key release failed ({first}); retry failed ({second})")),
    }
}
pub(crate) async fn send_key(
    backend: &Backend,
    key: &str,
    modifiers: &[String],
) -> Result<(Option<(String, &'static str)>, Vec<String>), String> {
    let key = normalize_key(key);
    validate_key(&key)?;
    if uses_hyprland_keyboard(backend) {
        let Some(keyboard) = direct_keyboard(backend).cloned() else {
            if wtype_keyboard_available(backend) {
                let key = key.clone();
                let modifiers = modifiers.to_vec();
                run_blocking_result("wtype key", move || wtype_key(&key, &modifiers)).await?;
                return Ok((None, Vec::new()));
            }
            return Err(
                "keyboard input is unavailable; the compositor has no active virtual keyboard"
                    .into(),
            );
        };
        let refreshed_layout = run_blocking_result("Hyprland XKB layout refresh", move || {
            keyboard
                .lock()
                .map_err(|_| "virtual keyboard lock was poisoned".to_owned())?
                .refresh_keymap()
        })
        .await?;
        let keyboard = direct_keyboard(backend)
            .cloned()
            .ok_or("Hyprland virtual keyboard stopped during key input")?;
        let key_name = key.clone();
        let automatic_modifiers = run_blocking_result("Hyprland XKB key resolution", move || {
            keyboard
                .lock()
                .map_err(|_| "virtual keyboard lock was poisoned".to_owned())?
                .required_modifiers(&key_name)
        })
        .await?;
        let mut applied_modifiers = modifiers.to_vec();
        for modifier in &automatic_modifiers {
            if !applied_modifiers.iter().any(|applied| applied == modifier) {
                applied_modifiers.push((*modifier).to_owned());
            }
        }
        let mut held = HeldInput::new(backend);
        for modifier in &applied_modifiers {
            let name = modifier_key(modifier)?;
            held.add_key(name);
            if let Err(error) = key_state(backend, name, true).await {
                return Err(match held.release_all().await {
                    Ok(()) => error,
                    Err(cleanup) => format!("{error}; modifier cleanup failed: {cleanup}"),
                });
            }
        }
        held.add_key(&key);
        let key_down = key_state(backend, &key, true).await;
        let key_up = release_key_state(backend, &key).await;
        if key_up.is_ok() {
            held.remove_key(&key);
        }
        let key_result = portal_key_cycle(key_down, key_up);
        let modifier_result = held.release_all().await;
        return match (key_result, modifier_result) {
            (Ok(()), Ok(())) => Ok((
                refreshed_layout,
                automatic_modifiers.into_iter().map(str::to_owned).collect(),
            )),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(error), Err(cleanup)) => {
                Err(format!("{error}; modifier cleanup failed: {cleanup}"))
            }
        };
    }
    match backend {
        Backend::Wayland { virtual_keyboard: true, portal, xkb, .. } if !portal.keyboard_available() || !xkb => {
            let key = key.clone();
            let modifiers = modifiers.to_vec();
            run_blocking_result("wtype key", move || wtype_key(&key, &modifiers)).await?;
            Ok((None, Vec::new()))
        }
        Backend::Wayland { .. } => {
            let mut held = HeldInput::new(backend);
            for modifier in modifiers {
                let name = modifier_key(modifier)?;
                held.add_key(name);
                if let Err(error) = key_state(backend, name, true).await {
                    return Err(match held.release_all().await {
                        Ok(()) => error,
                        Err(cleanup) => format!("{error}; modifier cleanup failed: {cleanup}"),
                    });
                }
            }
            held.add_key(&key);
            let key_down = key_state(backend, &key, true).await;
            let key_up = release_key_state(backend, &key).await;
            if key_up.is_ok() { held.remove_key(&key); }
            let key_result = portal_key_cycle(key_down, key_up);
            let modifier_result = held.release_all().await;
            match (key_result, modifier_result) {
                (Ok(()), Ok(())) => Ok((None, Vec::new())),
                (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
                (Err(error), Err(cleanup)) => Err(format!("{error}; modifier cleanup failed: {cleanup}")),
            }
        }
        Backend::Hyprland { .. } => unreachable!("Hyprland key action returned before direct routing"),
        Backend::Sway { virtual_keyboard: true, .. } | Backend::WaylandGrim { virtual_keyboard: true, .. } => {
            let key = key.clone();
            let modifiers = modifiers.to_vec();
            run_blocking_result("wtype key", move || wtype_key(&key, &modifiers)).await?;
            Ok((None, Vec::new()))
        }
        Backend::Sway { .. } => Err("keyboard input is unavailable; install wtype and use a compositor with virtual-keyboard support".into()),
        Backend::WaylandGrim { .. } => Err("Wayland input is unavailable in the Grim capture-only session".into()),
        Backend::X11 { .. } => {
            let key = key.clone();
            let modifiers = modifiers.to_vec();
            run_blocking_result("X11 key", move || x11::x11_key(&key, &modifiers)).await?;
            Ok((None, Vec::new()))
        }
        Backend::Unsupported => Err("keyboard input is unsupported".into()),
    }
}
pub(crate) async fn release_portal_key(
    portal: &portal::PortalSession,
    keysym: u32,
) -> Result<(), String> {
    match portal.key(keysym, false).await {
        Ok(()) => Ok(()),
        Err(first) => portal
            .key(keysym, false)
            .await
            .map_err(|second| format!("key release failed ({first}); retry failed ({second})")),
    }
}
pub(crate) fn portal_key_cycle(
    down: Result<(), String>,
    up: Result<(), String>,
) -> Result<(), String> {
    match (down, up) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(release)) => Err(format!("{error}; key release failed: {release}")),
    }
}
pub(crate) fn portal_coordinates(observation: &Observation, x: u32, y: u32) -> (f64, f64) {
    (
        x as f64 * observation.logical_width as f64 / observation.width as f64,
        y as f64 * observation.logical_height as f64 / observation.height as f64,
    )
}
pub(crate) fn rounded_pointer_coordinates(observation: &Observation, x: u32, y: u32) -> (u32, u32) {
    let (x, y) = portal_coordinates(observation, x, y);
    (
        x.round()
            .clamp(0.0, observation.logical_width.saturating_sub(1) as f64) as u32,
        y.round()
            .clamp(0.0, observation.logical_height.saturating_sub(1) as f64) as u32,
    )
}
pub(crate) fn portal_keysym(key: &str) -> Result<u32, String> {
    if let Some(keysym) = builtin_portal_keysym(key) {
        return Ok(keysym);
    }
    let library = DynamicLibrary::open(c"libxkbcommon.so.0")?;
    let keysym_from_name: unsafe extern "C" fn(*const c_char, c_int) -> u32 =
        library.symbol(c"xkb_keysym_from_name")?;
    let key = CString::new(key).map_err(|_| "invalid key name")?;
    let value = unsafe { keysym_from_name(key.as_ptr(), 1) };
    if value == 0 {
        Err("unknown XKB keysym".into())
    } else {
        Ok(value)
    }
}
pub(crate) fn builtin_portal_keysym(key: &str) -> Option<u32> {
    let keysym = match key {
        "BackSpace" => 0xff08,
        "Tab" => 0xff09,
        "Return" => 0xff0d,
        "Pause" => 0xff13,
        "Scroll_Lock" => 0xff14,
        "Escape" => 0xff1b,
        "Home" => 0xff50,
        "Left" => 0xff51,
        "Up" => 0xff52,
        "Right" => 0xff53,
        "Down" => 0xff54,
        "Page_Up" => 0xff55,
        "Page_Down" => 0xff56,
        "End" => 0xff57,
        "Print" => 0xff61,
        "Insert" => 0xff63,
        "Break" => 0xff6b,
        "Num_Lock" => 0xff7f,
        "KP_Space" => 0xff80,
        "KP_Enter" => 0xff8d,
        "KP_Home" => 0xff95,
        "KP_Left" => 0xff96,
        "KP_Up" => 0xff97,
        "KP_Right" => 0xff98,
        "KP_Down" => 0xff99,
        "KP_Page_Up" => 0xff9a,
        "KP_Page_Down" => 0xff9b,
        "KP_End" => 0xff9c,
        "KP_Begin" => 0xff9d,
        "KP_Insert" => 0xff9e,
        "KP_Delete" => 0xff9f,
        "KP_Multiply" => 0xffaa,
        "KP_Add" => 0xffab,
        "KP_Separator" => 0xffac,
        "KP_Subtract" => 0xffad,
        "KP_Decimal" => 0xffae,
        "KP_Divide" => 0xffaf,
        "KP_Equal" => 0xffbd,
        "minus" => 0x2d,
        "equal" => 0x3d,
        "bracketleft" => 0x5b,
        "bracketright" => 0x5d,
        "backslash" => 0x5c,
        "semicolon" => 0x3b,
        "apostrophe" => 0x27,
        "comma" => 0x2c,
        "period" => 0x2e,
        "slash" => 0x2f,
        "grave" => 0x60,
        "Shift_L" => 0xffe1,
        "Control_L" => 0xffe3,
        "Caps_Lock" => 0xffe5,
        "Alt_L" => 0xffe9,
        "Super_L" => 0xffeb,
        "Delete" => 0xffff,
        _ => {
            if let Some(number) = key
                .strip_prefix("KP_")
                .and_then(|number| number.parse::<u32>().ok())
                .filter(|number| *number <= 9)
            {
                return Some(0xffb0 + number);
            }
            if let Some(number) = key
                .strip_prefix('F')
                .and_then(|number| number.parse::<u32>().ok())
                .filter(|number| (1..=35).contains(number))
            {
                return Some(0xffbd + number);
            }
            if (key.len() == 1 && key.as_bytes()[0].is_ascii_graphic()) || key == "space" {
                return Some(if key == "space" {
                    0x20
                } else {
                    u32::from(key.as_bytes()[0])
                });
            }
            return None;
        }
    };
    Some(keysym)
}
pub(crate) fn xkb_available() -> bool {
    let Ok(library) = DynamicLibrary::open(c"libxkbcommon.so.0") else {
        return false;
    };
    let symbol: Result<unsafe extern "C" fn(*const c_char, c_int) -> u32, _> =
        library.symbol(c"xkb_keysym_from_name");
    symbol.is_ok()
        && library
            .symbol::<unsafe extern "C" fn(u32) -> u32>(c"xkb_utf32_to_keysym")
            .is_ok()
        && library
            .symbol::<unsafe extern "C" fn(u32, *mut c_char, usize) -> c_int>(
                c"xkb_keysym_get_name",
            )
            .is_ok()
}
pub(crate) fn wtype_available() -> bool {
    if !command_exists("wtype") {
        return false;
    }
    let Ok(mut child) = limited_command("2s", "wtype")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    drop(child.stdin.take());
    child.wait().is_ok_and(|status| status.success())
}
