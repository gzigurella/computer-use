use super::*;
use std::ffi::{c_uchar, c_uint, c_ulong};

pub(super) fn x11_active_window() -> Result<Option<Value>, String> {
    if !command_exists("xdotool") {
        let Ok(display) = XDisplay::open() else {
            return Ok(None);
        };
        let Some(get_input_focus) = display.api.get_input_focus else {
            return Ok(None);
        };
        let mut window = 0;
        let mut revert_to = 0;
        if unsafe { get_input_focus(display.ptr, &mut window, &mut revert_to) } == 0
            || window == 0
            || Some(window)
                == display
                    .api
                    .default_root_window
                    .map(|root| unsafe { root(display.ptr) })
        {
            return Ok(None);
        }
        return Ok(Some(json!({"window_id":window.to_string()})));
    }
    let output = limited_command("2s", "xdotool")
        .arg("getwindowfocus")
        .output()
        .map_err(|error| format!("reading the active X11 window: {error}"))?;
    if !output.status.success() {
        return Err("xdotool getwindowfocus failed".into());
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if id.is_empty() || id == "0" {
        return Ok(None);
    }
    let valid_id = id.strip_prefix("0x").map_or_else(
        || id.chars().all(|character| character.is_ascii_digit()),
        |hex| !hex.is_empty() && hex.chars().all(|character| character.is_ascii_hexdigit()),
    );
    if !valid_id {
        return Err("xdotool returned an invalid X11 window ID".into());
    }
    Ok(Some(json!({"window_id":id})))
}

type XOpenDisplayFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type XCloseDisplayFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type XDefaultRootWindowFn = unsafe extern "C" fn(*mut c_void) -> c_ulong;
type XGetInputFocusFn = unsafe extern "C" fn(*mut c_void, *mut c_ulong, *mut c_int) -> c_int;
type XGetGeometryFn = unsafe extern "C" fn(
    *mut c_void,
    c_ulong,
    *mut c_ulong,
    *mut c_int,
    *mut c_int,
    *mut c_uint,
    *mut c_uint,
    *mut c_uint,
    *mut c_uint,
) -> c_int;
type XQueryPointerFn = unsafe extern "C" fn(
    *mut c_void,
    c_ulong,
    *mut c_ulong,
    *mut c_ulong,
    *mut c_int,
    *mut c_int,
    *mut c_int,
    *mut c_int,
    *mut c_uint,
) -> c_int;
type XStringToKeysymFn = unsafe extern "C" fn(*const c_char) -> c_ulong;
type XKeysymToKeycodeFn = unsafe extern "C" fn(*mut c_void, c_ulong) -> c_uchar;
type XDisplayKeycodesFn = unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> c_int;
type XkbGetStateFn = unsafe extern "C" fn(*mut c_void, c_uint, *mut XkbState) -> c_int;
type XkbKeycodeToKeysymFn = unsafe extern "C" fn(*mut c_void, c_uint, c_int, c_int) -> c_ulong;
type XFlushFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type XTestFakeMotionEventFn =
    unsafe extern "C" fn(*mut c_void, c_int, c_int, c_int, c_ulong) -> c_int;
type XTestFakeButtonEventFn = unsafe extern "C" fn(*mut c_void, c_uint, c_int, c_ulong) -> c_int;
type XTestFakeKeyEventFn = unsafe extern "C" fn(*mut c_void, c_uint, c_int, c_ulong) -> c_int;
type XTestQueryExtensionFn =
    unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int, *mut c_int, *mut c_int) -> c_int;

#[derive(Default)]
#[repr(C)]
struct XkbState {
    group: u8,
    locked_group: u8,
    base_group: u16,
    latched_group: u16,
    mods: u8,
    base_mods: u8,
    latched_mods: u8,
    locked_mods: u8,
    compat_state: u8,
    grab_mods: u8,
    compat_grab_mods: u8,
    lookup_mods: u8,
    compat_lookup_mods: u8,
    ptr_buttons: u16,
}

struct X11Api {
    _x11: DynamicLibrary,
    _xtst: DynamicLibrary,
    open_display: XOpenDisplayFn,
    close_display: XCloseDisplayFn,
    default_root_window: Option<XDefaultRootWindowFn>,
    get_input_focus: Option<XGetInputFocusFn>,
    get_geometry: Option<XGetGeometryFn>,
    query_pointer: Option<XQueryPointerFn>,
    string_to_keysym: XStringToKeysymFn,
    keysym_to_keycode: XKeysymToKeycodeFn,
    display_keycodes: XDisplayKeycodesFn,
    xkb_get_state: Option<XkbGetStateFn>,
    xkb_keycode_to_keysym: Option<XkbKeycodeToKeysymFn>,
    flush: XFlushFn,
    fake_motion: XTestFakeMotionEventFn,
    fake_button: XTestFakeButtonEventFn,
    fake_key: XTestFakeKeyEventFn,
    query_extension: XTestQueryExtensionFn,
}

impl X11Api {
    fn load() -> Result<Self, String> {
        let x11 = DynamicLibrary::open(c"libX11.so.6")?;
        let xtst = DynamicLibrary::open(c"libXtst.so.6")?;
        Ok(Self {
            open_display: x11.symbol(c"XOpenDisplay")?,
            close_display: x11.symbol(c"XCloseDisplay")?,
            default_root_window: x11.symbol(c"XDefaultRootWindow").ok(),
            get_input_focus: x11.symbol(c"XGetInputFocus").ok(),
            get_geometry: x11.symbol(c"XGetGeometry").ok(),
            query_pointer: x11.symbol(c"XQueryPointer").ok(),
            string_to_keysym: x11.symbol(c"XStringToKeysym")?,
            keysym_to_keycode: x11.symbol(c"XKeysymToKeycode")?,
            display_keycodes: x11.symbol(c"XDisplayKeycodes")?,
            xkb_get_state: x11.symbol(c"XkbGetState").ok(),
            xkb_keycode_to_keysym: x11.symbol(c"XkbKeycodeToKeysym").ok(),
            flush: x11.symbol(c"XFlush")?,
            fake_motion: xtst.symbol(c"XTestFakeMotionEvent")?,
            fake_button: xtst.symbol(c"XTestFakeButtonEvent")?,
            fake_key: xtst.symbol(c"XTestFakeKeyEvent")?,
            query_extension: xtst.symbol(c"XTestQueryExtension")?,
            _x11: x11,
            _xtst: xtst,
        })
    }
}

struct XDisplay {
    ptr: *mut c_void,
    api: X11Api,
}
impl XDisplay {
    fn open() -> Result<Self, String> {
        let api = X11Api::load()?;
        let display = unsafe { (api.open_display)(std::ptr::null()) };
        if display.is_null() {
            Err("could not connect to the X11 display".into())
        } else {
            Ok(Self { ptr: display, api })
        }
    }
}
impl Drop for XDisplay {
    fn drop(&mut self) {
        unsafe {
            (self.api.close_display)(self.ptr);
        }
    }
}

pub(super) fn x11_test_available() -> bool {
    let Ok(display) = XDisplay::open() else {
        return false;
    };
    let (mut event_base, mut error_base, mut major, mut minor) = (0, 0, 0, 0);
    unsafe {
        (display.api.query_extension)(
            display.ptr,
            &mut event_base,
            &mut error_base,
            &mut major,
            &mut minor,
        ) != 0
    }
}

pub(super) fn x11_display_available() -> bool {
    let Ok(x11) = DynamicLibrary::open(c"libX11.so.6") else {
        return false;
    };
    let Ok(open_display) = x11.symbol::<XOpenDisplayFn>(c"XOpenDisplay") else {
        return false;
    };
    let Ok(close_display) = x11.symbol::<XCloseDisplayFn>(c"XCloseDisplay") else {
        return false;
    };
    let display = unsafe { open_display(std::ptr::null()) };
    if display.is_null() {
        false
    } else {
        unsafe {
            close_display(display);
        }
        true
    }
}

pub(super) fn x11_pointer_position() -> Option<(i32, i32)> {
    let display = XDisplay::open().ok()?;
    let root_window = display.api.default_root_window?;
    let query_pointer = display.api.query_pointer?;
    let root = unsafe { root_window(display.ptr) };
    let (
        mut root_return,
        mut child_return,
        mut root_x,
        mut root_y,
        mut window_x,
        mut window_y,
        mut mask,
    ) = (0, 0, 0, 0, 0, 0, 0);
    let found = unsafe {
        query_pointer(
            display.ptr,
            root,
            &mut root_return,
            &mut child_return,
            &mut root_x,
            &mut root_y,
            &mut window_x,
            &mut window_y,
            &mut mask,
        )
    };
    (found != 0).then_some((root_x, root_y))
}

pub(super) fn x11_screen_size() -> Option<(u32, u32)> {
    let display = XDisplay::open().ok()?;
    let root = unsafe { (display.api.default_root_window?)(display.ptr) };
    let mut root_return = 0;
    let (mut x, mut y, mut width, mut height, mut border, mut depth) = (0, 0, 0, 0, 0, 0);
    let ok = unsafe {
        (display.api.get_geometry?)(
            display.ptr,
            root,
            &mut root_return,
            &mut x,
            &mut y,
            &mut width,
            &mut height,
            &mut border,
            &mut depth,
        )
    };
    (ok != 0 && width > 0 && height > 0).then_some((width, height))
}

pub(super) fn x11_layout_text_available() -> bool {
    let Ok(display) = XDisplay::open() else {
        return false;
    };
    let Some(get_state) = display.api.xkb_get_state else {
        return false;
    };
    if display.api.xkb_keycode_to_keysym.is_none() {
        return false;
    }
    let mut state = XkbState::default();
    unsafe { get_state(display.ptr, 0x0100, &mut state) == 0 }
}

pub(super) fn x11_move(x: i32, y: i32) -> Result<(), String> {
    let display = XDisplay::open()?;
    let ok = unsafe { (display.api.fake_motion)(display.ptr, -1, x, y, 0) };
    unsafe {
        (display.api.flush)(display.ptr);
    }
    if ok == 0 {
        Err("XTEST pointer motion failed".into())
    } else {
        Ok(())
    }
}

pub(super) fn x11_click_at(x: i32, y: i32, button: u32) -> Result<(), String> {
    let display = XDisplay::open()?;
    if unsafe { (display.api.fake_motion)(display.ptr, -1, x, y, 0) } == 0 {
        unsafe {
            (display.api.flush)(display.ptr);
        }
        return Err("XTEST pointer motion failed".into());
    }
    let down = unsafe { (display.api.fake_button)(display.ptr, button, 1, 0) };
    let up = x11_release_button(&display, button);
    unsafe {
        (display.api.flush)(display.ptr);
    }
    match (down != 0, up) {
        (true, true) => Ok(()),
        (false, true) => Err("XTEST button press failed".into()),
        (true, false) => Err("XTEST button release failed".into()),
        (false, false) => Err("XTEST button press and release failed".into()),
    }
}

pub(super) fn x11_scroll_at(x: i32, y: i32, button: u32, amount: usize) -> Result<(), String> {
    let display = XDisplay::open()?;
    if unsafe { (display.api.fake_motion)(display.ptr, -1, x, y, 0) } == 0 {
        unsafe {
            (display.api.flush)(display.ptr);
        }
        return Err("XTEST pointer motion failed".into());
    }
    for _ in 0..amount {
        let down = unsafe { (display.api.fake_button)(display.ptr, button, 1, 0) } != 0;
        let up = x11_release_button(&display, button);
        if !down || !up {
            unsafe {
                (display.api.flush)(display.ptr);
            }
            return Err("XTEST scroll input failed".into());
        }
    }
    unsafe {
        (display.api.flush)(display.ptr);
    }
    Ok(())
}

pub(super) fn x11_drag_path(
    path: Vec<(u32, u32)>,
    button: u32,
    modifiers: Vec<String>,
) -> Result<(), String> {
    let display = XDisplay::open()?;
    let (x, y) = path[0];
    if unsafe { (display.api.fake_motion)(display.ptr, -1, x as i32, y as i32, 0) } == 0 {
        unsafe {
            (display.api.flush)(display.ptr);
        }
        return Err("XTEST pointer motion failed before drag".into());
    }
    let codes = modifiers
        .iter()
        .map(|modifier| keycode(&display, modifier_key(modifier)?))
        .collect::<Result<Vec<_>, _>>()?;
    let mut held_keys = Vec::new();
    for code in codes {
        let pressed = unsafe { (display.api.fake_key)(display.ptr, code as c_uint, 1, 0) } != 0;
        if !pressed {
            let current_released = x11_release_key(&display, code);
            let prior_released = held_keys
                .iter()
                .rev()
                .all(|held| x11_release_key(&display, *held));
            unsafe {
                (display.api.flush)(display.ptr);
            }
            return Err(if current_released && prior_released {
                "XTEST drag modifier press failed".into()
            } else {
                "XTEST drag modifier press failed and release was incomplete".into()
            });
        }
        held_keys.push(code);
    }
    if unsafe { (display.api.fake_button)(display.ptr, button, 1, 0) } == 0 {
        let released = x11_release_button(&display, button);
        let keys_released = held_keys
            .iter()
            .rev()
            .all(|held| x11_release_key(&display, *held));
        unsafe {
            (display.api.flush)(display.ptr);
        }
        return Err(if released && keys_released {
            "XTEST drag button press failed".into()
        } else {
            "XTEST drag button press or modifier release failed".into()
        });
    }
    unsafe {
        (display.api.flush)(display.ptr);
    }
    let mut movement_ok = true;
    for (x, y) in path.into_iter().skip(1) {
        std::thread::sleep(std::time::Duration::from_millis(8));
        if unsafe { (display.api.fake_motion)(display.ptr, -1, x as i32, y as i32, 0) } == 0 {
            movement_ok = false;
            break;
        }
        unsafe {
            (display.api.flush)(display.ptr);
        }
    }
    let released = x11_release_button(&display, button);
    let keys_released = held_keys
        .iter()
        .rev()
        .all(|held| x11_release_key(&display, *held));
    unsafe {
        (display.api.flush)(display.ptr);
    }
    match (movement_ok, released, keys_released) {
        (true, true, true) => Ok(()),
        (false, true, true) => Err("XTEST pointer motion failed during drag".into()),
        (true, false, true) => Err("XTEST drag button release failed".into()),
        (true, true, false) => Err("XTEST drag modifier release failed".into()),
        _ => Err("XTEST drag movement or input release failed".into()),
    }
}

fn x11_release_button(display: &XDisplay, button: u32) -> bool {
    let mut released = unsafe { (display.api.fake_button)(display.ptr, button, 0, 0) } != 0;
    if !released {
        released = unsafe { (display.api.fake_button)(display.ptr, button, 0, 0) } != 0;
    }
    released
}

pub(super) fn x11_button_state(button: u32, pressed: bool) -> Result<(), String> {
    let display = XDisplay::open()?;
    let result = unsafe { (display.api.fake_button)(display.ptr, button, i32::from(pressed), 0) };
    unsafe {
        (display.api.flush)(display.ptr);
    }
    if result == 0 {
        Err("XTEST button input failed".into())
    } else {
        Ok(())
    }
}

pub(super) fn x11_key_state(key: &str, pressed: bool) -> Result<(), String> {
    let display = XDisplay::open()?;
    let code = keycode(&display, key)?;
    if pressed {
        let result = (unsafe { (display.api.fake_key)(display.ptr, code as c_uint, 1, 0) }) != 0;
        if !result {
            let released = x11_release_key(&display, code);
            unsafe {
                (display.api.flush)(display.ptr);
            }
            return Err(if released {
                "XTEST key state press failed".into()
            } else {
                "XTEST key state press failed and release was incomplete".into()
            });
        }
        unsafe {
            (display.api.flush)(display.ptr);
        }
        return Ok(());
    } else {
        if !x11_release_key(&display, code) {
            return Err("XTEST key state release failed".into());
        }
    }
    unsafe {
        (display.api.flush)(display.ptr);
    }
    Ok(())
}

pub(super) fn x11_key(key: &str, modifiers: &[String]) -> Result<(), String> {
    let display = XDisplay::open()?;
    let mut codes = Vec::new();
    for modifier in modifiers {
        let name = match modifier.as_str() {
            "CTRL" => "Control_L",
            "SHIFT" => "Shift_L",
            "ALT" => "Alt_L",
            "SUPER" => "Super_L",
            _ => return Err("unsupported modifier".into()),
        };
        codes.push(keycode(&display, name)?);
    }
    let keycode = keycode(&display, key)?;
    let mut pressed = Vec::new();
    for code in &codes {
        if unsafe { (display.api.fake_key)(display.ptr, *code as c_uint, 1, 0) } == 0 {
            let failed_key_released = x11_release_key(&display, *code);
            let modifiers_released = x11_release_held(&display, &mut pressed);
            unsafe {
                (display.api.flush)(display.ptr);
            }
            return Err(if failed_key_released && modifiers_released {
                "XTEST modifier press failed".into()
            } else {
                "XTEST modifier press failed and key cleanup was incomplete".into()
            });
        }
        pressed.push(*code);
    }
    let key_down = unsafe { (display.api.fake_key)(display.ptr, keycode as c_uint, 1, 0) } != 0;
    let key_up = x11_release_key(&display, keycode);
    let mut modifiers_up = true;
    for code in pressed.iter().rev() {
        modifiers_up &= x11_release_key(&display, *code);
    }
    unsafe {
        (display.api.flush)(display.ptr);
    }
    match (key_down, key_up, modifiers_up) {
        (true, true, true) => Ok(()),
        (false, true, true) => Err("XTEST key press failed".into()),
        (true, false, true) => Err("XTEST key release failed".into()),
        (true, true, false) => Err("XTEST modifier release failed".into()),
        (false, false, _) => Err("XTEST key press failed and key release was incomplete".into()),
        (false, _, false) => {
            Err("XTEST key press failed and modifier cleanup was incomplete".into())
        }
        (true, false, false) => Err("XTEST key and modifier releases failed".into()),
    }
}

pub(super) fn x11_type_ascii(text: &str) -> Result<(), String> {
    let display = XDisplay::open()?;
    let shift = keycode(&display, "Shift_L")?;
    let mut held = Vec::new();
    let (mut minimum, mut maximum) = (0, 0);
    if unsafe { (display.api.display_keycodes)(display.ptr, &mut minimum, &mut maximum) } == 0 {
        return Err("could not read X11 keycode range".into());
    }
    let mut state = XkbState::default();
    let get_state = display
        .api
        .xkb_get_state
        .ok_or("X11 keyboard layout support is unavailable")?;
    let keycode_to_keysym = display
        .api
        .xkb_keycode_to_keysym
        .ok_or("X11 keyboard layout support is unavailable")?;
    if unsafe { get_state(display.ptr, 0x0100, &mut state) } != 0 {
        return Err("could not read the active X11 keyboard group".into());
    }
    const LOCK_MASK: u8 = 1 << 1;
    const TEXT_MODIFIER_MASK: u8 = (1 << 0) | (1 << 2) | (1 << 3) | (1 << 5) | (1 << 6) | (1 << 7);
    if state.mods & TEXT_MODIFIER_MASK != 0 {
        return Err(
            "release Shift, Ctrl, Alt, Super, and AltGr before typing with the X11 fallback".into(),
        );
    }
    let group = i32::from(state.group);
    for character in text.chars() {
        let keysym = match character {
            '\t' => 0xff09,
            '\n' | '\r' => 0xff0d,
            value if !value.is_control() => value as c_ulong,
            _ => {
                return Err(
                    "text input does not support control characters other than tab and newline"
                        .into(),
                );
            }
        };
        let mut selected = None;
        for code in minimum.max(8)..=maximum {
            for level in 0..=1 {
                if unsafe { keycode_to_keysym(display.ptr, code as c_uint, group, level) } == keysym
                {
                    selected = Some((code as c_uchar, level == 1));
                    break;
                }
            }
            if selected.is_some() {
                break;
            }
        }
        let (code, mut needs_shift) = selected.ok_or_else(|| {
            format!("character {character:?} is unavailable in the active X11 keyboard layout")
        })?;
        if character.is_ascii_alphabetic() && state.mods & LOCK_MASK != 0 {
            needs_shift = !needs_shift;
        }
        if needs_shift && !held.contains(&shift) {
            if unsafe { (display.api.fake_key)(display.ptr, shift as c_uint, 1, 0) } == 0 {
                let released =
                    x11_release_key(&display, shift) & x11_release_held(&display, &mut held);
                unsafe {
                    (display.api.flush)(display.ptr);
                }
                return Err(if released {
                    "XTEST shift press failed while typing".into()
                } else {
                    "XTEST shift press failed and key cleanup was incomplete".into()
                });
            }
            held.push(shift);
        }
        if unsafe { (display.api.fake_key)(display.ptr, code as c_uint, 1, 0) } == 0 {
            let released = x11_release_key(&display, code) & x11_release_held(&display, &mut held);
            unsafe {
                (display.api.flush)(display.ptr);
            }
            return Err(if released {
                "XTEST key press failed while typing".into()
            } else {
                "XTEST key press failed and key cleanup was incomplete".into()
            });
        }
        held.push(code);
        if !x11_release_key(&display, code) {
            let released = x11_release_held(&display, &mut held);
            unsafe {
                (display.api.flush)(display.ptr);
            }
            return Err(if released {
                "XTEST key release failed while typing".into()
            } else {
                "XTEST key release failed and cleanup was incomplete".into()
            });
        }
        held.pop();
        if needs_shift {
            if !x11_release_key(&display, shift) {
                let released = x11_release_held(&display, &mut held);
                unsafe {
                    (display.api.flush)(display.ptr);
                }
                return Err(if released {
                    "XTEST shift release failed while typing".into()
                } else {
                    "XTEST shift release failed and cleanup was incomplete".into()
                });
            }
            held.pop();
        }
    }
    unsafe {
        (display.api.flush)(display.ptr);
    }
    Ok(())
}

pub(super) fn xdotool_type(text: &str) -> Result<(), String> {
    let mut child = limited_command("30s", "xdotool")
        .args([
            "type",
            "--clearmodifiers",
            "--delay",
            "0",
            "--file",
            "/dev/stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|error| format!("starting xdotool: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("xdotool has no stdin")?;
    if let Err(error) = stdin.write_all(text.as_bytes()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("writing xdotool text input: {error}"));
    }
    drop(stdin);
    let status = child
        .wait()
        .map_err(|error| format!("waiting for xdotool: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("xdotool text input failed with status {status}"))
    }
}

fn x11_release_key(display: &XDisplay, code: c_uchar) -> bool {
    let mut released = unsafe { (display.api.fake_key)(display.ptr, code as c_uint, 0, 0) } != 0;
    if !released {
        released = unsafe { (display.api.fake_key)(display.ptr, code as c_uint, 0, 0) } != 0;
    }
    released
}

fn x11_release_held(display: &XDisplay, held: &mut Vec<c_uchar>) -> bool {
    let mut released = true;
    while let Some(code) = held.pop() {
        released &= x11_release_key(display, code);
    }
    released
}

fn keycode(display: &XDisplay, key: &str) -> Result<c_uchar, String> {
    let name = CString::new(key).map_err(|_| "invalid keysym")?;
    let sym = unsafe { (display.api.string_to_keysym)(name.as_ptr()) };
    if sym == 0 {
        return Err(format!("unknown X11 keysym: {key}"));
    }
    let code = unsafe { (display.api.keysym_to_keycode)(display.ptr, sym) };
    if code == 0 {
        return Err(format!("keysym has no keycode in the active layout: {key}"));
    }
    Ok(code)
}

pub(super) fn ffmpeg_x11grab_available() -> bool {
    ["demuxer=x11grab", "encoder=mjpeg"]
        .into_iter()
        .all(|component| {
            limited_command("3s", "ffmpeg")
                .args(["-hide_banner", "-h", component])
                .output()
                .is_ok_and(|output| output.status.success())
        })
}

pub(super) fn x11_image(
    quality: u8,
    cursor_capture: bool,
    screen: Option<(u32, u32)>,
    max_dimension: u32,
) -> Result<Vec<u8>, String> {
    if cursor_capture {
        match ffmpeg_x11_image(quality, screen.is_some(), max_dimension) {
            Ok(image) => return Ok(image),
            Err(error) if error.contains("exceeds") => return Err(error),
            Err(error) if !command_exists("import") => return Err(error),
            Err(_) => {}
        }
    }
    let mut command = limited_command("4s", "import");
    let quality = quality.to_string();
    command.args(["-window", "root"]);
    if screen.is_some() {
        command.args(["-resize", &format!("{max_dimension}x{max_dimension}>")]);
    }
    command.args(["-quality", &quality, "jpg:-"]);
    let (image, status) = bounded_command_output(&mut command, MAX_IMAGE)
        .map_err(|error| format!("X11 capture failed: {error}"))?;
    if status.success() {
        Ok(image)
    } else {
        Err(format!("ImageMagick import failed with status {status}"))
    }
}

fn ffmpeg_x11_image(quality: u8, scale: bool, max_dimension: u32) -> Result<Vec<u8>, String> {
    let display = std::env::var("DISPLAY").map_err(|_| "DISPLAY is unavailable")?;
    let quantizer = match quality {
        85 => "3",
        60 => "10",
        _ => "20",
    };
    let mut command = limited_command("4s", "ffmpeg");
    command.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "x11grab",
        "-draw_mouse",
        "1",
        "-framerate",
        "30",
        "-i",
        &display,
    ]);
    if scale {
        command.args(["-vf", &format!("scale=w='min({max_dimension},iw)':h='min({max_dimension},ih)':force_original_aspect_ratio=decrease")]);
    }
    command
        .args([
            "-frames:v",
            "1",
            "-an",
            "-c:v",
            "mjpeg",
            "-q:v",
            quantizer,
            "-f",
            "image2pipe",
            "pipe:1",
        ])
        .stderr(Stdio::null());
    let (image, status) = bounded_command_output(&mut command, MAX_IMAGE)
        .map_err(|error| format!("FFmpeg X11 capture failed: {error}"))?;
    if status.success() {
        Ok(image)
    } else {
        Err(format!("FFmpeg X11 capture failed with status {status}"))
    }
}
