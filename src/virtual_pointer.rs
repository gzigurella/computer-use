use std::os::raw::{c_int, c_long};

use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    protocol::{wl_pointer, wl_registry, wl_registry::WlRegistry},
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

#[repr(C)]
struct Timespec {
    seconds: c_long,
    nanoseconds: c_long,
}

#[repr(C)]
pub(crate) struct PollFd {
    pub(crate) fd: c_int,
    pub(crate) events: i16,
    pub(crate) revents: i16,
}

#[link(name = "c")]
unsafe extern "C" {
    fn clock_gettime(clock_id: c_int, time: *mut Timespec) -> c_int;
    pub(crate) fn poll(fds: *mut PollFd, nfds: usize, timeout: c_int) -> c_int;
}

#[derive(Default)]
struct State {
    manager: Option<ZwlrVirtualPointerManagerV1>,
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
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
            && interface == "zwlr_virtual_pointer_manager_v1"
            && state.manager.is_none()
        {
            state.manager = Some(registry.bind(name, version.min(2), qh, ()))
        }
    }
}

delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore ZwlrVirtualPointerV1);

pub fn probe() -> Result<(), String> {
    VirtualPointer::connect().map(drop)
}

pub fn display_probe() -> Result<(), String> {
    Connection::connect_to_env()
        .map(|_| ())
        .map_err(|error| format!("could not connect to the active Wayland display: {error}"))
}

pub struct VirtualPointer {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    pointer: ZwlrVirtualPointerV1,
}

unsafe impl Send for VirtualPointer {}

impl VirtualPointer {
    pub fn connect() -> Result<Self, String> {
        let connection = Connection::connect_to_env()
            .map_err(|error| format!("could not connect to the active Wayland display: {error}"))?;
        let mut queue = connection.new_event_queue();
        let qh = queue.handle();
        let registry = connection.display().get_registry(&qh, ());
        let mut state = State::default();
        queue
            .roundtrip(&mut state)
            .map_err(|error| format!("failed to discover Wayland globals: {error}"))?;
        let manager = state
            .manager
            .take()
            .ok_or("the compositor does not expose wlr virtual-pointer")?;
        let pointer = manager.create_virtual_pointer(None, &qh, ());
        connection
            .flush()
            .map_err(|error| format!("could not send the virtual-pointer request: {error}"))?;
        drop(registry);
        Ok(Self {
            connection,
            queue,
            state,
            pointer,
        })
    }

    pub fn pointer(&mut self, x: f64, y: f64, width: u32, height: u32) -> Result<(), String> {
        if width == 0
            || height == 0
            || x < 0.0
            || y < 0.0
            || x >= width as f64
            || y >= height as f64
        {
            return Err("pointer coordinate is outside the virtual display".into());
        }
        let x = x.round().min(width.saturating_sub(1) as f64) as u32;
        let y = y.round().min(height.saturating_sub(1) as f64) as u32;
        self.pointer
            .motion_absolute(self.time(), x, y, width, height);
        self.pointer.frame();
        self.flush()
    }

    pub fn button(&mut self, button: u32, pressed: bool) -> Result<(), String> {
        self.pointer.button(
            self.time(),
            button,
            if pressed {
                wl_pointer::ButtonState::Pressed
            } else {
                wl_pointer::ButtonState::Released
            },
        );
        self.pointer.frame();
        self.flush()
    }

    pub fn scroll(&mut self, direction: &str, amount: i32) -> Result<(), String> {
        let (axis, sign) = match direction {
            "up" => (wl_pointer::Axis::VerticalScroll, -1i32),
            "down" => (wl_pointer::Axis::VerticalScroll, 1),
            "left" => (wl_pointer::Axis::HorizontalScroll, -1),
            "right" => (wl_pointer::Axis::HorizontalScroll, 1),
            _ => return Err("invalid scroll direction".into()),
        };
        let steps = amount
            .checked_mul(sign)
            .ok_or("scroll amount is out of range")?;
        let value = f64::from(steps) * 10.0;
        let time = self.time();
        self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
        self.pointer.axis(time, axis, value);
        self.pointer.axis_discrete(time, axis, value, steps);
        self.pointer.frame();
        self.flush()
    }

    pub fn scroll_smooth(&mut self, x: i32, y: i32) -> Result<(), String> {
        let x = f64::from(x);
        let y = f64::from(y);
        let time = self.time();
        self.pointer.axis_source(wl_pointer::AxisSource::Continuous);
        if x != 0.0 {
            self.pointer
                .axis(time, wl_pointer::Axis::HorizontalScroll, x);
        }
        if y != 0.0 {
            self.pointer.axis(time, wl_pointer::Axis::VerticalScroll, y);
        }
        self.pointer.frame();
        if x != 0.0 {
            self.pointer
                .axis_stop(time, wl_pointer::Axis::HorizontalScroll);
        }
        if y != 0.0 {
            self.pointer
                .axis_stop(time, wl_pointer::Axis::VerticalScroll);
        }
        if x != 0.0 || y != 0.0 {
            self.pointer.frame();
        }
        self.flush()
    }

    fn time(&self) -> u32 {
        let mut time = Timespec {
            seconds: 0,
            nanoseconds: 0,
        };
        if unsafe { clock_gettime(1, &mut time) } == 0 {
            ((time.seconds as u64)
                .wrapping_mul(1000)
                .wrapping_add(time.nanoseconds as u64 / 1_000_000) as u32)
                .max(1)
        } else {
            1
        }
    }

    fn flush(&mut self) -> Result<(), String> {
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|error| format!("dispatching virtual-pointer events failed: {error}"))?;
        self.connection
            .flush()
            .map_err(|error| format!("sending virtual-pointer input to Wayland failed: {error}"))
    }
}

impl Drop for VirtualPointer {
    fn drop(&mut self) {
        self.pointer.destroy();
        let _ = self.connection.flush();
    }
}
