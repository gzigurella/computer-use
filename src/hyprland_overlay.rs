use serde_json::Value;
use std::{
    ffi::{CString, c_int},
    io::{BufRead, Read, Write},
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    os::unix::net::UnixStream,
    os::unix::process::CommandExt,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    protocol::{
        wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_output, wl_output::WlOutput,
        wl_region::WlRegion, wl_registry, wl_registry::WlRegistry, wl_shm, wl_shm::WlShm,
        wl_shm_pool::WlShmPool, wl_surface::WlSurface,
    },
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

const POINTER_SCALE: f64 = 0.32;
const CURSOR_WIDTH: u32 = 12;
const CURSOR_HEIGHT: u32 = 12;
const HOTSPOT_X: i32 = 1;
const HOTSPOT_Y: i32 = 1;
const PR_SET_PDEATHSIG: c_int = 1;
const SIGTERM: c_int = 15;
const SIG_IGN: usize = 1;
static PARENT_TERMINATED: AtomicBool = AtomicBool::new(false);

extern "C" fn overlay_signal_handler(_: c_int) {
    PARENT_TERMINATED.store(true, Ordering::Release);
}

struct ParentDeathCursorRestore {
    parent_pid: u32,
    previous_visibility: Option<bool>,
}

struct OverlayCursorVisibility {
    previous: Option<bool>,
    current: Option<bool>,
}

impl OverlayCursorVisibility {
    fn new(previous: Option<bool>) -> Self {
        Self {
            previous,
            current: previous,
        }
    }

    fn update(&mut self, _pointer_on_overlay_monitor: bool) {
        // The AI overlay may coexist with the user's native cursor; never hide it.
    }
}

impl Drop for OverlayCursorVisibility {
    fn drop(&mut self) {
        if self.previous == Some(false)
            && self.current == Some(true)
            && let Err(error) = crate::set_hyprland_cursor_invisible(false)
        {
            eprintln!(
                "computer-use-linux: overlay could not restore Hyprland cursor visibility: {error}"
            );
        }
    }
}

fn parent_process_has_exited(parent_pid: u32) -> bool {
    if unsafe { getppid() } as u32 != parent_pid {
        return true;
    }
    std::fs::read_to_string(format!("/proc/{parent_pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .map(|(_, fields)| fields.trim_start().to_owned())
        })
        .and_then(|fields| fields.chars().next())
        .is_some_and(|state| matches!(state, 'Z' | 'X'))
}

impl Drop for ParentDeathCursorRestore {
    fn drop(&mut self) {
        let parent_died = parent_process_has_exited(self.parent_pid);
        if parent_died
            && self.previous_visibility == Some(false)
            && let Err(error) = crate::set_hyprland_cursor_invisible(false)
        {
            eprintln!(
                "computer-use-linux: overlay could not restore cursor visibility after server exit: {error}"
            );
        }
    }
}
const POINTER_SHAPE: [(f64, f64); 24] = [
    (3.8, 1.5),
    (2.8, 1.5),
    (2.0, 1.9),
    (1.5, 2.6),
    (1.4, 3.5),
    (1.7, 4.5),
    (9.7, 27.7),
    (10.2, 28.7),
    (11.0, 29.5),
    (12.0, 29.9),
    (13.0, 29.6),
    (13.6, 28.8),
    (18.1, 19.4),
    (18.8, 18.5),
    (19.8, 18.0),
    (28.5, 15.0),
    (29.4, 14.5),
    (29.9, 13.7),
    (29.8, 12.7),
    (29.2, 11.8),
    (28.3, 11.3),
    (4.4, 1.7),
    (4.1, 1.6),
    (3.9, 1.5),
];

pub fn available() -> bool {
    let Ok(executable) = std::env::current_exe() else {
        return false;
    };
    let Ok(mut child) = Command::new(executable)
        .arg("--probe-pointer-overlay")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

pub fn probe_wayland() -> Result<(), String> {
    let connection =
        Connection::connect_to_env().map_err(|error| format!("connecting to Wayland: {error}"))?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let _registry = connection.display().get_registry(&qh, ());
    let mut state = Globals::default();
    queue
        .roundtrip(&mut state)
        .map_err(|error| format!("discovering Wayland globals: {error}"))?;
    queue
        .roundtrip(&mut state)
        .map_err(|error| format!("reading Wayland output names: {error}"))?;
    if state.compositor.is_none()
        || state.shm.is_none()
        || state.layer_shell.is_none()
        || state.viewporter.is_none()
    {
        return Err(
            "Hyprland does not expose compositor, shared-memory, layer-shell, or viewporter protocols".into(),
        );
    }
    Ok(())
}

pub fn select_monitor(requested: Option<&str>) -> Result<String, String> {
    choose_monitor(&active_monitors()?, requested)
}

fn choose_monitor(monitors: &[Value], requested: Option<&str>) -> Result<String, String> {
    let names = monitors
        .iter()
        .filter_map(|monitor| monitor["name"].as_str())
        .collect::<Vec<_>>();
    match requested {
        Some(name) if names.contains(&name) => Ok(name.to_owned()),
        Some(name) => Err(format!(
            "overlay_monitor {name:?} is not active; available outputs: {}",
            names.join(", ")
        )),
        None if monitors.len() == 1 => names
            .first()
            .map(|name| (*name).to_owned())
            .ok_or_else(|| "the active Hyprland monitor has no usable output name".into()),
        None if monitors.is_empty() => {
            Err("Hyprland has no active monitors for the pointer overlay".into())
        }
        None => Err(format!(
            "Hyprland has multiple active monitors; ask the user which one to use, then pass overlay_monitor. Available outputs: {}",
            names.join(", ")
        )),
    }
}

fn active_monitors() -> Result<Vec<Value>, String> {
    let output = crate::hyprland_json("monitors", 1024 * 1024, std::time::Duration::from_secs(2))?;
    let monitors: Value = serde_json::from_slice(&output)
        .map_err(|error| format!("invalid Hyprland monitor list: {error}"))?;
    monitors
        .as_array()
        .ok_or_else(|| "Hyprland monitor list is not an array".to_owned())
        .map(|monitors| {
            monitors
                .iter()
                .filter(|monitor| monitor["disabled"] != true)
                .cloned()
                .collect()
        })
}

pub fn monitor_choices() -> Result<Vec<Value>, String> {
    active_monitors()?
        .into_iter()
        .map(|monitor| {
            let name = monitor["name"]
                .as_str()
                .ok_or_else(|| "active Hyprland monitor omitted its output name".to_owned())?;
            let transform = monitor["transform"].as_u64().unwrap_or(0);
            let logical_size = crate::hyprland_monitor_logical_size(&monitor)
                .ok()
                .map(|(width, height, _)| (width, height));
            Ok(serde_json::json!({
                "name": name,
                "description": monitor["description"],
                "x": monitor["x"],
                "y": monitor["y"],
                "width": monitor["width"],
                "height": monitor["height"],
                "scale": monitor["scale"],
                "logical_width": logical_size.map(|(width, _)| width),
                "logical_height": logical_size.map(|(_, height)| height),
                "transform": transform,
                "focused": monitor["focused"]
            }))
        })
        .collect()
}

fn monitor_geometry_and_scale(name: &str) -> Result<((i32, i32, u32, u32), f64), String> {
    let monitor = active_monitors()?
        .into_iter()
        .find(|monitor| monitor["name"] == name)
        .ok_or_else(|| format!("Hyprland output {name:?} is no longer active"))?;
    let x = monitor["x"]
        .as_i64()
        .ok_or("monitor omitted its x position")?;
    let y = monitor["y"]
        .as_i64()
        .ok_or("monitor omitted its y position")?;
    let (width, height, _) = crate::hyprland_monitor_logical_size(&monitor)?;
    let scale = monitor["scale"]
        .as_f64()
        .filter(|scale| scale.is_finite() && *scale > 0.0)
        .ok_or_else(|| format!("Hyprland output {name:?} has no valid scale"))?;
    Ok((
        (
            i32::try_from(x).map_err(|_| "monitor x position is out of range")?,
            i32::try_from(y).map_err(|_| "monitor y position is out of range")?,
            width,
            height,
        ),
        scale,
    ))
}

fn logical_cursor_size(scale: f64) -> Result<(u32, u32), String> {
    if !scale.is_finite() || scale <= 0.0 {
        return Err("Hyprland output scale must be a positive finite number".into());
    }
    let dimension = |pixels: u32| -> Result<u32, String> {
        let logical = (f64::from(pixels) / scale).round().max(1.0);
        if logical > f64::from(i32::MAX) {
            return Err("scaled pointer dimensions are out of range".into());
        }
        Ok(logical as u32)
    };
    Ok((dimension(CURSOR_WIDTH)?, dimension(CURSOR_HEIGHT)?))
}

fn cursor_is_on_monitor(
    x: i32,
    y: i32,
    (origin_x, origin_y, width, height): (i32, i32, u32, u32),
) -> bool {
    let x = i64::from(x);
    let y = i64::from(y);
    let origin_x = i64::from(origin_x);
    let origin_y = i64::from(origin_y);
    x >= origin_x
        && y >= origin_y
        && x < origin_x + i64::from(width)
        && y < origin_y + i64::from(height)
}

fn offscreen_position((_, _, width, height): (i32, i32, u32, u32)) -> (i32, i32) {
    (
        i32::try_from(width)
            .unwrap_or(i32::MAX)
            .saturating_add(CURSOR_WIDTH as i32),
        i32::try_from(height)
            .unwrap_or(i32::MAX)
            .saturating_add(CURSOR_HEIGHT as i32),
    )
}

fn current_cursor_invisible() -> Result<bool, String> {
    let response = crate::hyprland_json(
        "getoption cursor:invisible",
        64 * 1024,
        Duration::from_secs(2),
    )?;
    let options: Value = serde_json::from_slice(&response)
        .map_err(|error| format!("reading Hyprland cursor visibility: {error}"))?;
    options["bool"]
        .as_bool()
        .ok_or_else(|| "Hyprland omitted cursor:invisible state".into())
}

pub struct PointerOverlay {
    child: Child,
    input: Option<ChildStdin>,
    native_cursor: Option<NativeCursorVisibility>,
}

struct NativeCursorVisibility {
    previous: bool,
}

impl NativeCursorVisibility {
    fn capture() -> Result<Self, String> {
        Ok(Self {
            previous: current_cursor_invisible()?,
        })
    }

    fn restore(&mut self) -> Result<(), String> {
        if !self.previous && current_cursor_invisible()? {
            crate::set_hyprland_cursor_invisible(false)?;
        }
        Ok(())
    }
}

fn abort_overlay_startup(child: &mut Child, restore_cursor: Option<bool>) {
    let _ = child.kill();
    let _ = child.wait();
    if restore_cursor == Some(false)
        && current_cursor_invisible().unwrap_or(false)
        && let Err(error) = crate::set_hyprland_cursor_invisible(false)
    {
        eprintln!(
            "computer-use-linux: failed to restore Hyprland cursor after overlay startup failure: {error}"
        );
    }
}

impl Drop for NativeCursorVisibility {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("computer-use-linux: failed to restore Hyprland cursor visibility: {error}");
        }
    }
}

impl PointerOverlay {
    pub fn native_cursor_hidden(&self) -> bool {
        current_cursor_invisible().unwrap_or(false)
    }

    pub fn start(monitor: &str) -> Result<Self, String> {
        let restore_cursor = match NativeCursorVisibility::capture() {
            Ok(visibility) => Some(visibility.previous),
            Err(error) => {
                eprintln!("computer-use-linux: keeping the native cursor visible: {error}");
                None
            }
        };
        Self::start_with_visibility(monitor, restore_cursor, true)
    }

    fn start_with_visibility(
        monitor: &str,
        restore_cursor: Option<bool>,
        track_visibility: bool,
    ) -> Result<Self, String> {
        let parent_pid = std::process::id();
        let mut command = Command::new(
            std::env::current_exe()
                .map_err(|error| format!("finding computer-use executable: {error}"))?,
        );
        command
            .args(["--pointer-overlay", monitor])
            .arg(parent_pid.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(previous) = restore_cursor {
            command.arg(if previous { "true" } else { "false" });
        }
        // The overlay must not survive an abruptly terminated MCP server.
        unsafe {
            command.pre_exec(move || {
                // Preserve the child long enough to install its SIGTERM
                // handler and restore the native cursor if this parent dies.
                if signal(SIGTERM, SIG_IGN) == usize::MAX {
                    return Err(std::io::Error::last_os_error());
                }
                if prctl(PR_SET_PDEATHSIG, SIGTERM as usize, 0usize, 0usize, 0usize) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if getppid() as u32 != parent_pid {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "overlay parent exited during startup",
                    ));
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("starting native Hyprland pointer overlay: {error}"))?;
        let Some(stdout) = child.stdout.take() else {
            abort_overlay_startup(&mut child, restore_cursor);
            return Err("pointer overlay has no stdout".into());
        };
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut line = String::new();
            let result = std::io::BufReader::new(stdout)
                .read_line(&mut line)
                .map(|_| line);
            let _ = ready_tx.send(result);
        });
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(line))
                if serde_json::from_str::<Value>(&line)
                    .is_ok_and(|value| value["ready"] == true) => {}
            Ok(Ok(_)) => {
                abort_overlay_startup(&mut child, restore_cursor);
                return Err("native Hyprland pointer overlay exited before becoming ready".into());
            }
            Ok(Err(error)) => {
                abort_overlay_startup(&mut child, restore_cursor);
                return Err(format!("reading Hyprland pointer overlay startup: {error}"));
            }
            Err(_) => {
                abort_overlay_startup(&mut child, restore_cursor);
                return Err(
                    "native Hyprland pointer overlay did not become ready within 5 seconds".into(),
                );
            }
        }
        let Some(input) = child.stdin.take() else {
            abort_overlay_startup(&mut child, restore_cursor);
            return Err("pointer overlay has no stdin".into());
        };
        Ok(Self {
            child,
            input: Some(input),
            native_cursor: if track_visibility {
                restore_cursor.map(|previous| NativeCursorVisibility { previous })
            } else {
                None
            },
        })
    }

    pub fn stop(&mut self) -> Result<(), String> {
        self.input.take();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    if let Some(mut visibility) = self.native_cursor.take() {
                        visibility.restore()?;
                    }
                    return Ok(());
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Ok(None) => {
                    self.child
                        .kill()
                        .map_err(|error| format!("stopping pointer overlay: {error}"))?;
                    self.child
                        .wait()
                        .map_err(|error| format!("reaping pointer overlay: {error}"))?;
                    if let Some(mut visibility) = self.native_cursor.take() {
                        visibility.restore()?;
                    }
                    return Ok(());
                }
                Err(error) => return Err(format!("checking pointer overlay process: {error}")),
            }
        }
    }

    pub fn is_running(&mut self) -> Result<bool, String> {
        self.child
            .try_wait()
            .map(|status| status.is_none())
            .map_err(|error| format!("checking pointer overlay process: {error}"))
    }

    pub fn ensure_running(&mut self, monitor: &str) -> Result<bool, String> {
        if self.is_running()? {
            return Ok(false);
        }
        let restore_cursor = self
            .native_cursor
            .as_ref()
            .map(|visibility| visibility.previous);
        if let Some(visibility) = self.native_cursor.as_mut() {
            visibility.restore()?;
        }
        let mut replacement = Self::start_with_visibility(monitor, restore_cursor, false)?;
        replacement.native_cursor = self.native_cursor.take();
        drop(std::mem::replace(self, replacement));
        Ok(true)
    }
}

impl Drop for PointerOverlay {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[derive(Default)]
struct Globals {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    viewporter: Option<WpViewporter>,
    outputs: Vec<(Arc<Mutex<String>>, WlOutput)>,
}

impl Dispatch<WlRegistry, ()> for Globals {
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
        {
            match interface.as_str() {
                "wl_compositor" if state.compositor.is_none() => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_shm" if state.shm.is_none() => state.shm = Some(registry.bind(name, 1, qh, ())),
                "wl_output" => {
                    let output_name = Arc::new(Mutex::new(String::new()));
                    let output = registry.bind(name, version.min(4), qh, output_name.clone());
                    state.outputs.push((output_name, output));
                }
                "zwlr_layer_shell_v1" if state.layer_shell.is_none() => {
                    state.layer_shell = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wp_viewporter" if state.viewporter.is_none() => {
                    state.viewporter = Some(registry.bind(name, version.min(1), qh, ()))
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(Globals: ignore WlCompositor);
delegate_noop!(Globals: ignore WlShm);
delegate_noop!(Globals: ignore WlOutput);

impl Dispatch<WlOutput, Arc<Mutex<String>>> for Globals {
    fn event(
        _: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        name: &Arc<Mutex<String>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name: output_name } = event {
            if let Ok(mut name) = name.lock() {
                *name = output_name;
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, Arc<AtomicBool>> for Globals {
    fn event(
        _: &mut Self,
        proxy: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        configured: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
            proxy.ack_configure(serial);
            configured.store(true, Ordering::Release);
        }
    }
}

delegate_noop!(Globals: ignore WlSurface);
delegate_noop!(Globals: ignore WlRegion);
delegate_noop!(Globals: ignore WlShmPool);
delegate_noop!(Globals: ignore WlBuffer);
delegate_noop!(Globals: ignore ZwlrLayerShellV1);
delegate_noop!(Globals: ignore WpViewporter);
delegate_noop!(Globals: ignore WpViewport);

struct BufferMemory {
    fd: OwnedFd,
    address: *mut u8,
    size: usize,
}
unsafe impl Send for BufferMemory {}

impl BufferMemory {
    fn new(size: usize) -> Result<Self, String> {
        let name = CString::new("computer-use-pointer").expect("static name has no NUL");
        let raw_fd = unsafe { memfd_create(name.as_ptr(), 1) };
        if raw_fd < 0 {
            return Err(format!(
                "creating Wayland pointer buffer: {}",
                std::io::Error::last_os_error()
            ));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        if unsafe { ftruncate(fd.as_raw_fd(), size as i64) } < 0 {
            return Err(format!(
                "sizing Wayland pointer buffer: {}",
                std::io::Error::last_os_error()
            ));
        }
        let address = unsafe { mmap(std::ptr::null_mut(), size, 3, 1, fd.as_raw_fd(), 0) };
        if address as isize == -1 {
            return Err(format!(
                "mapping Wayland pointer buffer: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self {
            fd,
            address: address.cast(),
            size,
        })
    }

    fn paint(&mut self) {
        let bytes = unsafe { std::slice::from_raw_parts_mut(self.address, self.size) };
        paint_pointer(bytes);
    }
}

impl Drop for BufferMemory {
    fn drop(&mut self) {
        unsafe {
            munmap(self.address.cast(), self.size);
        }
    }
}

fn paint_pointer(bytes: &mut [u8]) {
    assert_eq!(bytes.len(), (CURSOR_WIDTH * CURSOR_HEIGHT * 4) as usize);
    bytes.fill(0);
    for y in 0..CURSOR_HEIGHT as i32 {
        for x in 0..CURSOR_WIDTH as i32 {
            let index = ((y as usize * CURSOR_WIDTH as usize) + x as usize) * 4;
            let mut rgba = [0.0_f64; 4];
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = x as f64 + (sx as f64 + 0.5) / 4.0;
                    let py = y as f64 + (sy as f64 + 0.5) / 4.0;
                    let distance = pointer_edge_distance(px, py);
                    let mut sample = [0.0_f64; 4];
                    let shadow_distance =
                        pointer_edge_distance(px - 0.8 * POINTER_SCALE, py - 0.8 * POINTER_SCALE);
                    if shadow_distance < 3.0 * POINTER_SCALE {
                        let alpha = 0.2 * (1.0 - shadow_distance / (3.0 * POINTER_SCALE));
                        sample = [0.65 * alpha, 0.43 * alpha, 0.2 * alpha, alpha];
                    }
                    if distance <= 1.45 * POINTER_SCALE || inside_pointer(px, py) {
                        let (red, green, blue, alpha) = if distance <= 1.45 * POINTER_SCALE {
                            (0.96, 0.985, 1.0, 0.98)
                        } else {
                            (0.53, 0.83, 1.0, 0.66)
                        };
                        let alpha_inverse = 1.0 - alpha;
                        sample[0] = blue * alpha + sample[0] * alpha_inverse;
                        sample[1] = green * alpha + sample[1] * alpha_inverse;
                        sample[2] = red * alpha + sample[2] * alpha_inverse;
                        sample[3] = alpha + sample[3] * alpha_inverse;
                    }
                    for channel in 0..4 {
                        rgba[channel] += sample[channel];
                    }
                }
            }
            for channel in &mut rgba {
                *channel /= 16.0;
            }
            bytes[index] = (rgba[0] * 255.0).round() as u8;
            bytes[index + 1] = (rgba[1] * 255.0).round() as u8;
            bytes[index + 2] = (rgba[2] * 255.0).round() as u8;
            bytes[index + 3] = (rgba[3] * 255.0).round() as u8;
        }
    }
}

pub(crate) fn pointer_image() -> (u32, u32, i32, i32, Vec<u8>) {
    let mut pixels = vec![0; (CURSOR_WIDTH * CURSOR_HEIGHT * 4) as usize];
    paint_pointer(&mut pixels);
    (CURSOR_WIDTH, CURSOR_HEIGHT, HOTSPOT_X, HOTSPOT_Y, pixels)
}

fn inside_pointer(x: f64, y: f64) -> bool {
    let x = x / POINTER_SCALE;
    let y = y / POINTER_SCALE;
    let mut inside = false;
    let mut previous = POINTER_SHAPE.len() - 1;
    for current in 0..POINTER_SHAPE.len() {
        let (x1, y1) = POINTER_SHAPE[current];
        let (x2, y2) = POINTER_SHAPE[previous];
        if (y1 > y) != (y2 > y) && x < (x2 - x1) * (y - y1) / (y2 - y1) + x1 {
            inside = !inside;
        }
        previous = current;
    }
    inside
}

fn pointer_edge_distance(x: f64, y: f64) -> f64 {
    (0..POINTER_SHAPE.len())
        .map(|index| {
            let (x1, y1) = POINTER_SHAPE[index];
            let (x2, y2) = POINTER_SHAPE[(index + 1) % POINTER_SHAPE.len()];
            segment_distance(x / POINTER_SCALE, y / POINTER_SCALE, x1, y1, x2, y2) * POINTER_SCALE
        })
        .fold(f64::INFINITY, f64::min)
}

fn segment_distance(px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let dx = bx - ax;
    let dy = by - ay;
    let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((px - (ax + t * dx)).powi(2) + (py - (ay + t * dy)).powi(2)).sqrt()
}

pub fn run_pointer_overlay(
    monitor: &str,
    parent_pid: u32,
    restore_cursor: Option<bool>,
) -> Result<(), String> {
    PARENT_TERMINATED.store(false, Ordering::Release);
    if unsafe { signal(SIGTERM, overlay_signal_handler as *const () as usize) } == usize::MAX {
        return Err(format!(
            "installing pointer overlay parent-death handler: {}",
            std::io::Error::last_os_error()
        ));
    }
    let _parent_death_restore = ParentDeathCursorRestore {
        parent_pid,
        previous_visibility: restore_cursor,
    };
    let mut native_visibility = OverlayCursorVisibility::new(restore_cursor);
    if unsafe { getppid() } as u32 != parent_pid {
        return Err("pointer overlay parent exited during startup".into());
    }
    let (initial_geometry, mut output_scale) = monitor_geometry_and_scale(monitor)?;
    let mut geometry = Some(initial_geometry);
    let mut last_geometry = initial_geometry;
    let connection =
        Connection::connect_to_env().map_err(|error| format!("connecting to Wayland: {error}"))?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let registry = connection.display().get_registry(&qh, ());
    let mut globals = Globals::default();
    queue
        .roundtrip(&mut globals)
        .map_err(|error| format!("discovering Wayland globals: {error}"))?;
    queue
        .roundtrip(&mut globals)
        .map_err(|error| format!("reading Wayland output names: {error}"))?;
    let compositor = globals
        .compositor
        .clone()
        .ok_or("Wayland compositor protocol is unavailable")?;
    let shm = globals
        .shm
        .clone()
        .ok_or("Wayland shared-memory protocol is unavailable")?;
    let layer_shell = globals
        .layer_shell
        .clone()
        .ok_or("Hyprland layer-shell protocol is unavailable")?;
    let viewporter = globals
        .viewporter
        .clone()
        .ok_or("Wayland viewporter protocol is unavailable")?;
    let output = globals
        .outputs
        .iter()
        .find(|(name, _)| name.lock().is_ok_and(|name| name.as_str() == monitor))
        .map(|(_, output)| output.clone())
        .ok_or_else(|| format!("Wayland output {monitor:?} was not advertised"))?;

    let surface = compositor.create_surface(&qh, ());
    let viewport: WpViewport = viewporter.get_viewport(&surface, &qh, ());
    let (logical_width, logical_height) = logical_cursor_size(output_scale)?;
    viewport.set_destination(logical_width as i32, logical_height as i32);
    let configured = Arc::new(AtomicBool::new(false));
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        Some(&output),
        Layer::Overlay,
        "computer-use-pointer".into(),
        &qh,
        configured.clone(),
    );
    layer_surface.set_size(logical_width, logical_height);
    layer_surface.set_anchor(Anchor::Top | Anchor::Left);
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
    let input_region = compositor.create_region(&qh, ());
    surface.set_input_region(Some(&input_region));
    input_region.destroy();
    surface.commit();
    queue
        .roundtrip(&mut globals)
        .map_err(|error| format!("waiting for Hyprland layer configuration: {error}"))?;
    if !configured.load(Ordering::Acquire) {
        return Err("Hyprland did not configure the pointer overlay".into());
    }
    let initial_cursor = hyprland_cursor_position()
        .ok_or("Hyprland cursor position is unavailable while starting the pointer overlay")?;
    let mut overlay_on_monitor =
        cursor_is_on_monitor(initial_cursor.0, initial_cursor.1, initial_geometry);
    native_visibility.update(overlay_on_monitor);
    let initial_position = if overlay_on_monitor {
        (
            initial_cursor
                .0
                .saturating_sub(initial_geometry.0)
                .saturating_sub(HOTSPOT_X),
            initial_cursor
                .1
                .saturating_sub(initial_geometry.1)
                .saturating_sub(HOTSPOT_Y),
        )
    } else {
        offscreen_position(initial_geometry)
    };
    layer_surface.set_margin(initial_position.1, 0, 0, initial_position.0);

    let size = (CURSOR_WIDTH * CURSOR_HEIGHT * 4) as usize;
    let mut memory = BufferMemory::new(size)?;
    memory.paint();
    let pool = shm.create_pool(memory.fd.as_fd(), size as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        CURSOR_WIDTH as i32,
        CURSOR_HEIGHT as i32,
        (CURSOR_WIDTH * 4) as i32,
        wl_shm::Format::Argb8888,
        &qh,
        (),
    );
    surface.attach(Some(&buffer), 0, 0);
    surface.damage(0, 0, CURSOR_WIDTH as i32, CURSOR_HEIGHT as i32);
    surface.commit();
    connection
        .flush()
        .map_err(|error| format!("mapping Hyprland pointer overlay: {error}"))?;
    queue
        .roundtrip(&mut globals)
        .map_err(|error| format!("waiting for Hyprland to map the pointer overlay: {error}"))?;
    thread::sleep(Duration::from_millis(50));
    println!(
        "{{\"ready\":true,\"monitor\":{}}}",
        serde_json::to_string(monitor).unwrap_or_else(|_| "\"unknown\"".into())
    );
    let _ = std::io::stdout().flush();

    let stopping = Arc::new(AtomicBool::new(false));
    let input_stopping = stopping.clone();
    thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut byte = [0u8; 64];
        while stdin.read(&mut byte).is_ok_and(|count| count > 0) {}
        input_stopping.store(true, Ordering::Release);
    });

    let mut last_position = overlay_on_monitor.then_some(initial_position);
    let mut next_origin_refresh = Instant::now() + Duration::from_millis(250);
    while !stopping.load(Ordering::Acquire) && !PARENT_TERMINATED.load(Ordering::Acquire) {
        queue
            .dispatch_pending(&mut globals)
            .map_err(|error| format!("dispatching Wayland overlay events: {error}"))?;
        if Instant::now() >= next_origin_refresh {
            match monitor_geometry_and_scale(monitor) {
                Ok((new_geometry, scale)) => {
                    geometry = Some(new_geometry);
                    last_geometry = new_geometry;
                    if (scale - output_scale).abs() > f64::EPSILON {
                        output_scale = scale;
                        if let Ok((width, height)) = logical_cursor_size(output_scale) {
                            viewport.set_destination(width as i32, height as i32);
                            layer_surface.set_size(width, height);
                            surface.commit();
                        }
                    }
                }
                Err(_) => geometry = None,
            }
            next_origin_refresh = Instant::now() + Duration::from_millis(250);
        }
        if let (Some((x, y)), Some(geometry)) = (hyprland_cursor_position(), geometry) {
            let on_monitor = cursor_is_on_monitor(x, y, geometry);
            native_visibility.update(on_monitor);
            if on_monitor {
                let position = (
                    x.saturating_sub(geometry.0).saturating_sub(HOTSPOT_X),
                    y.saturating_sub(geometry.1).saturating_sub(HOTSPOT_Y),
                );
                if !overlay_on_monitor || last_position != Some(position) {
                    layer_surface.set_margin(position.1, 0, 0, position.0);
                    surface.commit();
                    overlay_on_monitor = true;
                    last_position = Some(position);
                }
            } else if overlay_on_monitor {
                let position = offscreen_position(geometry);
                layer_surface.set_margin(position.1, 0, 0, position.0);
                surface.commit();
                overlay_on_monitor = false;
                last_position = None;
            }
        } else {
            native_visibility.update(false);
            if overlay_on_monitor {
                let position = offscreen_position(last_geometry);
                layer_surface.set_margin(position.1, 0, 0, position.0);
                surface.commit();
                overlay_on_monitor = false;
                last_position = None;
            }
        }
        connection
            .flush()
            .map_err(|error| format!("updating Hyprland pointer overlay: {error}"))?;
        let Some(read_guard) = queue.prepare_read() else {
            continue;
        };
        let fd = connection.as_fd().as_raw_fd();
        let mut descriptor = crate::virtual_pointer::PollFd {
            fd,
            events: 1,
            revents: 0,
        };
        let ready = unsafe { crate::virtual_pointer::poll(&mut descriptor, 1, 8) };
        if ready > 0 {
            if descriptor.revents & 1 != 0 {
                read_guard
                    .read()
                    .map_err(|error| format!("reading Wayland overlay events: {error}"))?;
            } else if descriptor.revents & 0x38 != 0 {
                return Err("Wayland overlay connection was closed by the compositor".into());
            } else {
                drop(read_guard);
            }
        } else if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(format!("waiting for the Wayland overlay socket: {error}"));
            }
            drop(read_guard);
        } else {
            drop(read_guard);
        }
        let _ = (&registry, &memory, &pool, &buffer);
    }
    layer_surface.destroy();
    viewport.destroy();
    surface.destroy();
    connection
        .flush()
        .map_err(|error| format!("closing Wayland pointer overlay: {error}"))?;
    Ok(())
}

fn hyprland_cursor_position() -> Option<(i32, i32)> {
    let path = std::env::var_os("XDG_RUNTIME_DIR")?;
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let mut socket = UnixStream::connect(
        std::path::PathBuf::from(path)
            .join("hypr")
            .join(signature)
            .join(".socket.sock"),
    )
    .ok()?;
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .ok()?;
    socket.write_all(b"j/cursorpos").ok()?;
    let mut response = [0u8; 256];
    let count = socket.read(&mut response).ok()?;
    let value: Value = serde_json::from_slice(&response[..count]).ok()?;
    Some((
        i32::try_from(value["x"].as_i64()?).ok()?,
        i32::try_from(value["y"].as_i64()?).ok()?,
    ))
}

#[link(name = "c")]
unsafe extern "C" {
    fn prctl(option: c_int, ...) -> c_int;
    fn getppid() -> c_int;
    fn signal(signum: c_int, handler: usize) -> usize;
    fn memfd_create(name: *const std::ffi::c_char, flags: c_int) -> c_int;
    fn ftruncate(fd: c_int, length: i64) -> c_int;
    fn mmap(
        addr: *mut std::ffi::c_void,
        length: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_pointer_physical_size_stable_on_scaled_outputs() {
        assert_eq!(logical_cursor_size(1.0).unwrap(), (12, 12));
        assert_eq!(logical_cursor_size(1.25).unwrap(), (10, 10));
        assert_eq!(logical_cursor_size(1.5).unwrap(), (8, 8));
        assert_eq!(logical_cursor_size(2.0).unwrap(), (6, 6));
        assert!(logical_cursor_size(0.0).is_err());
        assert!(logical_cursor_size(f64::NAN).is_err());
    }

    #[test]
    fn chooses_the_only_monitor_and_requires_explicit_selection_for_multiple() {
        let monitors = [
            serde_json::json!({"name":"eDP-1"}),
            serde_json::json!({"name":"DP-1"}),
        ];
        assert_eq!(choose_monitor(&monitors[..1], None).unwrap(), "eDP-1");
        let error = choose_monitor(&monitors, None).unwrap_err();
        assert!(error.contains("ask the user which one to use"));
        assert!(error.contains("eDP-1, DP-1"));
        assert_eq!(choose_monitor(&monitors, Some("DP-1")).unwrap(), "DP-1");
        assert!(choose_monitor(&monitors, Some("HDMI-1")).is_err());
        assert!(choose_monitor(&[], None).is_err());
    }

    #[test]
    fn pointer_buffer_draws_a_translucent_mouse_arrow_at_its_hotspot() {
        let mut pixels = vec![0; (CURSOR_WIDTH * CURSOR_HEIGHT * 4) as usize];
        paint_pointer(&mut pixels);
        let pixel = |x: usize, y: usize| &pixels[(y * CURSOR_WIDTH as usize + x) * 4..][..4];
        let alpha = |x: usize, y: usize| pixel(x, y)[3];
        assert!(alpha(HOTSPOT_X as usize, HOTSPOT_Y as usize) > 0);
        assert!(pixel(10, 10)[3] < 240);
        let visible = (0..CURSOR_HEIGHT as usize)
            .flat_map(|y| (0..CURSOR_WIDTH as usize).map(move |x| (x, y)))
            .filter(|(x, y)| alpha(*x, *y) >= 128)
            .collect::<Vec<_>>();
        let min_x = visible.iter().map(|(x, _)| x).min().unwrap();
        let max_x = visible.iter().map(|(x, _)| x).max().unwrap();
        let min_y = visible.iter().map(|(_, y)| y).min().unwrap();
        let max_y = visible.iter().map(|(_, y)| y).max().unwrap();
        let visible_width = max_x - min_x + 1;
        let visible_height = max_y - min_y + 1;
        assert!(
            (8..=10).contains(&visible_width),
            "pointer width is out of range: {visible_width}"
        );
        assert!(
            (8..=10).contains(&visible_height),
            "pointer height is out of range: {visible_height}"
        );
        let shadowed = (0..CURSOR_HEIGHT as usize)
            .flat_map(|y| (0..CURSOR_WIDTH as usize).map(move |x| (x, y)))
            .filter(|(x, y)| alpha(*x, *y) > 0)
            .collect::<Vec<_>>();
        let shadow_min_x = shadowed.iter().map(|(x, _)| x).min().unwrap();
        let shadow_max_x = shadowed.iter().map(|(x, _)| x).max().unwrap();
        let shadow_min_y = shadowed.iter().map(|(_, y)| y).min().unwrap();
        let shadow_max_y = shadowed.iter().map(|(_, y)| y).max().unwrap();
        assert!(
            shadow_max_x - shadow_min_x + 1 <= 12,
            "pointer shadow is too wide: {shadow_min_x}..={shadow_max_x}"
        );
        assert!(
            shadow_max_y - shadow_min_y + 1 <= 12,
            "pointer shadow is too tall: {shadow_min_y}..={shadow_max_y}"
        );
    }

    #[test]
    fn pointer_outline_tracks_the_scaled_shape() {
        for (x, y) in POINTER_SHAPE {
            assert!(pointer_edge_distance(x * POINTER_SCALE, y * POINTER_SCALE) < 1e-9);
        }
    }
}
