use std::{
    collections::HashMap,
    ffi::{CStr, c_char, c_int, c_void},
    os::fd::{IntoRawFd, OwnedFd},
    ptr::NonNull,
    time::{Duration, Instant},
};

const POINTER_ABSOLUTE: c_int = 1 << 1;
const SCROLL: c_int = 1 << 4;
const BUTTON: c_int = 1 << 5;
const TEXT: c_int = 1 << 6;

#[link(name = "dl")]
unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
}

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

#[link(name = "c")]
unsafe extern "C" {
    fn poll(fds: *mut PollFd, nfds: usize, timeout: c_int) -> c_int;
}

struct Library(NonNull<c_void>);

impl Library {
    fn open() -> Result<Self, String> {
        for name in [c"libei.so.1", c"libei.so"] {
            let handle = unsafe { dlopen(name.as_ptr(), 2) };
            if let Some(handle) = NonNull::new(handle) {
                return Ok(Self(handle));
            }
        }
        Err("libei is not installed".into())
    }

    fn symbol<T: Copy>(&self, name: &CStr) -> Result<T, String> {
        let symbol = unsafe { dlsym(self.0.as_ptr(), name.as_ptr()) };
        if symbol.is_null() {
            return Err(format!("libei lacks {}", name.to_string_lossy()));
        }
        Ok(unsafe { std::mem::transmute_copy(&symbol) })
    }

    fn optional_symbol<T: Copy>(&self, name: &CStr) -> Option<T> {
        let symbol = unsafe { dlsym(self.0.as_ptr(), name.as_ptr()) };
        (!symbol.is_null()).then(|| unsafe { std::mem::transmute_copy(&symbol) })
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            dlclose(self.0.as_ptr());
        }
    }
}

type EiFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type SetupFn = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type GetFdFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type DispatchFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type GetEventFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type EventTypeFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type EventObjectFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type RefFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type UnrefFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type BindCapabilitiesFn = unsafe extern "C" fn(*mut c_void, ...);
type DeviceCapabilityFn = unsafe extern "C" fn(*mut c_void, c_int) -> bool;
type RegionFn = unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void;
type RegionStringFn = unsafe extern "C" fn(*mut c_void) -> *const c_char;
type RegionExtentFn = unsafe extern "C" fn(*mut c_void) -> u32;
type StartEmulatingFn = unsafe extern "C" fn(*mut c_void, u32);
type FrameFn = unsafe extern "C" fn(*mut c_void, u64);
type PointerAbsoluteFn = unsafe extern "C" fn(*mut c_void, f64, f64);
type ButtonFn = unsafe extern "C" fn(*mut c_void, u32, bool);
type ScrollFn = unsafe extern "C" fn(*mut c_void, i32, i32);
type ScrollDeltaFn = unsafe extern "C" fn(*mut c_void, f64, f64);
type ScrollStopFn = unsafe extern "C" fn(*mut c_void, bool, bool);
type KeysymFn = unsafe extern "C" fn(*mut c_void, u32, bool);
type TextFn = unsafe extern "C" fn(*mut c_void, *const c_char, usize);
type NowFn = unsafe extern "C" fn(*mut c_void) -> u64;

struct Api {
    _library: Library,
    new_sender: EiFn,
    setup_backend_fd: SetupFn,
    get_fd: GetFdFn,
    dispatch: DispatchFn,
    get_event: GetEventFn,
    event_type: EventTypeFn,
    event_seat: EventObjectFn,
    event_device: EventObjectFn,
    event_unref: UnrefFn,
    seat_ref: RefFn,
    seat_unref: UnrefFn,
    seat_bind_capabilities: BindCapabilitiesFn,
    device_ref: RefFn,
    device_unref: UnrefFn,
    device_has_capability: DeviceCapabilityFn,
    device_region: RegionFn,
    region_mapping_id: RegionStringFn,
    region_x: RegionExtentFn,
    region_y: RegionExtentFn,
    region_width: RegionExtentFn,
    region_height: RegionExtentFn,
    start_emulating: StartEmulatingFn,
    frame: FrameFn,
    pointer_absolute: PointerAbsoluteFn,
    button: ButtonFn,
    scroll_discrete: ScrollFn,
    scroll_delta: Option<ScrollDeltaFn>,
    scroll_stop: ScrollStopFn,
    text_keysym: KeysymFn,
    text_utf8: TextFn,
    now: NowFn,
    unref: UnrefFn,
}

impl Api {
    fn load() -> Result<Self, String> {
        let library = Library::open()?;
        Ok(Self {
            new_sender: library.symbol(c"ei_new_sender")?,
            setup_backend_fd: library.symbol(c"ei_setup_backend_fd")?,
            get_fd: library.symbol(c"ei_get_fd")?,
            dispatch: library.symbol(c"ei_dispatch")?,
            get_event: library.symbol(c"ei_get_event")?,
            event_type: library.symbol(c"ei_event_get_type")?,
            event_seat: library.symbol(c"ei_event_get_seat")?,
            event_device: library.symbol(c"ei_event_get_device")?,
            event_unref: library.symbol(c"ei_event_unref")?,
            seat_ref: library.symbol(c"ei_seat_ref")?,
            seat_unref: library.symbol(c"ei_seat_unref")?,
            seat_bind_capabilities: library.symbol(c"ei_seat_bind_capabilities")?,
            device_ref: library.symbol(c"ei_device_ref")?,
            device_unref: library.symbol(c"ei_device_unref")?,
            device_has_capability: library.symbol(c"ei_device_has_capability")?,
            device_region: library.symbol(c"ei_device_get_region")?,
            region_mapping_id: library.symbol(c"ei_region_get_mapping_id")?,
            region_x: library.symbol(c"ei_region_get_x")?,
            region_y: library.symbol(c"ei_region_get_y")?,
            region_width: library.symbol(c"ei_region_get_width")?,
            region_height: library.symbol(c"ei_region_get_height")?,
            start_emulating: library.symbol(c"ei_device_start_emulating")?,
            frame: library.symbol(c"ei_device_frame")?,
            pointer_absolute: library.symbol(c"ei_device_pointer_motion_absolute")?,
            button: library.symbol(c"ei_device_button_button")?,
            scroll_discrete: library.symbol(c"ei_device_scroll_discrete")?,
            scroll_delta: library.optional_symbol(c"ei_device_scroll_delta"),
            scroll_stop: library.symbol(c"ei_device_scroll_stop")?,
            text_keysym: library.symbol(c"ei_device_text_keysym")?,
            text_utf8: library.symbol(c"ei_device_text_utf8_with_length")?,
            now: library.symbol(c"ei_now")?,
            unref: library.symbol(c"ei_unref")?,
            _library: library,
        })
    }
}

struct Device {
    ptr: NonNull<c_void>,
    absolute_pointer: bool,
    button: bool,
    scroll: bool,
    text: bool,
    resumed: bool,
}

pub struct EisInput {
    api: Api,
    context: NonNull<c_void>,
    seats: Vec<NonNull<c_void>>,
    devices: Vec<Device>,
    pointer_mapping: Option<String>,
    pressed_buttons: HashMap<u32, String>,
    sequence: u32,
    connected: bool,
}

unsafe impl Send for EisInput {}

impl EisInput {
    pub fn available() -> bool {
        Api::load().is_ok()
    }

    pub fn connect(fd: OwnedFd) -> Result<Self, String> {
        let api = Api::load()?;
        let context = NonNull::new(unsafe { (api.new_sender)(std::ptr::null_mut()) })
            .ok_or("libei could not create a sender context")?;
        let raw_fd = fd.into_raw_fd();
        let result = unsafe { (api.setup_backend_fd)(context.as_ptr(), raw_fd) };
        if result < 0 {
            unsafe {
                (api.unref)(context.as_ptr());
            }
            return Err(format!(
                "libei could not connect to the portal EIS socket ({result})"
            ));
        }

        let mut input = Self {
            api,
            context,
            seats: Vec::new(),
            devices: Vec::new(),
            pointer_mapping: None,
            pressed_buttons: HashMap::new(),
            sequence: 0,
            connected: false,
        };
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            input.dispatch()?;
            if input.connected && input.devices.iter().any(|device| device.resumed) {
                return Ok(input);
            }
            input.wait(Duration::from_millis(100))?;
        }
        Err("libei did not provide an active input device within four seconds".into())
    }

    pub fn has_pointer_for(&mut self, mapping_id: &str) -> bool {
        self.has_pointer_mapping(mapping_id) && self.has_button_for(mapping_id)
    }

    pub fn has_pointer_mapping(&mut self, mapping_id: &str) -> bool {
        if self.dispatch().is_err() {
            return false;
        }
        self.devices.iter().any(|device| {
            device.absolute_pointer
                && device.resumed
                && self.region_for_mapping(device.ptr, mapping_id).is_some()
        })
    }

    pub fn has_button_for(&mut self, mapping_id: &str) -> bool {
        if self.dispatch().is_err() {
            return false;
        }
        self.devices.iter().any(|device| {
            device.button
                && device.resumed
                && self.region_for_mapping(device.ptr, mapping_id).is_some()
        })
    }

    pub fn has_keyboard(&mut self) -> bool {
        if self.dispatch().is_err() {
            return false;
        }
        self.devices
            .iter()
            .any(|device| device.text && device.resumed)
    }

    pub fn has_scroll_for(&mut self, mapping_id: &str) -> bool {
        if self.dispatch().is_err() {
            return false;
        }
        self.devices.iter().any(|device| {
            device.absolute_pointer
                && device.resumed
                && self.region_for_mapping(device.ptr, mapping_id).is_some()
        }) && self.devices.iter().any(|device| {
            device.scroll
                && device.resumed
                && self.region_for_mapping(device.ptr, mapping_id).is_some()
        })
    }

    pub fn has_smooth_scroll_for(&mut self, mapping_id: &str) -> bool {
        self.api.scroll_delta.is_some() && self.has_scroll_for(mapping_id)
    }

    pub fn pointer(
        &mut self,
        mapping_id: &str,
        x: f64,
        y: f64,
        source_width: u32,
        source_height: u32,
    ) -> Result<(), String> {
        self.dispatch()?;
        if source_width == 0 || source_height == 0 {
            return Err("portal stream has invalid dimensions for pointer mapping".into());
        }
        let (ptr, region) = self
            .devices
            .iter()
            .find_map(|device| {
                if device.absolute_pointer && device.resumed {
                    self.region_for_mapping(device.ptr, mapping_id)
                        .map(|region| (device.ptr, region))
                } else {
                    None
                }
            })
            .ok_or("EIS has no absolute pointer mapped to the captured stream")?;
        let local_x = self.region_local_coordinate(region, x, source_width, true)?;
        let local_y = self.region_local_coordinate(region, y, source_height, false)?;
        unsafe {
            (self.api.pointer_absolute)(ptr.as_ptr(), local_x, local_y);
            (self.api.frame)(ptr.as_ptr(), (self.api.now)(self.context.as_ptr()));
        }
        self.pointer_mapping = Some(mapping_id.to_owned());
        Ok(())
    }

    pub fn button(&mut self, button: u32, pressed: bool) -> Result<(), String> {
        self.dispatch()?;
        let mapping_id = if pressed {
            self.pointer_mapping.as_deref()
        } else {
            self.pressed_buttons
                .get(&button)
                .map(String::as_str)
                .or(self.pointer_mapping.as_deref())
        }
        .ok_or("EIS pointer has no selected display mapping")?;
        let device = self
            .devices
            .iter()
            .find(|device| {
                device.button
                    && device.resumed
                    && self.region_for_mapping(device.ptr, mapping_id).is_some()
            })
            .ok_or("EIS has no button device for the selected display")?;
        let device = device.ptr;
        unsafe {
            (self.api.button)(device.as_ptr(), button, pressed);
            (self.api.frame)(device.as_ptr(), (self.api.now)(self.context.as_ptr()));
        }
        if pressed {
            self.pressed_buttons.insert(button, mapping_id.to_owned());
        } else {
            self.pressed_buttons.remove(&button);
        }
        Ok(())
    }

    pub fn scroll(&mut self, direction: &str, amount: i32) -> Result<(), String> {
        self.dispatch()?;
        let mapping_id = self
            .pointer_mapping
            .as_deref()
            .ok_or("EIS pointer has no selected display mapping")?;
        let device = self
            .devices
            .iter()
            .find(|device| {
                device.scroll
                    && device.resumed
                    && self.region_for_mapping(device.ptr, mapping_id).is_some()
            })
            .ok_or("EIS has no scroll device for the selected display")?;
        let (x, y) = match direction {
            "up" => (0, -amount * 120),
            "down" => (0, amount * 120),
            "left" => (-amount * 120, 0),
            "right" => (amount * 120, 0),
            _ => return Err("invalid scroll direction".into()),
        };
        unsafe {
            (self.api.scroll_discrete)(device.ptr.as_ptr(), x, y);
            (self.api.scroll_stop)(device.ptr.as_ptr(), x != 0, y != 0);
            (self.api.frame)(device.ptr.as_ptr(), (self.api.now)(self.context.as_ptr()));
        }
        Ok(())
    }

    pub fn scroll_smooth(&mut self, x: f64, y: f64) -> Result<(), String> {
        self.dispatch()?;
        let scroll_delta = self
            .api
            .scroll_delta
            .ok_or("libei does not support smooth scrolling")?;
        let mapping_id = self
            .pointer_mapping
            .as_deref()
            .ok_or("EIS pointer has no selected display mapping")?;
        let device = self
            .devices
            .iter()
            .find(|device| {
                device.scroll
                    && device.resumed
                    && self.region_for_mapping(device.ptr, mapping_id).is_some()
            })
            .ok_or("EIS has no scroll device for the selected display")?;
        unsafe {
            scroll_delta(device.ptr.as_ptr(), x, y);
            (self.api.scroll_stop)(device.ptr.as_ptr(), x != 0.0, y != 0.0);
            (self.api.frame)(device.ptr.as_ptr(), (self.api.now)(self.context.as_ptr()));
        }
        Ok(())
    }

    pub fn key(&mut self, keysym: u32, pressed: bool) -> Result<(), String> {
        self.dispatch()?;
        let device = self
            .devices
            .iter()
            .find(|device| device.text && device.resumed)
            .ok_or("EIS has no active text device")?;
        unsafe {
            (self.api.text_keysym)(device.ptr.as_ptr(), keysym, pressed);
            (self.api.frame)(device.ptr.as_ptr(), (self.api.now)(self.context.as_ptr()));
        }
        Ok(())
    }

    pub fn text(&mut self, text: &str) -> Result<(), String> {
        self.dispatch()?;
        let device = self
            .devices
            .iter()
            .find(|device| device.text && device.resumed)
            .ok_or("EIS has no active text device")?;
        unsafe {
            (self.api.text_utf8)(device.ptr.as_ptr(), text.as_ptr().cast(), text.len());
            (self.api.frame)(device.ptr.as_ptr(), (self.api.now)(self.context.as_ptr()));
        }
        Ok(())
    }

    fn region_for_mapping(
        &self,
        device: NonNull<c_void>,
        mapping_id: &str,
    ) -> Option<NonNull<c_void>> {
        for index in 0..32 {
            let Some(region) =
                NonNull::new(unsafe { (self.api.device_region)(device.as_ptr(), index) })
            else {
                break;
            };
            let mapping = unsafe { (self.api.region_mapping_id)(region.as_ptr()) };
            if !mapping.is_null()
                && unsafe { CStr::from_ptr(mapping) }.to_bytes() == mapping_id.as_bytes()
            {
                return Some(region);
            }
        }
        None
    }

    fn region_local_coordinate(
        &self,
        region: NonNull<c_void>,
        coordinate: f64,
        source_extent: u32,
        horizontal: bool,
    ) -> Result<f64, String> {
        let (origin, extent) = unsafe {
            if horizontal {
                (
                    (self.api.region_x)(region.as_ptr()),
                    (self.api.region_width)(region.as_ptr()),
                )
            } else {
                (
                    (self.api.region_y)(region.as_ptr()),
                    (self.api.region_height)(region.as_ptr()),
                )
            }
        };
        region_coordinate(origin, extent, coordinate, source_extent)
    }

    fn wait(&self, duration: Duration) -> Result<(), String> {
        let fd = unsafe { (self.api.get_fd)(self.context.as_ptr()) };
        if fd < 0 {
            return Err("libei returned an invalid socket".into());
        }
        let mut descriptor = PollFd {
            fd,
            events: 1,
            revents: 0,
        };
        let result = unsafe { poll(&mut descriptor, 1, duration.as_millis() as c_int) };
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            Err("waiting for libei events failed".into())
        } else {
            Ok(())
        }
    }

    fn dispatch(&mut self) -> Result<(), String> {
        if unsafe { (self.api.dispatch)(self.context.as_ptr()) } < 0 {
            return Err("libei event dispatch failed".into());
        }
        while let Some(event) = NonNull::new(unsafe { (self.api.get_event)(self.context.as_ptr()) })
        {
            let event_result = (|| {
                match unsafe { (self.api.event_type)(event.as_ptr()) } {
                    1 => self.connected = true,
                    2 => return Err("the portal EIS connection was closed".into()),
                    3 => {
                        if let Some(seat) =
                            NonNull::new(unsafe { (self.api.event_seat)(event.as_ptr()) })
                        {
                            let seat = NonNull::new(unsafe { (self.api.seat_ref)(seat.as_ptr()) })
                                .ok_or("libei could not retain a seat")?;
                            self.seats.push(seat);
                            unsafe {
                                (self.api.seat_bind_capabilities)(
                                    seat.as_ptr(),
                                    POINTER_ABSOLUTE,
                                    SCROLL,
                                    BUTTON,
                                    TEXT,
                                    std::ptr::null::<c_void>(),
                                );
                            }
                        }
                    }
                    4 => {
                        if let Some(seat) =
                            NonNull::new(unsafe { (self.api.event_seat)(event.as_ptr()) })
                        {
                            if let Some(index) = self
                                .seats
                                .iter()
                                .position(|item| item.as_ptr() == seat.as_ptr())
                            {
                                unsafe {
                                    (self.api.seat_unref)(self.seats.swap_remove(index).as_ptr());
                                }
                            }
                        }
                    }
                    5 => {
                        if let Some(device) =
                            NonNull::new(unsafe { (self.api.event_device)(event.as_ptr()) })
                        {
                            let device =
                                NonNull::new(unsafe { (self.api.device_ref)(device.as_ptr()) })
                                    .ok_or("libei could not retain a device")?;
                            self.devices.push(self.describe_device(device));
                        }
                    }
                    6 => {
                        if let Some(device) =
                            NonNull::new(unsafe { (self.api.event_device)(event.as_ptr()) })
                        {
                            if let Some(index) = self
                                .devices
                                .iter()
                                .position(|item| item.ptr.as_ptr() == device.as_ptr())
                            {
                                unsafe {
                                    (self.api.device_unref)(
                                        self.devices.swap_remove(index).ptr.as_ptr(),
                                    );
                                }
                            }
                        }
                    }
                    7 => {
                        if let Some(device) =
                            NonNull::new(unsafe { (self.api.event_device)(event.as_ptr()) })
                        {
                            if let Some(item) = self
                                .devices
                                .iter_mut()
                                .find(|item| item.ptr.as_ptr() == device.as_ptr())
                            {
                                item.resumed = false;
                            }
                        }
                    }
                    8 => {
                        if let Some(device) =
                            NonNull::new(unsafe { (self.api.event_device)(event.as_ptr()) })
                        {
                            if let Some(item) = self
                                .devices
                                .iter_mut()
                                .find(|item| item.ptr.as_ptr() == device.as_ptr())
                            {
                                self.sequence = self.sequence.wrapping_add(1).max(1);
                                unsafe {
                                    (self.api.start_emulating)(device.as_ptr(), self.sequence);
                                }
                                item.resumed = true;
                            }
                        }
                    }
                    _ => {}
                }
                Ok::<(), String>(())
            })();
            unsafe {
                (self.api.event_unref)(event.as_ptr());
            }
            event_result?;
        }
        Ok(())
    }

    fn describe_device(&self, ptr: NonNull<c_void>) -> Device {
        let has =
            |capability| unsafe { (self.api.device_has_capability)(ptr.as_ptr(), capability) };
        Device {
            ptr,
            absolute_pointer: has(POINTER_ABSOLUTE),
            button: has(BUTTON),
            scroll: has(SCROLL),
            text: has(TEXT),
            resumed: false,
        }
    }
}

fn region_coordinate(
    origin: u32,
    extent: u32,
    coordinate: f64,
    source_extent: u32,
) -> Result<f64, String> {
    if extent == 0
        || source_extent == 0
        || !coordinate.is_finite()
        || coordinate < 0.0
        || coordinate >= f64::from(source_extent)
    {
        return Err("pointer coordinate is outside the captured display region".into());
    }
    Ok(f64::from(origin) + coordinate * f64::from(extent) / f64::from(source_extent))
}

impl Drop for EisInput {
    fn drop(&mut self) {
        for device in self.devices.drain(..) {
            unsafe {
                (self.api.device_unref)(device.ptr.as_ptr());
            }
        }
        for seat in self.seats.drain(..) {
            unsafe {
                (self.api.seat_unref)(seat.as_ptr());
            }
        }
        unsafe {
            (self.api.unref)(self.context.as_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::region_coordinate;

    #[test]
    fn maps_captured_coordinates_to_scaled_eis_regions_with_offsets() {
        assert_eq!(region_coordinate(1920, 2560, 640.0, 1280).unwrap(), 3200.0);
    }

    #[test]
    fn rejects_coordinates_outside_empty_or_non_finite_regions() {
        assert!(region_coordinate(0, 100, 100.0, 100).is_err());
        assert!(region_coordinate(0, 0, 1.0, 100).is_err());
        assert!(region_coordinate(0, 100, 1.0, 0).is_err());
        assert!(region_coordinate(0, 100, f64::NAN, 100).is_err());
    }
}
