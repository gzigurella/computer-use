use std::{
    collections::{HashMap, HashSet},
    ffi::{CStr, CString, c_char, c_int, c_void},
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::fd::{AsFd, AsRawFd, FromRawFd},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    protocol::{wl_keyboard, wl_registry, wl_seat},
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

const MAX_KEYMAP_KEYS: usize = 240;
const MAX_LINUX_KEYCODE: u32 = 0x2ff;
const MAX_TEXT_BYTES: usize = 65_536;
const INPUT_KEYS: &[(&str, &str, u32, &str)] = &[
    ("ESCAPE", "ESC", 1, "Escape"),
    ("1", "AE01", 2, "1"),
    ("2", "AE02", 3, "2"),
    ("3", "AE03", 4, "3"),
    ("4", "AE04", 5, "4"),
    ("5", "AE05", 6, "5"),
    ("6", "AE06", 7, "6"),
    ("7", "AE07", 8, "7"),
    ("8", "AE08", 9, "8"),
    ("9", "AE09", 10, "9"),
    ("0", "AE10", 11, "0"),
    ("MINUS", "AE11", 12, "minus"),
    ("EQUAL", "AE12", 13, "equal"),
    ("BACKSPACE", "BKSP", 14, "BackSpace"),
    ("TAB", "TAB", 15, "Tab"),
    ("Q", "AD01", 16, "q"),
    ("W", "AD02", 17, "w"),
    ("E", "AD03", 18, "e"),
    ("R", "AD04", 19, "r"),
    ("T", "AD05", 20, "t"),
    ("Y", "AD06", 21, "y"),
    ("U", "AD07", 22, "u"),
    ("I", "AD08", 23, "i"),
    ("O", "AD09", 24, "o"),
    ("P", "AD10", 25, "p"),
    ("BRACKETLEFT", "AD11", 26, "bracketleft"),
    ("BRACKETRIGHT", "AD12", 27, "bracketright"),
    ("ENTER", "RTRN", 28, "Return"),
    ("A", "AC01", 30, "a"),
    ("S", "AC02", 31, "s"),
    ("D", "AC03", 32, "d"),
    ("F", "AC04", 33, "f"),
    ("G", "AC05", 34, "g"),
    ("H", "AC06", 35, "h"),
    ("J", "AC07", 36, "j"),
    ("K", "AC08", 37, "k"),
    ("L", "AC09", 38, "l"),
    ("SEMICOLON", "AC10", 39, "semicolon"),
    ("APOSTROPHE", "AC11", 40, "apostrophe"),
    ("GRAVE", "TLDE", 41, "grave"),
    ("BACKSLASH", "BKSL", 43, "backslash"),
    ("Z", "AB01", 44, "z"),
    ("X", "AB02", 45, "x"),
    ("C", "AB03", 46, "c"),
    ("V", "AB04", 47, "v"),
    ("B", "AB05", 48, "b"),
    ("N", "AB06", 49, "n"),
    ("M", "AB07", 50, "m"),
    ("COMMA", "AB08", 51, "comma"),
    ("PERIOD", "AB09", 52, "period"),
    ("SLASH", "AB10", 53, "slash"),
    ("SPACE", "SPCE", 57, "space"),
    ("F1", "FK01", 59, "F1"),
    ("F2", "FK02", 60, "F2"),
    ("F3", "FK03", 61, "F3"),
    ("F4", "FK04", 62, "F4"),
    ("F5", "FK05", 63, "F5"),
    ("F6", "FK06", 64, "F6"),
    ("F7", "FK07", 65, "F7"),
    ("F8", "FK08", 66, "F8"),
    ("F9", "FK09", 67, "F9"),
    ("F10", "FK10", 68, "F10"),
    ("F11", "FK11", 87, "F11"),
    ("F12", "FK12", 88, "F12"),
    ("CAPS_LOCK", "CAPS", 58, "Caps_Lock"),
    ("NUM_LOCK", "NMLK", 69, "Num_Lock"),
    ("SCROLL_LOCK", "SCLK", 70, "Scroll_Lock"),
    ("PRINT", "PRSC", 99, "Print"),
    ("PAUSE", "PAUS", 119, "Pause"),
    ("F13", "FK13", 183, "F13"),
    ("F14", "FK14", 184, "F14"),
    ("F15", "FK15", 185, "F15"),
    ("F16", "FK16", 186, "F16"),
    ("F17", "FK17", 187, "F17"),
    ("F18", "FK18", 188, "F18"),
    ("F19", "FK19", 189, "F19"),
    ("F20", "FK20", 190, "F20"),
    ("F21", "FK21", 191, "F21"),
    ("F22", "FK22", 192, "F22"),
    ("F23", "FK23", 193, "F23"),
    ("F24", "FK24", 194, "F24"),
    ("F25", "FK25", 195, "F25"),
    ("F26", "FK26", 196, "F26"),
    ("F27", "FK27", 197, "F27"),
    ("F28", "FK28", 198, "F28"),
    ("F29", "FK29", 199, "F29"),
    ("F30", "FK30", 200, "F30"),
    ("F31", "FK31", 201, "F31"),
    ("F32", "FK32", 202, "F32"),
    ("F33", "FK33", 203, "F33"),
    ("F34", "FK34", 204, "F34"),
    ("F35", "FK35", 205, "F35"),
    ("BREAK", "BRK_", 206, "Break"),
    ("KP_SPACE", "NKSC", 207, "KP_Space"),
    ("KP_BEGIN", "NKBG", 208, "KP_Begin"),
    ("KP_HOME", "NKHM", 231, "KP_Home"),
    ("KP_LEFT", "NKLF", 232, "KP_Left"),
    ("KP_UP", "NKUP", 233, "KP_Up"),
    ("KP_RIGHT", "NKRG", 234, "KP_Right"),
    ("KP_DOWN", "NKDN", 235, "KP_Down"),
    ("KP_PAGE_UP", "NKPU", 236, "KP_Page_Up"),
    ("KP_PAGE_DOWN", "NKPD", 237, "KP_Page_Down"),
    ("KP_END", "NKEN", 238, "KP_End"),
    ("KP_INSERT", "NKIN", 239, "KP_Insert"),
    ("KP_DELETE", "NKDE", 240, "KP_Delete"),
    ("KP_MULTIPLY", "NKMX", 55, "KP_Multiply"),
    ("KP_7", "NK07", 71, "KP_7"),
    ("KP_8", "NK08", 72, "KP_8"),
    ("KP_9", "NK09", 73, "KP_9"),
    ("KP_SUBTRACT", "NKMN", 74, "KP_Subtract"),
    ("KP_4", "NK04", 75, "KP_4"),
    ("KP_5", "NK05", 76, "KP_5"),
    ("KP_6", "NK06", 77, "KP_6"),
    ("KP_ADD", "NKPL", 78, "KP_Add"),
    ("KP_1", "NK01", 79, "KP_1"),
    ("KP_2", "NK02", 80, "KP_2"),
    ("KP_3", "NK03", 81, "KP_3"),
    ("KP_0", "NK00", 82, "KP_0"),
    ("KP_DECIMAL", "NKDT", 83, "KP_Decimal"),
    ("KP_SEPARATOR", "NKSP", 95, "KP_Separator"),
    ("KP_ENTER", "NKER", 96, "KP_Enter"),
    ("KP_DIVIDE", "NKDV", 98, "KP_Divide"),
    ("KP_EQUAL", "NKEQ", 117, "KP_Equal"),
    ("KP_COMMA", "NKCM", 121, "KP_Separator"),
    ("XF86CALCULATOR", "XCAL", 140, "XF86Calculator"),
    ("XF86SLEEP", "XSLE", 142, "XF86Sleep"),
    ("XF86WAKEUP", "XWAK", 143, "XF86WakeUp"),
    ("XF86MAIL", "XMAI", 155, "XF86Mail"),
    ("XF86BACK", "XBAK", 158, "XF86Back"),
    ("XF86FORWARD", "XFWD", 159, "XF86Forward"),
    ("XF86AUDIONEXT", "XANX", 163, "XF86AudioNext"),
    ("XF86AUDIOPLAY", "XAPL", 164, "XF86AudioPlay"),
    ("XF86AUDIOPREV", "XAPV", 165, "XF86AudioPrev"),
    ("XF86AUDIOSTOP", "XAST", 166, "XF86AudioStop"),
    ("XF86HOMEPAGE", "XHOM", 172, "XF86HomePage"),
    ("XF86SEARCH", "XSEA", 217, "XF86Search"),
    ("XF86BRIGHTNESSDOWN", "XBDN", 224, "XF86MonBrightnessDown"),
    ("XF86BRIGHTNESSUP", "XBUP", 225, "XF86MonBrightnessUp"),
    ("XF86AUDIOMUTE", "XAMU", 113, "XF86AudioMute"),
    ("XF86VOLUMEDOWN", "XVDN", 114, "XF86AudioLowerVolume"),
    ("XF86VOLUMEUP", "XVUP", 115, "XF86AudioRaiseVolume"),
    ("XF86MICMUTE", "XMMU", 246, "XF86AudioMicMute"),
    ("HOME", "HOME", 102, "Home"),
    ("UP", "UP", 103, "Up"),
    ("PAGE_UP", "PGUP", 104, "Page_Up"),
    ("LEFT", "LEFT", 105, "Left"),
    ("RIGHT", "RGHT", 106, "Right"),
    ("END", "END", 107, "End"),
    ("DOWN", "DOWN", 108, "Down"),
    ("PAGE_DOWN", "PGDN", 109, "Page_Down"),
    ("INSERT", "INS", 110, "Insert"),
    ("DELETE", "DELE", 111, "Delete"),
];

#[link(name = "c")]
unsafe extern "C" {
    fn memfd_create(name: *const std::ffi::c_char, flags: std::ffi::c_int) -> std::ffi::c_int;
    fn free(pointer: *mut c_void);
}

#[repr(C)]
struct XkbRuleNames {
    rules: *const c_char,
    model: *const c_char,
    layout: *const c_char,
    variant: *const c_char,
    options: *const c_char,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct XkbSettings {
    layout: String,
    group_index: u32,
    active_keymap: Option<String>,
    variant: Option<String>,
    rules: Option<String>,
    model: Option<String>,
    options: Option<String>,
}

#[derive(Clone)]
struct KeyMapping {
    keycode: u32,
    modifiers: Vec<&'static str>,
}

#[derive(Default)]
struct State {
    manager: Option<ZwpVirtualKeyboardManagerV1>,
    seat: Option<wl_seat::WlSeat>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "zwp_virtual_keyboard_manager_v1" if state.manager.is_none() => {
                    state.manager = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(1), qh, ()));
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardV1);

#[derive(Default)]
struct KeymapState {
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keymap: Option<String>,
    error: Option<String>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for KeymapState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
            && interface == "wl_seat"
            && state.seat.is_none()
        {
            state.seat = Some(registry.bind(name, version.min(1), qh, ()));
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for KeymapState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event {
            if !matches!(
                format,
                wayland_client::WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)
            ) {
                state.error =
                    Some("Wayland seat supplied an unsupported keyboard keymap format".into());
                return;
            }
            if size == 0 || size > 16 * 1024 * 1024 {
                state.error = Some(format!("Wayland keyboard keymap size {size} is invalid"));
                return;
            }
            let read = || -> Result<String, String> {
                let mut file = File::from(fd);
                file.seek(SeekFrom::Start(0))
                    .map_err(|error| format!("seeking Wayland keyboard keymap: {error}"))?;
                let mut bytes = vec![0; size as usize];
                file.read_exact(&mut bytes)
                    .map_err(|error| format!("reading Wayland keyboard keymap: {error}"))?;
                if bytes.last() != Some(&0) {
                    return Err("Wayland keyboard keymap is not NUL-terminated".into());
                }
                bytes.pop();
                let keymap = String::from_utf8(bytes)
                    .map_err(|error| format!("Wayland keyboard keymap is not UTF-8: {error}"))?;
                if !keymap.starts_with("xkb_keymap") || keymap.contains('\0') {
                    return Err("Wayland seat supplied a malformed XKB keymap".into());
                }
                Ok(keymap)
            }();
            match read {
                Ok(keymap) => state.keymap = Some(keymap),
                Err(error) => state.error = Some(error),
            }
        }
    }
}

delegate_noop!(KeymapState: ignore wl_seat::WlSeat);

fn current_wayland_keymap() -> Result<String, String> {
    let connection = Connection::connect_to_env().map_err(|error| {
        format!("connecting to Wayland for the active keyboard keymap: {error}")
    })?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    let mut state = KeymapState::default();
    queue
        .roundtrip(&mut state)
        .map_err(|error| format!("discovering the Wayland keyboard seat: {error}"))?;
    let seat = state.seat.as_ref().ok_or("Wayland has no keyboard seat")?;
    state.keyboard = Some(seat.get_keyboard(&qh, ()));
    queue
        .roundtrip(&mut state)
        .map_err(|error| format!("requesting the active Wayland keyboard keymap: {error}"))?;
    if let Some(error) = state.error {
        return Err(error);
    }
    state
        .keymap
        .ok_or_else(|| "Wayland seat did not provide its active keyboard keymap".into())
}

fn xkbcommon_available() -> bool {
    crate::DynamicLibrary::open(c"libxkbcommon.so.0").is_ok()
}

pub fn available() -> bool {
    discover().is_ok()
}

pub struct VirtualKeyboard {
    _connection: Connection,
    queue: EventQueue<State>,
    state: State,
    keyboard: ZwpVirtualKeyboardV1,
    started: Instant,
    last_event_time: u32,
    pressed: HashSet<u32>,
    locked_modifiers: u32,
    keycodes: HashMap<String, KeyMapping>,
    modifier_masks: HashMap<u32, u32>,
    keymap_layout: String,
    keymap_source: &'static str,
    xkb_settings: Option<XkbSettings>,
}

unsafe impl Send for VirtualKeyboard {}

impl VirtualKeyboard {
    pub fn connect() -> Result<Self, String> {
        let xkb_settings = current_hyprland_xkb_settings().ok();
        let dynamic_keymap = if xkbcommon_available() {
            let compositor_keymap = current_wayland_keymap().ok();
            xkb_settings.as_ref().and_then(|settings| {
                compile_xkb_keymap_with_source(settings, compositor_keymap.as_deref())
                    .or_else(|_| compile_xkb_keymap_with_source(settings, None))
                    .ok()
                    .map(|(text, keycodes, modifier_masks)| {
                        (
                            text,
                            keycodes,
                            modifier_masks,
                            settings.layout.clone(),
                            "hyprland-xkb",
                        )
                    })
            })
        } else {
            None
        }
        .or_else(|| {
            xkb_settings.as_ref().and_then(|settings| {
                compile_rust_xkb_keymap(settings)
                    .ok()
                    .map(|(text, keycodes, modifier_masks)| {
                        (
                            text,
                            keycodes,
                            modifier_masks,
                            settings.layout.clone(),
                            "hyprland-rust-xkb",
                        )
                    })
            })
        });
        let (keymap_text, keycodes, modifier_masks, keymap_layout, keymap_source) =
            match dynamic_keymap {
                Some((keymap_text, keycodes, modifier_masks, layout, source)) => {
                    (keymap_text, keycodes, modifier_masks, layout, source)
                }
                None => (
                    modifier_keymap(),
                    embedded_keycodes(),
                    embedded_modifier_masks(),
                    "us".into(),
                    "embedded-us-fallback",
                ),
            };
        let (connection, mut queue, mut state) = discover()?;
        let qh = queue.handle();
        let seat = state
            .seat
            .as_ref()
            .ok_or("Wayland has no input seat")?
            .clone();
        let manager = state
            .manager
            .as_ref()
            .ok_or("compositor has no virtual keyboard protocol")?
            .clone();
        let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
        let keymap = create_keymap_file(keymap_text.as_bytes())?;
        keyboard.keymap(
            wl_keyboard::KeymapFormat::XkbV1.into(),
            keymap.as_fd(),
            u32::try_from(
                keymap
                    .metadata()
                    .map_err(|error| format!("reading virtual keyboard keymap: {error}"))?
                    .len(),
            )
            .map_err(|_| "virtual keyboard keymap is too large")?,
        );
        queue
            .roundtrip(&mut state)
            .map_err(|error| format!("synchronizing virtual keyboard keymap: {error}"))?;
        Ok(Self {
            _connection: connection,
            queue,
            state,
            keyboard,
            started: Instant::now(),
            last_event_time: 0,
            pressed: HashSet::new(),
            locked_modifiers: 0,
            keycodes,
            modifier_masks,
            keymap_layout,
            keymap_source,
            xkb_settings,
        })
    }

    pub fn key_state(&mut self, key: &str, pressed: bool) -> Result<(), String> {
        let keycode = key_mapping(key, &self.keycodes)?.keycode;
        self.last_event_time = self
            .last_event_time
            .max(self.started.elapsed().as_millis() as u32)
            .saturating_add(1);
        if pressed {
            self.pressed.insert(keycode);
            self.locked_modifiers ^= match keycode {
                58 => 1 << 1,
                69 => 1 << 4,
                70 => 1 << 5,
                _ => 0,
            };
        } else {
            self.pressed.remove(&keycode);
        }
        self.keyboard.key(
            self.last_event_time,
            keycode,
            if pressed {
                wl_keyboard::KeyState::Pressed
            } else {
                wl_keyboard::KeyState::Released
            }
            .into(),
        );
        let depressed = self
            .pressed
            .iter()
            .filter_map(|code| self.modifier_masks.get(code))
            .fold(0, |mask, modifier| mask | modifier);
        self.keyboard
            .modifiers(depressed, 0, self.locked_modifiers, 0);
        self.queue
            .roundtrip(&mut self.state)
            .map_err(|error| format!("sending virtual keyboard modifier: {error}"))?;
        Ok(())
    }

    pub fn keymap_layout(&self) -> &str {
        &self.keymap_layout
    }

    pub fn keymap_source(&self) -> &'static str {
        self.keymap_source
    }

    pub fn refresh_keymap(&mut self) -> Result<Option<(String, &'static str)>, String> {
        let settings = current_hyprland_xkb_settings()
            .map_err(|error| format!("could not check the active Hyprland XKB layout: {error}"))?;
        if self.xkb_settings.as_ref() == Some(&settings) {
            return Ok(None);
        }
        if !self.pressed.is_empty() {
            return Err("cannot change the virtual keyboard layout while keys are held".into());
        }
        let compositor_keymap = current_wayland_keymap().ok();
        let compiled = if xkbcommon_available() {
            compile_xkb_keymap_with_source(&settings, compositor_keymap.as_deref())
                .or_else(|_| compile_xkb_keymap_with_source(&settings, None))
                .map(|(text, keys, masks)| (text, keys, masks, "hyprland-xkb"))
        } else {
            Err("libxkbcommon is unavailable".into())
        }
        .or_else(|_| {
            compile_rust_xkb_keymap(&settings)
                .map(|(text, keys, masks)| (text, keys, masks, "hyprland-rust-xkb"))
        });
        let (keymap_text, keycodes, modifier_masks, source, layout) = match compiled {
            Ok((text, keys, masks, source)) => (text, keys, masks, source, settings.layout.clone()),
            Err(_) => (
                modifier_keymap(),
                embedded_keycodes(),
                embedded_modifier_masks(),
                "embedded-us-fallback",
                "us".into(),
            ),
        };
        let keymap = create_keymap_file(keymap_text.as_bytes())?;
        self.keyboard.keymap(
            wl_keyboard::KeymapFormat::XkbV1.into(),
            keymap.as_fd(),
            u32::try_from(
                keymap
                    .metadata()
                    .map_err(|error| format!("reading virtual keyboard keymap: {error}"))?
                    .len(),
            )
            .map_err(|_| "virtual keyboard keymap is too large")?,
        );
        self.queue
            .roundtrip(&mut self.state)
            .map_err(|error| format!("synchronizing changed virtual keyboard keymap: {error}"))?;
        self.keycodes = keycodes;
        self.modifier_masks = modifier_masks;
        self.keymap_layout = layout;
        self.keymap_source = source;
        self.xkb_settings = Some(settings.clone());
        Ok(Some((self.keymap_layout.clone(), source)))
    }

    pub fn required_modifiers(&self, key: &str) -> Result<Vec<&'static str>, String> {
        Ok(key_mapping(key, &self.keycodes)?.modifiers)
    }
}

impl Drop for VirtualKeyboard {
    fn drop(&mut self) {
        for keycode in std::mem::take(&mut self.pressed) {
            self.last_event_time = self
                .last_event_time
                .max(self.started.elapsed().as_millis() as u32)
                .saturating_add(1);
            self.keyboard.key(
                self.last_event_time,
                keycode,
                wl_keyboard::KeyState::Released.into(),
            );
        }
        self.keyboard.destroy();
        let _ = self.queue.roundtrip(&mut self.state);
    }
}

fn keycode_for(key: &str) -> Result<u32, String> {
    match key {
        "Control_L" => Ok(29),
        "Control_R" => Ok(97),
        "Shift_L" => Ok(42),
        "Shift_R" => Ok(54),
        "Alt_L" => Ok(56),
        "Alt_R" => Ok(100),
        "Super_L" => Ok(125),
        "Super_R" => Ok(126),
        _ => {
            let key = match key {
                "Return" => "ENTER",
                "BackSpace" => "BACKSPACE",
                "Page_Up" => "PAGE_UP",
                "Page_Down" => "PAGE_DOWN",
                "Caps_Lock" => "CAPS_LOCK",
                "Num_Lock" => "NUM_LOCK",
                "Scroll_Lock" => "SCROLL_LOCK",
                "Break" => "BREAK",
                other => other,
            };
            INPUT_KEYS
                .iter()
                .find(|(name, _, _, _)| *name == key.to_ascii_uppercase())
                .map(|(_, _, evdev, _)| *evdev)
                .ok_or_else(|| format!("virtual keyboard key {key:?} is unsupported"))
        }
    }
}

fn key_mapping(key: &str, keycodes: &HashMap<String, KeyMapping>) -> Result<KeyMapping, String> {
    match key {
        "Control_L" | "Control_R" | "Shift_L" | "Shift_R" | "Alt_L" | "Alt_R" | "Super_L"
        | "Super_R" => Ok(KeyMapping {
            keycode: keycode_for(key)?,
            modifiers: Vec::new(),
        }),
        _ => {
            let name = match key {
                "Return" => "ENTER",
                "BackSpace" => "BACKSPACE",
                "Page_Up" => "PAGE_UP",
                "Page_Down" => "PAGE_DOWN",
                "Caps_Lock" => "CAPS_LOCK",
                "Num_Lock" => "NUM_LOCK",
                "Scroll_Lock" => "SCROLL_LOCK",
                "Break" => "BREAK",
                other => other,
            };
            keycodes
                .get(&name.to_ascii_uppercase())
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "virtual keyboard key {key:?} cannot be resolved in the active XKB layout"
                    )
                })
        }
    }
}

fn embedded_keycodes() -> HashMap<String, KeyMapping> {
    INPUT_KEYS
        .iter()
        .map(|(name, _, evdev, _)| {
            (
                (*name).to_owned(),
                KeyMapping {
                    keycode: *evdev,
                    modifiers: Vec::new(),
                },
            )
        })
        .collect()
}

fn embedded_modifier_masks() -> HashMap<u32, u32> {
    [
        (29, 1 << 2),
        (97, 1 << 2),
        (42, 1),
        (54, 1),
        (56, 1 << 3),
        (100, 1 << 3),
        (125, 1 << 6),
        (126, 1 << 6),
    ]
    .into()
}

fn current_hyprland_xkb_settings() -> Result<XkbSettings, String> {
    let option = |name: &str| -> Option<String> {
        let response = crate::hyprland_json(
            &format!("getoption input:kb_{name}"),
            64 * 1024,
            Duration::from_secs(2),
        )
        .ok()?;
        let value: serde_json::Value = serde_json::from_slice(&response).ok()?;
        let value = value.get("str")?.as_str()?;
        (!value.is_empty() && value != "[[EMPTY]]").then(|| value.to_owned())
    };
    let devices = crate::hyprland_json("devices", 1024 * 1024, Duration::from_secs(2))
        .map_err(|error| format!("reading active Hyprland keyboard layout: {error}"))?;
    let devices: serde_json::Value = serde_json::from_slice(&devices)
        .map_err(|error| format!("parsing active Hyprland keyboard layout: {error}"))?;
    let keyboard = devices["keyboards"].as_array().and_then(|keyboards| {
        keyboards.iter().find(|keyboard| {
            keyboard["name"]
                .as_str()
                .is_some_and(|name| !name.starts_with("hl-virtual-keyboard"))
        })
    });
    let device_option = |name: &str| {
        keyboard
            .and_then(|keyboard| keyboard[name].as_str())
            .filter(|value| *value != "[[EMPTY]]")
            .map(str::to_owned)
    };
    let layout_index = keyboard
        .and_then(|keyboard| keyboard["active_layout_index"].as_u64())
        .unwrap_or(0) as usize;
    let configured_layouts = device_option("layout")
        .or_else(|| option("layout"))
        .unwrap_or_else(|| "us".into());
    let layouts = configured_layouts.split(',').collect::<Vec<_>>();
    let index = layout_index.min(layouts.len().saturating_sub(1));
    let layout = layouts
        .get(index)
        .copied()
        .filter(|layout| !layout.is_empty())
        .unwrap_or("us");
    let variant = device_option("variant")
        .or_else(|| option("variant"))
        .and_then(|variants| variants.split(',').nth(index).map(str::to_owned));
    let rules = device_option("rules").or_else(|| option("rules"));
    let model = device_option("model").or_else(|| option("model"));
    let options = device_option("options").or_else(|| option("options"));
    let active_keymap = device_option("active_keymap");
    Ok(XkbSettings {
        layout: layout.to_owned(),
        group_index: index as u32,
        active_keymap,
        variant,
        rules,
        model,
        options,
    })
}

fn normalize_keymap_name(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn compile_rust_xkb_keymap(
    settings: &XkbSettings,
) -> Result<(String, HashMap<String, KeyMapping>, HashMap<u32, u32>), String> {
    if settings
        .rules
        .as_deref()
        .is_some_and(|rules| !rules.is_empty() && rules != "evdev")
        || settings
            .model
            .as_deref()
            .is_some_and(|model| !model.is_empty() && model != "pc105")
    {
        return Err(
            "Rust XKB fallback only supports the standard evdev/pc105 rules and model".into(),
        );
    }
    let mut keymap = wkb::WKB::new_from_names(
        "evdev",
        "pc105",
        &settings.layout,
        settings.variant.as_deref().unwrap_or(""),
        settings.options.as_deref(),
    )
    .map_err(|error| format!("parsing XKB data for layout {:?}: {error}", settings.layout))?;
    let layout_name = keymap
        .layout_name(0)
        .ok_or("Rust XKB fallback produced no active layout")?;
    if settings
        .active_keymap
        .as_deref()
        .is_some_and(|active| normalize_keymap_name(active) != normalize_keymap_name(layout_name))
    {
        return Err(format!(
            "local XKB layout {layout_name:?} does not match Hyprland's active map {:?}",
            settings.active_keymap
        ));
    }

    let mut keycodes = HashMap::new();
    let mut missing = Vec::new();
    for (name, _, _, symbol) in INPUT_KEYS {
        let found = if let Some(target) = character_for_xkb_symbol(symbol) {
            [(0, 0), (1, 1), (128, 2), (129, 3)]
                .into_iter()
                .find_map(|(mask, level)| {
                    (0..=MAX_LINUX_KEYCODE)
                        .find(|code| keymap.level_char(*code, 0, level) == Some(target))
                        .map(|code| KeyMapping {
                            keycode: code,
                            modifiers: match mask {
                                0 => Vec::new(),
                                1 => vec!["SHIFT"],
                                128 => vec!["ALTGR"],
                                _ => vec!["SHIFT", "ALTGR"],
                            },
                        })
                })
        } else if let Some(target) = named_key_for_xkb_symbol(symbol) {
            keymap.update_modifiers(0, 0, 0, 0);
            (0..=MAX_LINUX_KEYCODE)
                .find(|code| keymap.named_key(*code) == target)
                .map(|keycode| KeyMapping {
                    keycode,
                    modifiers: Vec::new(),
                })
        } else {
            None
        };
        if let Some(mapping) = found {
            keycodes.insert((*name).to_owned(), mapping);
        } else {
            missing.push((*name, *symbol));
        }
    }

    let mut keymap_text = normalize_legacy_xkb_keysyms(&keymap.as_xkb_string());
    let mut synthetic = Vec::new();
    let mut next_keycode = xkb_keymap_maximum(&keymap_text)?.saturating_add(1);
    for (name, symbol) in missing {
        let Some(evdev_code) = next_keycode
            .checked_sub(8)
            .filter(|code| *code <= MAX_LINUX_KEYCODE)
        else {
            break;
        };
        synthetic.push((next_keycode, symbol.to_owned()));
        keycodes.insert(
            name.to_owned(),
            KeyMapping {
                keycode: evdev_code,
                modifiers: Vec::new(),
            },
        );
        next_keycode = next_keycode.saturating_add(1);
    }
    keymap_text = extend_xkb_keymap_with_symbols_in_group(&keymap_text, &synthetic, 0)?;
    if wkb::WKB::new_from_string(&keymap_text).is_err() {
        return Err("validating the Rust-generated XKB keymap failed".into());
    }

    let mut modifier_masks = HashMap::new();
    for keycode in 0..=MAX_LINUX_KEYCODE {
        keymap.update_modifiers(0, 0, 0, 0);
        keymap.press_key(keycode);
        let mask = keymap.raw_modifiers().depressed;
        keymap.release_key(keycode);
        if mask != 0 {
            modifier_masks.insert(keycode, mask);
        }
        if mask & 128 != 0 {
            keycodes.entry("ALTGR".into()).or_insert(KeyMapping {
                keycode,
                modifiers: Vec::new(),
            });
        }
    }
    // WKB does not assign Mod1/Mod4 state to the standard Alt/Super positions.
    modifier_masks.extend(embedded_modifier_masks());
    if keycodes.len() < INPUT_KEYS.len() {
        return Err(format!(
            "Rust XKB fallback resolved only {} of {} input keys",
            keycodes.len(),
            INPUT_KEYS.len()
        ));
    }
    Ok((keymap_text, keycodes, modifier_masks))
}

fn character_for_xkb_symbol(symbol: &str) -> Option<char> {
    let character = match symbol {
        "minus" => '-',
        "equal" => '=',
        "bracketleft" => '[',
        "bracketright" => ']',
        "semicolon" => ';',
        "apostrophe" => '\'',
        "grave" => '`',
        "backslash" => '\\',
        "comma" => ',',
        "period" => '.',
        "slash" => '/',
        "space" => ' ',
        _ if symbol.chars().count() == 1 => symbol.chars().next()?,
        _ => return None,
    };
    Some(character)
}

fn named_key_for_xkb_symbol(symbol: &str) -> Option<wkb::NamedKey> {
    use wkb::NamedKey as K;
    Some(match symbol {
        "Escape" => K::Escape,
        "BackSpace" => K::Backspace,
        "Return" => K::Enter,
        "Tab" => K::Tab,
        "Caps_Lock" => K::CapsLock,
        "Num_Lock" => K::NumLock,
        "Scroll_Lock" => K::ScrollLock,
        "Print" => K::PrintScreen,
        "Pause" => K::Pause,
        "F1" => K::F1,
        "F2" => K::F2,
        "F3" => K::F3,
        "F4" => K::F4,
        "F5" => K::F5,
        "F6" => K::F6,
        "F7" => K::F7,
        "F8" => K::F8,
        "F9" => K::F9,
        "F10" => K::F10,
        "F11" => K::F11,
        "F12" => K::F12,
        "F13" => K::F13,
        "F14" => K::F14,
        "F15" => K::F15,
        "F16" => K::F16,
        "F17" => K::F17,
        "F18" => K::F18,
        "F19" => K::F19,
        "F20" => K::F20,
        "F21" => K::F21,
        "F22" => K::F22,
        "F23" => K::F23,
        "F24" => K::F24,
        "F25" => K::F25,
        "F26" => K::F26,
        "F27" => K::F27,
        "F28" => K::F28,
        "F29" => K::F29,
        "F30" => K::F30,
        "F31" => K::F31,
        "F32" => K::F32,
        "F33" => K::F33,
        "F34" => K::F34,
        "F35" => K::F35,
        "Home" => K::Home,
        "Up" => K::ArrowUp,
        "Page_Up" => K::PageUp,
        "Left" => K::ArrowLeft,
        "Right" => K::ArrowRight,
        "End" => K::End,
        "Down" => K::ArrowDown,
        "Page_Down" => K::PageDown,
        "Insert" => K::Insert,
        "Delete" => K::Delete,
        "XF86Calculator" => K::LaunchCalculator,
        "XF86Sleep" => K::Sleep,
        "XF86WakeUp" => K::WakeUp,
        "XF86Mail" => K::LaunchMail,
        "XF86Back" => K::BrowserBack,
        "XF86Forward" => K::BrowserForward,
        "XF86AudioNext" => K::MediaNextTrack,
        "XF86AudioPlay" => K::MediaPlay,
        "XF86AudioPrev" => K::MediaPreviousTrack,
        "XF86AudioStop" => K::MediaStop,
        "XF86HomePage" => K::BrowserHome,
        "XF86MonBrightnessDown" => K::BrightnessDown,
        "XF86MonBrightnessUp" => K::BrightnessUp,
        "XF86AudioMute" => K::VolumeMute,
        "XF86AudioLowerVolume" => K::VolumeDown,
        "XF86AudioRaiseVolume" => K::VolumeUp,
        _ => return None,
    })
}

fn xkb_keymap_maximum(keymap: &str) -> Result<u32, String> {
    let (_, open, close) = xkb_section_bounds(keymap, "xkb_keycodes ")?;
    let start = open
        + keymap[open..close]
            .find("maximum = ")
            .ok_or("Rust XKB keymap has no keycode maximum")?
        + "maximum = ".len();
    let end = start
        + keymap[start..close]
            .find(';')
            .ok_or("Rust XKB keycode maximum is malformed")?;
    keymap[start..end]
        .trim()
        .parse()
        .map_err(|_| "Rust XKB keycode maximum is malformed".into())
}

fn normalize_legacy_xkb_keysyms(keymap: &str) -> String {
    // Keep Latin keysyms canonical so toolkits recognize shortcuts such as Ctrl+A.
    let bytes = keymap.as_bytes();
    let mut normalized = String::with_capacity(keymap.len());
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset..].starts_with(b"0x")
            && offset + 10 <= bytes.len()
            && bytes[offset + 2..offset + 10]
                .iter()
                .all(u8::is_ascii_hexdigit)
            && let Ok(symbol) = u32::from_str_radix(&keymap[offset + 2..offset + 10], 16)
            && (0x0100_0000..=0x0100_00ff).contains(&symbol)
        {
            normalized.push_str(&format!("0x{:08x}", symbol & 0xff));
            offset += 10;
            continue;
        }
        let character = keymap[offset..]
            .chars()
            .next()
            .expect("offset remains on a UTF-8 boundary");
        normalized.push(character);
        offset += character.len_utf8();
    }
    normalized
}

#[cfg(test)]
fn compile_xkb_keymap(
    settings: &XkbSettings,
) -> Result<(String, HashMap<String, KeyMapping>, HashMap<u32, u32>), String> {
    compile_xkb_keymap_with_source(settings, None)
}

fn compile_xkb_keymap_with_source(
    settings: &XkbSettings,
    compositor_keymap: Option<&str>,
) -> Result<(String, HashMap<String, KeyMapping>, HashMap<u32, u32>), String> {
    use crate::DynamicLibrary;

    type ContextNew = unsafe extern "C" fn(u32) -> *mut c_void;
    type ContextUnref = unsafe extern "C" fn(*mut c_void);
    type KeymapNewFromNames =
        unsafe extern "C" fn(*mut c_void, *const XkbRuleNames, u32) -> *mut c_void;
    type KeymapNewFromString =
        unsafe extern "C" fn(*mut c_void, *const c_char, u32, u32) -> *mut c_void;
    type KeymapNumLayouts = unsafe extern "C" fn(*mut c_void) -> u32;
    type KeymapLayoutGetName = unsafe extern "C" fn(*mut c_void, u32) -> *const c_char;
    type KeymapUnref = unsafe extern "C" fn(*mut c_void);
    type KeymapGetAsString = unsafe extern "C" fn(*mut c_void, u32) -> *mut c_char;
    type KeymapMinKeycode = unsafe extern "C" fn(*mut c_void) -> u32;
    type KeymapMaxKeycode = unsafe extern "C" fn(*mut c_void) -> u32;
    type KeymapNumLevelsForKey = unsafe extern "C" fn(*mut c_void, u32, u32) -> u32;
    type KeyGetSymsByLevel =
        unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut *const u32) -> c_int;
    type KeyGetModsForLevel =
        unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut u32, usize) -> usize;
    type KeymapModGetName = unsafe extern "C" fn(*mut c_void, u32) -> *const c_char;
    type KeysymFromName = unsafe extern "C" fn(*const c_char, u32) -> u32;
    type StateNew = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    type StateUnref = unsafe extern "C" fn(*mut c_void);
    type StateUpdateKey = unsafe extern "C" fn(*mut c_void, u32, u32) -> u32;
    type StateSerializeMods = unsafe extern "C" fn(*mut c_void, u32) -> u32;

    let library = DynamicLibrary::open(c"libxkbcommon.so.0")?;
    let context_new: ContextNew = library.symbol(c"xkb_context_new")?;
    let context_unref: ContextUnref = library.symbol(c"xkb_context_unref")?;
    let keymap_new: KeymapNewFromNames = library.symbol(c"xkb_keymap_new_from_names")?;
    let keymap_new_from_string: KeymapNewFromString =
        library.symbol(c"xkb_keymap_new_from_string")?;
    let keymap_num_layouts: KeymapNumLayouts = library.symbol(c"xkb_keymap_num_layouts")?;
    let keymap_layout_get_name: KeymapLayoutGetName =
        library.symbol(c"xkb_keymap_layout_get_name")?;
    let keymap_unref: KeymapUnref = library.symbol(c"xkb_keymap_unref")?;
    let keymap_get_as_string: KeymapGetAsString = library.symbol(c"xkb_keymap_get_as_string")?;
    let keymap_min_keycode: KeymapMinKeycode = library.symbol(c"xkb_keymap_min_keycode")?;
    let keymap_max_keycode: KeymapMaxKeycode = library.symbol(c"xkb_keymap_max_keycode")?;
    let keymap_num_levels_for_key: KeymapNumLevelsForKey =
        library.symbol(c"xkb_keymap_num_levels_for_key")?;
    let key_get_syms_by_level: KeyGetSymsByLevel =
        library.symbol(c"xkb_keymap_key_get_syms_by_level")?;
    let key_get_mods_for_level: KeyGetModsForLevel =
        library.symbol(c"xkb_keymap_key_get_mods_for_level")?;
    let keymap_mod_get_name: KeymapModGetName = library.symbol(c"xkb_keymap_mod_get_name")?;
    let keysym_from_name: KeysymFromName = library.symbol(c"xkb_keysym_from_name")?;
    let state_new: StateNew = library.symbol(c"xkb_state_new")?;
    let state_unref: StateUnref = library.symbol(c"xkb_state_unref")?;
    let state_update_key: StateUpdateKey = library.symbol(c"xkb_state_update_key")?;
    let state_serialize_mods: StateSerializeMods = library.symbol(c"xkb_state_serialize_mods")?;

    let c_string = |value: Option<&str>| -> Result<Option<CString>, String> {
        value
            .filter(|value| !value.is_empty())
            .map(|value| {
                CString::new(value)
                    .map_err(|_| "Hyprland XKB configuration contains NUL".to_owned())
            })
            .transpose()
    };
    let rules = c_string(settings.rules.as_deref())?;
    let model = c_string(settings.model.as_deref())?;
    let layout = CString::new(settings.layout.as_str())
        .map_err(|_| "Hyprland keyboard layout contains NUL")?;
    let variant = c_string(settings.variant.as_deref())?;
    let options = c_string(settings.options.as_deref())?;
    let names = XkbRuleNames {
        rules: rules
            .as_ref()
            .map_or(std::ptr::null(), |value| value.as_ptr()),
        model: model
            .as_ref()
            .map_or(std::ptr::null(), |value| value.as_ptr()),
        layout: layout.as_ptr(),
        variant: variant
            .as_ref()
            .map_or(std::ptr::null(), |value| value.as_ptr()),
        options: options
            .as_ref()
            .map_or(std::ptr::null(), |value| value.as_ptr()),
    };
    let source = compositor_keymap
        .map(CString::new)
        .transpose()
        .map_err(|_| "Hyprland compositor keymap contains NUL")?;
    let context = unsafe { context_new(0) };
    if context.is_null() {
        return Err("creating an XKB context failed".into());
    }
    let keymap = unsafe {
        if let Some(source) = source.as_ref() {
            keymap_new_from_string(context, source.as_ptr(), 1, 0)
        } else {
            keymap_new(context, &names, 0)
        }
    };
    if keymap.is_null() {
        unsafe { context_unref(context) };
        return Err(format!(
            "compiling Hyprland XKB layout {:?} failed",
            settings.layout
        ));
    }
    let group = if compositor_keymap.is_some() {
        settings.group_index
    } else {
        0
    };
    let layout_count = unsafe { keymap_num_layouts(keymap) };
    if group >= layout_count {
        unsafe {
            keymap_unref(keymap);
            context_unref(context)
        };
        return Err(format!(
            "active Hyprland layout index {group} is outside the compositor keymap ({layout_count} layouts)"
        ));
    }
    if compositor_keymap.is_some() {
        let Some(expected) = settings.active_keymap.as_deref() else {
            unsafe {
                keymap_unref(keymap);
                context_unref(context)
            };
            return Err("Hyprland did not report the active keyboard map name".into());
        };
        let actual = unsafe { keymap_layout_get_name(keymap, group) };
        if actual.is_null()
            || normalize_keymap_name(unsafe { CStr::from_ptr(actual) }.to_string_lossy().as_ref())
                != normalize_keymap_name(expected)
        {
            let actual = if actual.is_null() {
                "<unknown>".into()
            } else {
                unsafe { CStr::from_ptr(actual) }
                    .to_string_lossy()
                    .into_owned()
            };
            unsafe {
                keymap_unref(keymap);
                context_unref(context)
            };
            return Err(format!(
                "Wayland keyboard map {actual:?} does not match Hyprland's active map {expected:?}"
            ));
        }
        let expected_keymap = unsafe { keymap_new(context, &names, 0) };
        if !expected_keymap.is_null() {
            let actual_text = unsafe { keymap_get_as_string(keymap, 1) };
            let expected_text = unsafe { keymap_get_as_string(expected_keymap, 1) };
            let matches_settings = !actual_text.is_null()
                && !expected_text.is_null()
                && unsafe { CStr::from_ptr(actual_text) }.to_bytes()
                    == unsafe { CStr::from_ptr(expected_text) }.to_bytes();
            if !actual_text.is_null() {
                unsafe { free(actual_text.cast()) };
            }
            if !expected_text.is_null() {
                unsafe { free(expected_text.cast()) };
            }
            unsafe { keymap_unref(expected_keymap) };
            if !matches_settings {
                unsafe {
                    keymap_unref(keymap);
                    context_unref(context)
                };
                return Err("Wayland keyboard keymap does not exactly match the active Hyprland XKB settings".into());
            }
        }
    }
    let text = unsafe { keymap_get_as_string(keymap, 1) };
    if text.is_null() {
        unsafe {
            keymap_unref(keymap);
            context_unref(context)
        };
        return Err("serializing the Hyprland XKB keymap failed".into());
    }
    let mut keymap_text = unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned();
    unsafe {
        free(text.cast());
    }

    let mut keycodes = HashMap::new();
    let minimum = unsafe { keymap_min_keycode(keymap) };
    let maximum = unsafe { keymap_max_keycode(keymap) };
    let mut missing_symbols = Vec::new();
    for (name, _, _, symbol) in INPUT_KEYS {
        let symbol_name = *symbol;
        let symbol = CString::new(symbol_name).map_err(|_| "built-in XKB symbol contains NUL")?;
        let target = unsafe { keysym_from_name(symbol.as_ptr(), 1) };
        if target == 0 {
            continue;
        }
        let mut found: Option<KeyMapping> = None;
        for keycode in minimum..=maximum {
            let levels = unsafe { keymap_num_levels_for_key(keymap, keycode, group) };
            for level in 0..levels {
                let mut symbols = std::ptr::null();
                let count =
                    unsafe { key_get_syms_by_level(keymap, keycode, group, level, &mut symbols) };
                if count <= 0
                    || !unsafe { std::slice::from_raw_parts(symbols, count as usize) }
                        .contains(&target)
                {
                    continue;
                }
                let mut masks = [0_u32; 32];
                let count = unsafe {
                    key_get_mods_for_level(
                        keymap,
                        keycode,
                        group,
                        level,
                        masks.as_mut_ptr(),
                        masks.len(),
                    )
                };
                let modifiers = masks[..count.min(masks.len())]
                    .iter()
                    .filter_map(|mask| xkb_modifiers_for_mask(keymap, *mask, keymap_mod_get_name))
                    .min_by_key(Vec::len);
                if let Some(modifiers) = modifiers {
                    let mapping = KeyMapping {
                        keycode: keycode.saturating_sub(8),
                        modifiers,
                    };
                    if found
                        .as_ref()
                        .is_none_or(|current| mapping.modifiers.len() < current.modifiers.len())
                    {
                        found = Some(mapping);
                    }
                }
            }
        }
        if let Some(mapping) = found {
            keycodes.insert((*name).to_owned(), mapping);
        } else {
            missing_symbols.push((*name, symbol_name));
        }
    }

    let mut synthetic_symbols = Vec::new();
    let mut next_keycode = maximum.saturating_add(1);
    for (name, symbol) in missing_symbols {
        let Some(evdev_keycode) = next_keycode
            .checked_sub(8)
            .filter(|keycode| *keycode <= MAX_LINUX_KEYCODE)
        else {
            break;
        };
        synthetic_symbols.push((next_keycode, symbol.to_owned()));
        keycodes.insert(
            name.to_owned(),
            KeyMapping {
                keycode: evdev_keycode,
                modifiers: Vec::new(),
            },
        );
        next_keycode = next_keycode.saturating_add(1);
    }
    keymap_text = extend_xkb_keymap_with_symbols_in_group(&keymap_text, &synthetic_symbols, group)?;
    if !synthetic_symbols.is_empty() {
        let serialized = match CString::new(keymap_text.as_bytes()) {
            Ok(serialized) => serialized,
            Err(_) => {
                unsafe {
                    keymap_unref(keymap);
                    context_unref(context);
                }
                return Err("extended Hyprland XKB keymap contains NUL".into());
            }
        };
        let extended_keymap = unsafe { keymap_new_from_string(context, serialized.as_ptr(), 1, 0) };
        if extended_keymap.is_null() {
            unsafe {
                keymap_unref(keymap);
                context_unref(context);
            }
            return Err("validating the extended Hyprland XKB keymap failed".into());
        }
        unsafe {
            keymap_unref(extended_keymap);
        }
    }

    let level3 = CString::new("ISO_Level3_Shift").expect("static XKB symbol has no NUL");
    let level3_keysym = unsafe { keysym_from_name(level3.as_ptr(), 1) };
    if level3_keysym != 0 {
        'outer: for keycode in minimum..=maximum {
            let levels = unsafe { keymap_num_levels_for_key(keymap, keycode, group) };
            for level in 0..levels {
                let mut symbols = std::ptr::null();
                let count =
                    unsafe { key_get_syms_by_level(keymap, keycode, group, level, &mut symbols) };
                if count > 0
                    && unsafe { std::slice::from_raw_parts(symbols, count as usize) }
                        .contains(&level3_keysym)
                {
                    keycodes.insert(
                        "ALTGR".into(),
                        KeyMapping {
                            keycode: keycode.saturating_sub(8),
                            modifiers: Vec::new(),
                        },
                    );
                    break 'outer;
                }
            }
        }
    }
    let mut modifier_masks = HashMap::new();
    for keycode in minimum..=maximum {
        let state = unsafe { state_new(keymap) };
        if state.is_null() {
            continue;
        }
        unsafe {
            state_update_key(state, keycode, 1);
        }
        let mask = unsafe { state_serialize_mods(state, 1) };
        unsafe {
            state_unref(state);
        }
        if mask != 0 {
            modifier_masks.insert(keycode.saturating_sub(8), mask);
        }
    }
    unsafe {
        keymap_unref(keymap);
        context_unref(context)
    };
    Ok((keymap_text, keycodes, modifier_masks))
}

#[cfg(test)]
fn extend_xkb_keymap_with_symbols(
    keymap: &str,
    additions: &[(u32, String)],
) -> Result<String, String> {
    extend_xkb_keymap_with_symbols_in_group(keymap, additions, 0)
}

fn extend_xkb_keymap_with_symbols_in_group(
    keymap: &str,
    additions: &[(u32, String)],
    group: u32,
) -> Result<String, String> {
    if additions.is_empty() {
        return Ok(keymap.to_owned());
    }

    let (_, keycodes_open, keycodes_close) = xkb_section_bounds(keymap, "xkb_keycodes ")?;
    let maximum_start = keymap[keycodes_open..keycodes_close]
        .find("maximum = ")
        .map(|offset| keycodes_open + offset + "maximum = ".len())
        .ok_or("compiled XKB keycodes section has no maximum")?;
    let maximum_end = keymap[maximum_start..keycodes_close]
        .find(';')
        .map(|offset| maximum_start + offset)
        .ok_or("compiled XKB keycodes maximum is malformed")?;
    let current_maximum = keymap[maximum_start..maximum_end]
        .trim()
        .parse::<u32>()
        .map_err(|_| "compiled XKB keycodes maximum is malformed")?;
    let new_maximum = additions
        .iter()
        .map(|(keycode, _)| *keycode)
        .max()
        .unwrap_or(current_maximum);
    if new_maximum <= current_maximum {
        return Err("synthetic XKB keycodes must extend the active keymap".into());
    }

    let mut extended = keymap.to_owned();
    extended.replace_range(maximum_start..maximum_end, &new_maximum.to_string());
    let (start, _, close) = xkb_section_bounds(&extended, "xkb_keycodes ")?;
    let mut keycode_entries = String::new();
    for (keycode, _) in additions {
        let name = format!("I{keycode:03}");
        if name.len() != 4 || extended[start..close].contains(&format!("<{name}>")) {
            return Err(format!("cannot allocate synthetic XKB keycode {keycode}"));
        }
        keycode_entries.push_str(&format!("\n\t<{name}> = {keycode};"));
    }
    extended.insert_str(close, &keycode_entries);

    let (_, _, symbols_close) = xkb_section_bounds(&extended, "xkb_symbols ")?;
    let mut symbol_entries = String::new();
    for (keycode, symbol) in additions {
        let name = format!("I{keycode:03}");
        if group == 0 {
            symbol_entries.push_str(&format!("\n\tkey <{name}> {{ [ {symbol} ] }};"));
        } else {
            symbol_entries.push_str(&format!(
                "\n\tkey <{name}> {{ symbols[{}] = [ {symbol} ] }};",
                group + 1
            ));
        }
    }
    extended.insert_str(symbols_close, &symbol_entries);
    Ok(extended)
}

fn xkb_section_bounds(keymap: &str, section: &str) -> Result<(usize, usize, usize), String> {
    let start = keymap
        .find(section)
        .ok_or_else(|| format!("compiled XKB keymap has no {section} section"))?;
    let open = keymap[start..]
        .find('{')
        .map(|offset| start + offset)
        .ok_or_else(|| format!("compiled XKB {section} section is malformed"))?;
    let mut depth = 0_u32;
    for (offset, byte) in keymap.as_bytes().iter().enumerate().skip(open) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| format!("compiled XKB {section} section is malformed"))?;
                if depth == 0 {
                    return Ok((start, open, offset));
                }
            }
            _ => {}
        }
    }
    Err(format!("compiled XKB {section} section is unterminated"))
}

fn xkb_modifiers_for_mask(
    keymap: *mut c_void,
    mask: u32,
    mod_get_name: unsafe extern "C" fn(*mut c_void, u32) -> *const c_char,
) -> Option<Vec<&'static str>> {
    let mut modifiers = Vec::new();
    for index in 0..32 {
        if mask & (1 << index) == 0 {
            continue;
        }
        let name = unsafe { mod_get_name(keymap, index) };
        if name.is_null() {
            return None;
        }
        let name = unsafe { CStr::from_ptr(name) }.to_bytes();
        let modifier = match name {
            b"Shift" => "SHIFT",
            b"Control" => "CTRL",
            b"Mod1" | b"Alt" => "ALT",
            b"Mod4" | b"Super" => "SUPER",
            b"Mod5" | b"LevelThree" => "ALTGR",
            _ => return None,
        };
        if !modifiers.contains(&modifier) {
            modifiers.push(modifier);
        }
    }
    Some(modifiers)
}

fn modifier_keymap() -> String {
    let mut keymap = String::from(
        r#"xkb_keymap {
xkb_keycodes "computer-use" { minimum = 8; maximum = 255; <LCTL> = 37; <RCTL> = 105; <LFSH> = 50; <RTSH> = 62; <LALT> = 64; <RALT> = 108; <LWIN> = 133; <RWIN> = 134;
"#,
    );
    for (_, name, evdev, _) in INPUT_KEYS {
        keymap.push_str(&format!("<{name}> = {};\n", evdev + 8));
    }
    keymap.push_str(r#"};
xkb_types { type "ONE_LEVEL" { modifiers = None; map[None] = Level1; level_name[Level1] = "Any"; }; type "TWO_LEVEL" { modifiers = Shift; map[None] = Level1; map[Shift] = Level2; level_name[Level1] = "Base"; level_name[Level2] = "Shift"; }; };
xkb_compatibility { interpret Caps_Lock+AnyOfOrNone(all) { action = LockMods(modifiers=Lock); }; interpret Num_Lock+AnyOfOrNone(all) { action = LockMods(modifiers=Mod2); }; interpret Scroll_Lock+AnyOfOrNone(all) { action = LockMods(modifiers=Mod3); }; };
xkb_symbols "computer-use" { key <LCTL> { [ Control_L ] }; key <RCTL> { [ Control_R ] }; key <LFSH> { [ Shift_L ] }; key <RTSH> { [ Shift_R ] }; key <LALT> { [ Alt_L ] }; key <RALT> { [ Alt_R ] }; key <LWIN> { [ Super_L ] }; key <RWIN> { [ Super_R ] }; modifier_map Control { <LCTL>, <RCTL> }; modifier_map Shift { <LFSH>, <RTSH> }; modifier_map Mod1 { <LALT>, <RALT> }; modifier_map Mod4 { <LWIN>, <RWIN> }; modifier_map Lock { <CAPS> }; modifier_map Mod2 { <NMLK> }; modifier_map Mod3 { <SCLK> };
"#);
    for (_, name, _, symbol) in INPUT_KEYS {
        if let Some(shifted) = shifted_symbol(symbol) {
            keymap.push_str(&format!(
                "key <{name}> {{ type = \"TWO_LEVEL\", [ {symbol}, {shifted} ] }};\n"
            ));
        } else {
            keymap.push_str(&format!("key <{name}> {{ [ {symbol} ] }};\n"));
        }
    }
    keymap.push_str("};\n};\n");
    keymap
}

fn shifted_symbol(symbol: &str) -> Option<String> {
    let shifted = match symbol {
        "1" => "exclam",
        "2" => "at",
        "3" => "numbersign",
        "4" => "dollar",
        "5" => "percent",
        "6" => "asciicircum",
        "7" => "ampersand",
        "8" => "asterisk",
        "9" => "parenleft",
        "0" => "parenright",
        "minus" => "underscore",
        "equal" => "plus",
        "bracketleft" => "braceleft",
        "bracketright" => "braceright",
        "grave" => "asciitilde",
        "backslash" => "bar",
        "semicolon" => "colon",
        "apostrophe" => "quotedbl",
        "comma" => "less",
        "period" => "greater",
        "slash" => "question",
        _ if symbol.len() == 1 && symbol.as_bytes()[0].is_ascii_lowercase() => {
            return Some(symbol.to_ascii_uppercase());
        }
        _ => return None,
    };
    Some(shifted.into())
}

pub fn type_text_cancellable(text: &str, cancelled: &AtomicBool) -> Result<(), String> {
    type_text_inner(text, Some(cancelled))
}

fn type_text_inner(text: &str, cancelled: Option<&AtomicBool>) -> Result<(), String> {
    if text.len() > MAX_TEXT_BYTES {
        return Err("text input exceeds 64 KiB".into());
    }
    if text
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return Err(
            "text input does not support control characters other than tab and newline".into(),
        );
    }
    if text.is_empty() {
        return Ok(());
    }
    if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        return Err("text input cancelled before typing started".into());
    }

    let (_connection, mut queue, mut state) = discover()?;
    let qh = queue.handle();
    let seat = state
        .seat
        .as_ref()
        .ok_or("Wayland has no input seat")?
        .clone();
    let manager = state
        .manager
        .as_ref()
        .ok_or("compositor has no virtual keyboard protocol")?
        .clone();
    let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
    let keyboard_started = Instant::now();
    let mut last_event_time = 0;

    let mut chunk = String::new();
    let mut keycodes = HashMap::new();
    for ch in text.chars() {
        if !keycodes.contains_key(&ch) && keycodes.len() == MAX_KEYMAP_KEYS {
            send_chunk(
                &mut queue,
                &mut state,
                &keyboard,
                keyboard_started,
                &mut last_event_time,
                &mut chunk,
                &mut keycodes,
                cancelled,
            )?;
        }
        chunk.push(ch);
        if !keycodes.contains_key(&ch) {
            let keycode = keycodes.len() as u32 + 1;
            keycodes.insert(ch, keycode);
        }
    }
    if !chunk.is_empty() {
        send_chunk(
            &mut queue,
            &mut state,
            &keyboard,
            keyboard_started,
            &mut last_event_time,
            &mut chunk,
            &mut keycodes,
            cancelled,
        )?;
    }
    keyboard.destroy();
    queue
        .roundtrip(&mut state)
        .map(|_| ())
        .map_err(|error| format!("synchronizing virtual keyboard events: {error}"))
}

fn send_chunk(
    queue: &mut EventQueue<State>,
    state: &mut State,
    keyboard: &ZwpVirtualKeyboardV1,
    keyboard_started: Instant,
    last_event_time: &mut u32,
    text: &mut String,
    keycodes: &mut HashMap<char, u32>,
    cancelled: Option<&AtomicBool>,
) -> Result<(), String> {
    let keymap = make_keymap(keycodes)?;
    let file = create_keymap_file(keymap.as_bytes())?;
    let size =
        u32::try_from(keymap.len() + 1).map_err(|_| "virtual keyboard keymap is too large")?;
    keyboard.keymap(wl_keyboard::KeymapFormat::XkbV1.into(), file.as_fd(), size);
    queue
        .roundtrip(state)
        .map_err(|error| format!("synchronizing virtual keyboard keymap: {error}"))?;
    drop(file);

    for ch in text.chars() {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err("text input cancelled after releasing the previous key".into());
        }
        let keycode = *keycodes
            .get(&ch)
            .ok_or("virtual keyboard keymap lost a character")?;
        // The protocol takes an evdev keycode; XKB applies the +8 offset itself.
        *last_event_time = (*last_event_time)
            .max(keyboard_started.elapsed().as_millis() as u32)
            .saturating_add(1);
        keyboard.key(
            *last_event_time,
            keycode,
            wl_keyboard::KeyState::Pressed.into(),
        );
        queue
            .roundtrip(state)
            .map_err(|error| format!("sending virtual keyboard press: {error}"))?;
        std::thread::sleep(Duration::from_millis(2));
        *last_event_time = (*last_event_time)
            .max(keyboard_started.elapsed().as_millis() as u32)
            .saturating_add(1);
        keyboard.key(
            *last_event_time,
            keycode,
            wl_keyboard::KeyState::Released.into(),
        );
        queue
            .roundtrip(state)
            .map_err(|error| format!("sending virtual keyboard release: {error}"))?;
        std::thread::sleep(Duration::from_millis(2));
        if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err("text input cancelled after releasing the current key".into());
        }
    }
    text.clear();
    keycodes.clear();
    Ok(())
}

fn make_keymap(keycodes: &HashMap<char, u32>) -> Result<String, String> {
    let mut entries = keycodes.iter().collect::<Vec<_>>();
    entries.sort_by_key(|(_, code)| **code);
    let maximum = entries.len() + 8;
    let mut keymap =
        format!("xkb_keymap {{\nxkb_keycodes \"(unnamed)\" {{ minimum = 8; maximum = {maximum};\n");
    for (_, code) in &entries {
        keymap.push_str(&format!("<K{code}> = {};\n", *code + 8));
    }
    keymap.push_str("};\nxkb_types { type \"ONE_LEVEL\" { modifiers = None; map[None] = Level1; level_name[Level1] = \"Any\"; }; };\nxkb_compatibility { };\nxkb_symbols \"(unnamed)\" {\n");
    for (ch, code) in entries {
        let symbol = match ch {
            '\n' | '\r' => "Return".to_owned(),
            '\t' => "Tab".to_owned(),
            ch => format!("U{:04X}", *ch as u32),
        };
        keymap.push_str(&format!("key <K{code}> {{ [ {symbol} ] }};\n"));
    }
    keymap.push_str("};\n};\n");
    Ok(keymap)
}

fn discover() -> Result<(Connection, EventQueue<State>, State), String> {
    let connection = Connection::connect_to_env()
        .map_err(|error| format!("connecting to Wayland for keyboard input: {error}"))?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    let mut state = State::default();
    queue
        .flush()
        .map_err(|error| format!("requesting Wayland keyboard globals: {error}"))?;
    wait_for(&mut queue, &mut state, Duration::from_secs(3), |state| {
        state.manager.is_some() && state.seat.is_some()
    })?;
    Ok((connection, queue, state))
}

fn wait_for(
    queue: &mut EventQueue<State>,
    state: &mut State,
    timeout: Duration,
    complete: impl Fn(&State) -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        queue
            .dispatch_pending(state)
            .map_err(|error| format!("dispatching virtual keyboard events: {error}"))?;
        if complete(state) {
            return Ok(());
        }
        queue
            .flush()
            .map_err(|error| format!("flushing virtual keyboard requests: {error}"))?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("timed out discovering the Wayland virtual keyboard".into());
        }
        let mut fd = crate::virtual_pointer::PollFd {
            fd: queue.as_fd().as_raw_fd(),
            events: 1,
            revents: 0,
        };
        let milliseconds = remaining.as_millis().min(i32::MAX as u128) as i32;
        let ready = unsafe { crate::virtual_pointer::poll(&mut fd, 1, milliseconds) };
        if ready == 0 {
            return Err("timed out discovering the Wayland virtual keyboard".into());
        }
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("waiting for the Wayland keyboard socket: {error}"));
        }
        queue
            .blocking_dispatch(state)
            .map_err(|error| format!("receiving virtual keyboard globals: {error}"))?;
    }
}

fn create_keymap_file(keymap: &[u8]) -> Result<File, String> {
    let name = c"computer-use-keymap";
    let fd = unsafe { memfd_create(name.as_ptr(), 1) };
    if fd < 0 {
        return Err(format!(
            "creating the virtual keyboard keymap: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(keymap)
        .map_err(|error| format!("writing the virtual keyboard keymap: {error}"))?;
    file.write_all(&[0])
        .map_err(|error| format!("terminating the virtual keyboard keymap: {error}"))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::{
        INPUT_KEYS, XkbSettings, compile_xkb_keymap, compile_xkb_keymap_with_source,
        current_hyprland_xkb_settings, current_wayland_keymap, extend_xkb_keymap_with_symbols,
        keycode_for,
    };
    use std::collections::HashSet;

    #[test]
    fn maps_extended_function_keypad_and_media_keysyms() {
        for key in [
            "F35",
            "KP_Enter",
            "KP_7",
            "KP_Page_Up",
            "XF86AudioMute",
            "XF86HomePage",
        ] {
            assert!(keycode_for(key).is_ok(), "missing key mapping for {key}");
        }
        assert!(keycode_for("not-a-keysym").is_err());
    }

    #[test]
    fn embedded_keymap_has_no_duplicate_evdev_codes() {
        let mut codes = HashSet::new();
        for (_, _, code, _) in INPUT_KEYS {
            assert!(codes.insert(code), "duplicate virtual keycode {code}");
        }
    }

    #[test]
    fn adds_missing_symbols_to_spare_extended_xkb_keycodes() {
        let keymap = r#"xkb_keymap {
xkb_keycodes "test" { minimum = 8; maximum = 255; <ESC> = 9; };
xkb_types "test" { type "ONE_LEVEL" { modifiers = None; map[None] = Level1; level_name[Level1] = "Any"; }; };
xkb_compatibility "test" {};
xkb_symbols "test" { key <ESC> { [ Escape ] }; };
};
"#;
        let extended = extend_xkb_keymap_with_symbols(keymap, &[(256, "F35".into())]).unwrap();
        assert!(extended.contains("maximum = 256;"));
        assert!(extended.contains("<I256> = 256;"));
        assert!(extended.contains("key <I256> { [ F35 ] };"));
    }

    #[test]
    fn active_layout_maps_named_keys_without_reusing_existing_codes() {
        for layout in ["us", "it", "de", "fr"] {
            let settings = XkbSettings {
                layout: layout.into(),
                group_index: 0,
                active_keymap: None,
                variant: None,
                rules: Some("evdev".into()),
                model: Some("pc105".into()),
                options: None,
            };
            let (_, keys, _) = match compile_xkb_keymap(&settings) {
                Ok(compiled) => compiled,
                Err(error)
                    if error.contains("could not load libxkbcommon")
                        || error.contains("missing symbol") =>
                {
                    return;
                }
                Err(error) => panic!("compiling the {layout} XKB map failed: {error}"),
            };
            let missing = INPUT_KEYS
                .iter()
                .filter(|(name, _, _, _)| !keys.contains_key(*name))
                .map(|(name, _, _, _)| *name)
                .collect::<Vec<_>>();
            assert!(
                missing.is_empty(),
                "{layout} has unmapped named keys: {missing:?}"
            );
        }
    }

    #[test]
    fn compositor_keymap_maps_named_keys_for_the_active_layout() {
        let (Ok(settings), Ok(compositor_keymap)) =
            (current_hyprland_xkb_settings(), current_wayland_keymap())
        else {
            return;
        };
        match compile_xkb_keymap_with_source(&settings, Some(&compositor_keymap)) {
            Ok((_, source_keys, _)) => {
                let missing = INPUT_KEYS
                    .iter()
                    .filter(|(name, _, _, _)| !source_keys.contains_key(*name))
                    .map(|(name, _, _, _)| *name)
                    .collect::<Vec<_>>();
                assert!(
                    missing.is_empty(),
                    "matching compositor keymap has unmapped keys: {missing:?}"
                );
            }
            Err(error)
                if error.contains("could not load libxkbcommon")
                    || error.contains("missing symbol") =>
            {
                return;
            }
            Err(error) => assert!(
                error.contains("does not match")
                    || error.contains("does not exactly match")
                    // The live compositor can briefly expose a keymap without layouts.
                    || error.contains("is outside the compositor keymap"),
                "unexpected compositor keymap error: {error}"
            ),
        }
        let mut wrong_name = settings.clone();
        wrong_name.active_keymap = Some("deliberately-mismatched-layout".into());
        if let Err(error) = compile_xkb_keymap_with_source(&wrong_name, Some(&compositor_keymap)) {
            if error.contains("could not load libxkbcommon") || error.contains("missing symbol") {
                return;
            }
            assert!(
                error.contains("does not match Hyprland's active map"),
                "unexpected keymap validation error: {error}"
            );
        } else {
            panic!("a compositor keymap with the wrong active name was accepted");
        }
        let (_, keys, _) = match compile_xkb_keymap_with_source(&settings, Some(&compositor_keymap))
            .or_else(|_| compile_xkb_keymap_with_source(&settings, None))
        {
            Ok(compiled) => compiled,
            Err(error)
                if error.contains("could not load libxkbcommon")
                    || error.contains("missing symbol") =>
            {
                return;
            }
            Err(error) => panic!("compiling the active compositor keymap failed: {error}"),
        };
        let missing = INPUT_KEYS
            .iter()
            .filter(|(name, _, _, _)| !keys.contains_key(*name))
            .map(|(name, _, _, _)| *name)
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "active compositor keymap has unmapped keys: {missing:?}"
        );
        if settings.layout == "it" {
            assert!(
                keys["BRACKETLEFT"].modifiers.contains(&"ALTGR"),
                "BRACKETLEFT resolved as {:?}",
                keys["BRACKETLEFT"].modifiers
            );
        }
    }

    #[test]
    fn italian_bracket_key_uses_the_active_layouts_altgr_modifier() {
        let settings = XkbSettings {
            layout: "it".into(),
            group_index: 0,
            active_keymap: None,
            variant: None,
            rules: Some("evdev".into()),
            model: Some("pc105".into()),
            options: None,
        };
        let (keymap, keys, modifier_masks) = match compile_xkb_keymap(&settings) {
            Ok(compiled) => compiled,
            Err(error)
                if error.contains("could not load libxkbcommon")
                    || error.contains("missing symbol") =>
            {
                return;
            }
            Err(error) => panic!("compiling the installed Italian XKB map failed: {error}"),
        };
        assert!(!keymap.is_empty());
        assert!(keys["SEMICOLON"].modifiers.contains(&"SHIFT"));
        assert!(keys["BRACKETLEFT"].modifiers.contains(&"ALTGR"));
        let altgr_keycode = keys["ALTGR"].keycode;
        assert_ne!(modifier_masks[&altgr_keycode] & (1 << 7), 0);
    }
}
