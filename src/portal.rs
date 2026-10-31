use futures_util::{Stream, StreamExt};
use std::{
    collections::HashMap,
    fmt,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::time::timeout;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, Proxy};

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const ROOT: &str = "/org/freedesktop/portal/desktop";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
const INPUT_TIMEOUT: Duration = Duration::from_secs(5);
const PORTAL_CALL_TIMEOUT: Duration = Duration::from_secs(5);
const PORTAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(270);
const MAX_PORTAL_STREAMS: usize = 16;
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;
static REQUEST_CLEANUPS: OnceLock<Mutex<Vec<tokio::task::JoinHandle<()>>>> = OnceLock::new();

pub async fn await_request_cleanups() {
    let cleanups = REQUEST_CLEANUPS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain(..)
        .collect::<Vec<_>>();
    for cleanup in cleanups {
        let _ = cleanup.await;
    }
}

pub fn libei_available() -> bool {
    crate::eis::EisInput::available()
}

#[derive(Debug)]
pub enum PortalStartError {
    Denied(String),
    Timeout(String),
    Failed(String),
}

impl PortalStartError {
    pub fn allows_compositor_fallback(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

impl fmt::Display for PortalStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied(message) | Self::Timeout(message) | Self::Failed(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl From<String> for PortalStartError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<&str> for PortalStartError {
    fn from(message: &str) -> Self {
        Self::Failed(message.to_owned())
    }
}

#[derive(Clone, Copy)]
enum JpegEncoder {
    Jpegenc,
    AvencMjpeg,
}

impl JpegEncoder {
    fn element(self) -> &'static str {
        match self {
            Self::Jpegenc => "jpegenc",
            Self::AvencMjpeg => "avenc_mjpeg",
        }
    }

    fn quality(self, quality: u8) -> String {
        match self {
            Self::Jpegenc => format!("quality={quality}"),
            Self::AvencMjpeg => format!(
                "quantizer={}",
                2 + (100 - u32::from(quality.min(100))) * 29 / 100
            ),
        }
    }
}

fn jpeg_encoder() -> Option<JpegEncoder> {
    static ENCODER: OnceLock<Option<JpegEncoder>> = OnceLock::new();
    *ENCODER.get_or_init(|| {
        [JpegEncoder::Jpegenc, JpegEncoder::AvencMjpeg]
            .into_iter()
            .find(|encoder| {
                crate::limited_command("3s", "gst-inspect-1.0")
                    .arg(encoder.element())
                    .output()
                    .is_ok_and(|result| result.status.success())
            })
    })
}

pub fn jpeg_encoder_available() -> bool {
    jpeg_encoder().is_some()
}

fn gstreamer_python() -> Option<&'static str> {
    static PYTHON: OnceLock<Option<String>> = OnceLock::new();
    PYTHON.get_or_init(|| {
        let probe = "import gi; gi.require_version('Gst', '1.0'); gi.require_version('GstVideo', '1.0'); from gi.repository import Gst, GstVideo; Gst.init(None); assert hasattr(GstVideo, 'buffer_get_video_region_of_interest_meta_id'); assert all(Gst.ElementFactory.find(name) for name in ('pipewiresrc', 'tee', 'queue', 'appsink', 'videoconvert', 'videoscale')); assert any(Gst.ElementFactory.find(name) for name in ('jpegenc', 'avenc_mjpeg'))";
        let mut candidates = vec!["python3".to_owned()];
        candidates.extend(["/usr/bin/python3".to_owned(), "/usr/libexec/platform-python".to_owned()]);
        candidates.into_iter().find(|program| {
            crate::limited_command("3s", program).args(["-c", probe])
                .status().is_ok_and(|status| status.success())
        })
    }).as_deref()
}

#[link(name = "c")]
unsafe extern "C" {
    fn dup2(old_fd: i32, new_fd: i32) -> i32;
}

pub struct PortalSession {
    pub connection: Connection,
    pub session: OwnedObjectPath,
    streams: Vec<PortalStream>,
    pub logical_width: u32,
    pub logical_height: u32,
    pub origin_x: i32,
    pub origin_y: i32,
    pub position_available: bool,
    pub synthetic_layout: bool,
    pub source: Option<String>,
    pub remote_desktop_active: bool,
    pub remote_pointer: bool,
    pub remote_keyboard: bool,
    cursor_metadata: bool,
    cursor_metadata_failed: AtomicBool,
    pub eis_error: Option<String>,
    pipewire_fds: Vec<Arc<OwnedFd>>,
    eis_input: Option<Arc<Mutex<crate::eis::EisInput>>>,
    closed: Arc<AtomicBool>,
}

#[derive(Clone)]
struct PortalStream {
    id: u32,
    pipewire_serial: Option<u64>,
    width: u32,
    height: u32,
    origin_x: i32,
    origin_y: i32,
    position_available: bool,
    source: Option<String>,
    mapping_id: Option<String>,
}

struct PortalLayout {
    origin_x: i32,
    origin_y: i32,
    width: u32,
    height: u32,
    position_available: bool,
    synthetic_layout: bool,
    source: Option<String>,
}

struct PortalStart {
    session: OwnedObjectPath,
    streams: Vec<PortalStream>,
    layout: PortalLayout,
    remote_desktop_active: bool,
    remote_pointer: bool,
    remote_keyboard: bool,
    pipewire_fds: Vec<OwnedFd>,
    eis_input: Option<Arc<Mutex<crate::eis::EisInput>>>,
    eis_error: Option<String>,
    cursor_metadata: bool,
}

struct PortalSessionGuard {
    connection: Connection,
    session: OwnedObjectPath,
    active: bool,
}

struct PortalRequestGuard {
    connection: Connection,
    request: OwnedObjectPath,
    active: bool,
}

#[derive(Clone, Copy)]
enum PointerPurpose {
    Motion,
    Click,
    Scroll,
}

impl PortalSessionGuard {
    fn new(connection: &Connection, session: OwnedObjectPath) -> Self {
        Self {
            connection: connection.clone(),
            session,
            active: true,
        }
    }

    async fn close(&mut self) {
        if self.active && close_session(&self.connection, &self.session).await.is_ok() {
            self.active = false;
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for PortalRequestGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        // Dropping the response stream alone can leave the portal chooser open.
        let connection = self.connection.clone();
        let request = self.request.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let cleanup = runtime.spawn(async move {
            let proxy = match timeout(
                PORTAL_CALL_TIMEOUT,
                Proxy::new(&connection, DESTINATION, request, REQUEST_IFACE),
            )
            .await
            {
                Ok(Ok(proxy)) => proxy,
                Ok(Err(error)) => {
                    eprintln!("computer-use-linux: cannot close cancelled portal request: {error}");
                    return;
                }
                Err(_) => {
                    eprintln!("computer-use-linux: timed out preparing portal request cleanup");
                    return;
                }
            };
            match timeout(PORTAL_CALL_TIMEOUT, proxy.call::<_, _, ()>("Close", &())).await {
                Ok(Ok(())) => (),
                Ok(Err(error)) => {
                    eprintln!("computer-use-linux: cannot close cancelled portal request: {error}")
                }
                Err(error) => eprintln!(
                    "computer-use-linux: timed out closing cancelled portal request: {error}"
                ),
            }
        });
        let mut cleanups = REQUEST_CLEANUPS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cleanups.retain(|cleanup| !cleanup.is_finished());
        cleanups.push(cleanup);
    }
}

impl Drop for PortalSessionGuard {
    fn drop(&mut self) {
        if self.active {
            close_in_background(self.connection.clone(), self.session.clone());
        }
    }
}

impl PortalSession {
    pub async fn start(hyprland_fallback: bool) -> Result<Self, PortalStartError> {
        let connection = session_connection().await?;
        match start_remote_desktop(&connection).await {
            Ok(started) => Ok(Self::from_started(connection, started).await),
            Err(error @ (PortalStartError::Denied(_) | PortalStartError::Timeout(_))) => Err(error),
            Err(remote_error) => {
                drop(connection);
                let connection = session_connection().await?;
                match start_screencast(&connection, hyprland_fallback).await {
                    Ok(started) => Ok(Self::from_started(connection, started).await),
                    Err(
                        capture_error
                        @ (PortalStartError::Denied(_) | PortalStartError::Timeout(_)),
                    ) => Err(capture_error),
                    Err(capture_error) => Err(PortalStartError::Failed(format!(
                        "RemoteDesktop unavailable ({remote_error}); ScreenCast failed ({capture_error})"
                    ))),
                }
            }
        }
    }

    async fn from_started(connection: Connection, started: PortalStart) -> Self {
        let PortalStart {
            session,
            streams,
            layout,
            remote_desktop_active,
            remote_pointer,
            remote_keyboard,
            pipewire_fds,
            eis_input,
            eis_error,
            cursor_metadata,
        } = started;
        let closed = Arc::new(AtomicBool::new(false));
        let signal_closed = Arc::clone(&closed);
        let signal_connection = connection.clone();
        let signal_session = session.clone();
        tokio::spawn(async move {
            let Ok(proxy) = session_proxy(&signal_connection, &signal_session).await else {
                return;
            };
            let Ok(Ok(mut signals)) =
                timeout(PORTAL_CALL_TIMEOUT, proxy.receive_signal("Closed")).await
            else {
                return;
            };
            if signals.next().await.is_some() {
                signal_closed.store(true, Ordering::Release);
            }
        });
        Self {
            connection,
            session,
            streams,
            logical_width: layout.width,
            logical_height: layout.height,
            origin_x: layout.origin_x,
            origin_y: layout.origin_y,
            position_available: layout.position_available,
            synthetic_layout: layout.synthetic_layout,
            source: layout.source,
            remote_desktop_active,
            remote_pointer,
            remote_keyboard,
            cursor_metadata,
            cursor_metadata_failed: AtomicBool::new(false),
            eis_error,
            pipewire_fds: pipewire_fds.into_iter().map(Arc::new).collect(),
            eis_input,
            closed,
        }
    }

    pub fn capture(&self, quality: u8, max_dimension: u32) -> Result<Vec<u8>, String> {
        self.ensure_open()?;
        let encoder = jpeg_encoder()
            .ok_or("GStreamer needs jpegenc or avenc_mjpeg to capture portal frames")?;
        if self.logical_width == 0 || self.logical_height == 0 {
            let stream = self
                .streams
                .first()
                .ok_or("portal returned no display stream")?;
            let fd = self
                .pipewire_fds
                .first()
                .ok_or("portal PipeWire connection is missing")?
                .try_clone()
                .map_err(|error| format!("duplicating PipeWire connection: {error}"))?;
            let mut command = crate::limited_command("4s", "gst-launch-1.0");
            command
                .args([
                    "-q",
                    "pipewiresrc",
                    "fd=0",
                    &stream.target(),
                    "num-buffers=1",
                    "!",
                    "videoconvert",
                    "!",
                    encoder.element(),
                    &encoder.quality(quality),
                    "!",
                    "fdsink",
                    "fd=1",
                ])
                .stdin(std::process::Stdio::from(fd));
            let (image, status) = crate::bounded_command_output(&mut command, crate::MAX_IMAGE)
                .map_err(|error| format!("PipeWire capture failed: {error}"))?;
            return if status.success() {
                Ok(image)
            } else {
                Err(format!("PipeWire capture failed with status {status}"))
            };
        }
        let (scale, output_width, output_height) =
            capture_layout_size(self.logical_width, self.logical_height, max_dimension);
        if self.streams.len() == 1 {
            let stream = self
                .streams
                .first()
                .ok_or("portal returned no display stream")?;
            let fd = self
                .pipewire_fds
                .first()
                .ok_or("portal PipeWire connection is missing")?
                .try_clone()
                .map_err(|error| format!("duplicating PipeWire connection: {error}"))?;
            let pipeline = vec![
                "-q".to_owned(),
                "pipewiresrc".into(),
                "fd=0".into(),
                stream.target(),
                "num-buffers=1".into(),
                "!".into(),
                "videoconvert".into(),
                "!".into(),
                "videoscale".into(),
                "!".into(),
                format!("video/x-raw,width={output_width},height={output_height}"),
                "!".into(),
                encoder.element().into(),
                encoder.quality(quality),
                "!".into(),
                "fdsink".into(),
                "fd=1".into(),
            ];
            let mut command = crate::limited_command("4s", "gst-launch-1.0");
            command.args(pipeline).stdin(std::process::Stdio::from(fd));
            let (image, status) = crate::bounded_command_output(&mut command, crate::MAX_IMAGE)
                .map_err(|error| format!("PipeWire capture failed: {error}"))?;
            return if status.success() {
                Ok(image)
            } else {
                Err(format!("PipeWire capture failed with status {status}"))
            };
        }
        if self.pipewire_fds.len() != self.streams.len() {
            return Err(
                "portal did not provide one PipeWire connection per selected stream".into(),
            );
        }
        let pipewire_fds = self
            .pipewire_fds
            .iter()
            .map(|fd| {
                fd.try_clone()
                    .map_err(|error| format!("duplicating PipeWire connection: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let first_target = 10.max(
            pipewire_fds
                .iter()
                .map(AsRawFd::as_raw_fd)
                .max()
                .unwrap_or(2)
                .checked_add(1)
                .ok_or("PipeWire file descriptor is out of range")?,
        );
        let stream_fds = pipewire_fds
            .iter()
            .enumerate()
            .map(|(index, fd)| {
                i32::try_from(index)
                    .ok()
                    .and_then(|index| first_target.checked_add(index))
                    .map(|target| (fd.as_raw_fd(), target))
                    .ok_or("too many PipeWire streams")
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut pipeline = vec!["-q".to_owned()];
        for (index, stream) in self.streams.iter().enumerate() {
            let fd = stream_fds[index].1;
            pipeline.extend([
                "pipewiresrc".into(),
                format!("fd={fd}"),
                stream.target(),
                "num-buffers=1".into(),
                "!".into(),
                "queue".into(),
                "!".into(),
                "videoconvert".into(),
                "!".into(),
                "videoscale".into(),
                "!".into(),
                format!(
                    "video/x-raw,width={},height={}",
                    scaled(stream.width, scale),
                    scaled(stream.height, scale)
                ),
                "!".into(),
                "compositor.".into(),
            ]);
            pipeline.push(format!(
                "compositor.sink_{index}::xpos={}",
                scaled_offset(stream.origin_x as i64 - self.origin_x as i64, scale)
            ));
            pipeline.push(format!(
                "compositor.sink_{index}::ypos={}",
                scaled_offset(stream.origin_y as i64 - self.origin_y as i64, scale)
            ));
        }
        pipeline.extend([
            "compositor".into(),
            "name=compositor".into(),
            "background=1".into(),
            "!".into(),
            "videoscale".into(),
            "!".into(),
            format!("video/x-raw,width={output_width},height={output_height}"),
            "!".into(),
            "videoconvert".into(),
            "!".into(),
            encoder.element().into(),
            encoder.quality(quality),
            "!".into(),
            "fdsink".into(),
            "fd=1".into(),
        ]);
        let mut command = crate::limited_command("4s", "gst-launch-1.0");
        // SAFETY: dup2 is async-signal-safe; each source descriptor stays open until wait completes.
        unsafe {
            command.pre_exec(move || {
                for &(source, target) in &stream_fds {
                    if dup2(source, target) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        command.args(pipeline).stdin(std::process::Stdio::null());
        let (image, status) = crate::bounded_command_output(&mut command, crate::MAX_IMAGE)
            .map_err(|error| format!("PipeWire capture failed: {error}"))?;
        if !status.success() {
            return Err(format!("PipeWire capture failed with status {status}"));
        }
        Ok(image)
    }

    pub fn capture_with_cursor(
        &self,
        quality: u8,
        max_dimension: u32,
    ) -> Result<(Vec<u8>, Option<(i32, i32)>), String> {
        self.ensure_open()?;
        if !self.cursor_position_available() || self.cursor_metadata_failed.load(Ordering::Acquire)
        {
            return self
                .capture(quality, max_dimension)
                .map(|image| (image, None));
        }
        if self.pipewire_fds.len() != self.streams.len() {
            return Err(
                "portal did not provide one PipeWire connection per selected stream".into(),
            );
        }
        let pipewire_fds = self
            .pipewire_fds
            .iter()
            .map(|fd| {
                fd.try_clone()
                    .map_err(|error| format!("duplicating PipeWire connection: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let first_target = 10.max(
            pipewire_fds
                .iter()
                .map(AsRawFd::as_raw_fd)
                .max()
                .unwrap_or(2)
                .checked_add(1)
                .ok_or("PipeWire file descriptor is out of range")?,
        );
        let stream_fds = pipewire_fds
            .iter()
            .enumerate()
            .map(|(index, fd)| {
                i32::try_from(index)
                    .ok()
                    .and_then(|index| first_target.checked_add(index))
                    .map(|target| (fd.as_raw_fd(), target))
                    .ok_or("too many PipeWire streams")
            })
            .collect::<Result<Vec<_>, _>>()?;
        let (_, output_width, output_height) =
            capture_layout_size(self.logical_width, self.logical_height, max_dimension);
        match cursor_metadata_capture(
            &self.streams,
            &stream_fds,
            quality,
            output_width,
            output_height,
            self.origin_x,
            self.origin_y,
            self.logical_width,
            self.logical_height,
            max_dimension,
        ) {
            Ok((image, Some((x, y)))) => {
                let pointer = crate::captured_pointer_position(
                    (x, y),
                    self.origin_x,
                    self.origin_y,
                    self.logical_width,
                    self.logical_height,
                    &image,
                );
                Ok((image, pointer))
            }
            Ok((image, None)) => Ok((image, None)),
            Err(error) if error.contains("exceeds") => Err(error),
            Err(_) => {
                self.cursor_metadata_failed.store(true, Ordering::Release);
                self.capture(quality, max_dimension)
                    .map(|image| (image, None))
            }
        }
    }

    pub fn cursor_position_available(&self) -> bool {
        let single_stream_has_local_coordinates =
            self.streams.len() == 1 && self.streams[0].width > 0 && self.streams[0].height > 0;
        !self.closed.load(Ordering::Acquire)
            && !self.cursor_metadata_failed.load(Ordering::Acquire)
            && self.cursor_metadata
            && (self.position_available || single_stream_has_local_coordinates)
            && !self.synthetic_layout
            && !self.streams.is_empty()
            && self.coordinates_available()
            && self.streams.iter().all(|stream| {
                stream.width > 0
                    && stream.height > 0
                    && (stream.position_available || single_stream_has_local_coordinates)
            })
    }

    pub async fn pointer(&self, x: f64, y: f64) -> Result<(), String> {
        self.pointer_with_purpose(x, y, PointerPurpose::Motion)
            .await
    }

    pub async fn pointer_for_click(&self, x: f64, y: f64) -> Result<(), String> {
        self.pointer_with_purpose(x, y, PointerPurpose::Click).await
    }

    pub async fn pointer_for_scroll(&self, x: f64, y: f64) -> Result<(), String> {
        self.pointer_with_purpose(x, y, PointerPurpose::Scroll)
            .await
    }

    async fn pointer_with_purpose(
        &self,
        x: f64,
        y: f64,
        purpose: PointerPurpose,
    ) -> Result<(), String> {
        self.ensure_open()?;
        if !self.coordinates_available() {
            return Err("portal stream omitted logical dimensions; pointer coordinates cannot be mapped safely".into());
        }
        let screen_x = x + self.origin_x as f64;
        let screen_y = y + self.origin_y as f64;
        let contains = |stream: &&PortalStream| {
            screen_x >= stream.origin_x as f64
                && screen_y >= stream.origin_y as f64
                && screen_x < stream.origin_x as f64 + stream.width as f64
                && screen_y < stream.origin_y as f64 + stream.height as f64
        };
        if let Some(eis) = &self.eis_input {
            let mut input = eis.lock().map_err(|_| "EIS input lock is unavailable")?;
            let stream = self.streams.iter().filter(contains).find(|stream| {
                let Some(mapping) = stream.mapping_id.as_deref() else { return false };
                match purpose {
                    PointerPurpose::Motion => input.has_pointer_mapping(mapping),
                    PointerPurpose::Click => input.has_pointer_for(mapping),
                    PointerPurpose::Scroll => input.has_pointer_mapping(mapping) && input.has_scroll_for(mapping),
                }
            }).ok_or("no selected portal stream supports this pointer action at the requested coordinate")?;
            let local_x = screen_x - stream.origin_x as f64;
            let local_y = screen_y - stream.origin_y as f64;
            return input.pointer(
                stream
                    .mapping_id
                    .as_deref()
                    .ok_or("portal stream has no EIS mapping ID")?,
                local_x,
                local_y,
                stream.width,
                stream.height,
            );
        }
        if !self.remote_pointer {
            return Err("RemoteDesktop pointer input is not available in this session".into());
        }
        let stream = self
            .streams
            .iter()
            .find(contains)
            .ok_or("pointer coordinate is outside the selected portal streams")?;
        let local_x = screen_x - stream.origin_x as f64;
        let local_y = screen_y - stream.origin_y as f64;
        let proxy = portal_proxy(&self.connection, "org.freedesktop.portal.RemoteDesktop").await?;
        let options = HashMap::<String, OwnedValue>::new();
        timeout(
            INPUT_TIMEOUT,
            proxy.call::<_, _, ()>(
                "NotifyPointerMotionAbsolute",
                &(&self.session, &options, stream.id, local_x, local_y),
            ),
        )
        .await
        .map_err(|_| "portal pointer motion timed out")?
        .map_err(|error| format!("portal pointer motion failed: {error}"))
    }

    pub async fn button(&self, button: u32, pressed: bool) -> Result<(), String> {
        self.ensure_open()?;
        let button = match button {
            1 => BTN_LEFT,
            2 => BTN_MIDDLE,
            3 => BTN_RIGHT,
            8 => BTN_SIDE,
            9 => BTN_EXTRA,
            _ => return Err("unsupported pointer button".into()),
        };
        if let Some(eis) = &self.eis_input {
            let mut input = eis.lock().map_err(|_| "EIS input lock is unavailable")?;
            return input.button(button, pressed);
        }
        if !self.remote_pointer {
            return Err("RemoteDesktop pointer input is not available in this session".into());
        }
        let proxy = portal_proxy(&self.connection, "org.freedesktop.portal.RemoteDesktop").await?;
        let options = HashMap::<String, OwnedValue>::new();
        let state = u32::from(pressed);
        timeout(
            INPUT_TIMEOUT,
            proxy.call::<_, _, ()>(
                "NotifyPointerButton",
                &(&self.session, &options, button as i32, state),
            ),
        )
        .await
        .map_err(|_| "portal button input timed out")?
        .map_err(|error| format!("portal button input failed: {error}"))
    }

    pub async fn key(&self, keysym: u32, pressed: bool) -> Result<(), String> {
        self.ensure_open()?;
        if let Some(eis) = &self.eis_input {
            let mut input = eis.lock().map_err(|_| "EIS input lock is unavailable")?;
            if !input.has_keyboard() {
                return Err("EIS has no active text device".into());
            }
            return input.key(keysym, pressed);
        }
        if !self.remote_keyboard {
            return Err("RemoteDesktop keyboard input is not available in this session".into());
        }
        let proxy = portal_proxy(&self.connection, "org.freedesktop.portal.RemoteDesktop").await?;
        let options = HashMap::<String, OwnedValue>::new();
        let state = u32::from(pressed);
        timeout(
            INPUT_TIMEOUT,
            proxy.call::<_, _, ()>(
                "NotifyKeyboardKeysym",
                &(&self.session, &options, keysym as i32, state),
            ),
        )
        .await
        .map_err(|_| "portal keyboard input timed out")?
        .map_err(|error| format!("portal keyboard input failed: {error}"))
    }

    pub async fn scroll(&self, direction: &str, amount: i32) -> Result<(), String> {
        self.ensure_open()?;
        if let Some(eis) = &self.eis_input {
            let mut input = eis.lock().map_err(|_| "EIS input lock is unavailable")?;
            return input.scroll(direction, amount);
        }
        if !self.remote_pointer {
            return Err("RemoteDesktop scroll input is not available in this session".into());
        }
        let proxy = portal_proxy(&self.connection, "org.freedesktop.portal.RemoteDesktop").await?;
        let options = HashMap::<String, OwnedValue>::new();
        let (axis, sign) = match direction {
            "up" => (0u32, -1i32),
            "down" => (0, 1),
            "left" => (1, -1),
            "right" => (1, 1),
            _ => return Err("invalid scroll direction".into()),
        };
        let steps = sign * amount;
        timeout(
            INPUT_TIMEOUT,
            proxy.call::<_, _, ()>(
                "NotifyPointerAxisDiscrete",
                &(&self.session, &options, axis, steps),
            ),
        )
        .await
        .map_err(|_| "portal scroll input timed out")?
        .map_err(|error| format!("portal scroll failed: {error}"))
    }

    pub fn smooth_scroll_available(&self) -> bool {
        if self.closed.load(Ordering::Acquire) || !self.coordinates_available() {
            return false;
        }
        self.eis_input.as_ref().map_or(self.remote_pointer, |eis| {
            eis.lock().is_ok_and(|mut input| {
                self.streams
                    .iter()
                    .filter_map(|stream| stream.mapping_id.as_deref())
                    .any(|id| input.has_smooth_scroll_for(id))
            })
        })
    }

    pub async fn scroll_smooth(&self, x: f64, y: f64) -> Result<(), String> {
        self.ensure_open()?;
        if let Some(eis) = &self.eis_input {
            let mut input = eis.lock().map_err(|_| "EIS input lock is unavailable")?;
            return input.scroll_smooth(x, y);
        }
        if !self.remote_pointer {
            return Err("RemoteDesktop pointer input is not available in this session".into());
        }
        let proxy = portal_proxy(&self.connection, "org.freedesktop.portal.RemoteDesktop").await?;
        let mut options = HashMap::<String, OwnedValue>::new();
        options.insert("finish".into(), OwnedValue::from(true));
        timeout(
            INPUT_TIMEOUT,
            proxy.call::<_, _, ()>("NotifyPointerAxis", &(&self.session, &options, x, y)),
        )
        .await
        .map_err(|_| "portal smooth scroll timed out")?
        .map_err(|error| format!("portal smooth scroll failed: {error}"))
    }

    pub async fn close(&self) -> Result<(), String> {
        if self.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let proxy = session_proxy(&self.connection, &self.session).await?;
        timeout(PORTAL_CALL_TIMEOUT, proxy.call::<_, _, ()>("Close", &()))
            .await
            .map_err(|_| "closing portal session timed out")?
            .map_err(|error| format!("closing portal session: {error}"))?;
        self.closed.store(true, Ordering::Release);
        Ok(())
    }

    pub fn pointer_available(&self) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        if !self.coordinates_available() {
            return false;
        }
        self.eis_input.as_ref().map_or(self.remote_pointer, |eis| {
            eis.lock().is_ok_and(|mut input| {
                self.streams
                    .iter()
                    .filter_map(|stream| stream.mapping_id.as_deref())
                    .any(|id| input.has_pointer_for(id))
            })
        })
    }

    pub fn keyboard_available(&self) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        self.eis_input.as_ref().map_or(self.remote_keyboard, |eis| {
            eis.lock().is_ok_and(|mut input| input.has_keyboard())
        })
    }

    pub fn scroll_available(&self) -> bool {
        if self.closed.load(Ordering::Acquire) || !self.coordinates_available() {
            return false;
        }
        self.eis_input.as_ref().map_or(self.remote_pointer, |eis| {
            eis.lock().is_ok_and(|mut input| {
                self.streams
                    .iter()
                    .filter_map(|stream| stream.mapping_id.as_deref())
                    .any(|id| input.has_scroll_for(id))
            })
        })
    }

    pub fn input_regions(&self, image_width: u32, image_height: u32) -> Vec<serde_json::Value> {
        if !self.coordinates_available() || image_width == 0 || image_height == 0 {
            return Vec::new();
        }
        let mut eis = self.eis_input.as_ref().and_then(|input| input.lock().ok());
        self.streams.iter().enumerate().map(|(index, stream)| {
            let left = i64::from(stream.origin_x) - i64::from(self.origin_x);
            let top = i64::from(stream.origin_y) - i64::from(self.origin_y);
            let right = left + i64::from(stream.width);
            let bottom = top + i64::from(stream.height);
            let x = (left * i64::from(image_width) / i64::from(self.logical_width)) as u32;
            let y = (top * i64::from(image_height) / i64::from(self.logical_height)) as u32;
            let x_end = ((right * i64::from(image_width) + i64::from(self.logical_width) - 1) / i64::from(self.logical_width)) as u32;
            let y_end = ((bottom * i64::from(image_height) + i64::from(self.logical_height) - 1) / i64::from(self.logical_height)) as u32;
            let (pointer_motion, pointer_button, scroll, smooth_scroll) = if self.eis_input.is_some() {
                if let (Some(input), Some(mapping)) = (eis.as_mut(), stream.mapping_id.as_deref()) {
                    (
                        input.has_pointer_mapping(mapping),
                        input.has_button_for(mapping),
                        input.has_scroll_for(mapping),
                        input.has_smooth_scroll_for(mapping),
                    )
                } else { (false, false, false, false) }
            } else {
                (self.remote_pointer, self.remote_pointer, self.remote_pointer, self.remote_pointer)
            };
            serde_json::json!({
                "index":index,
                "source":stream.source,
                "bounds":{"x":x,"y":y,"width":x_end.saturating_sub(x),"height":y_end.saturating_sub(y),"coordinate_space":"captured output pixels"},
                "pointer_motion":pointer_motion,
                "pointer_button":pointer_button,
                "pointer_click":pointer_motion && pointer_button,
                "scroll":scroll,
                "smooth_scroll":smooth_scroll,
            })
        }).collect()
    }

    pub fn coordinates_available(&self) -> bool {
        self.logical_width > 0 && self.logical_height > 0
    }

    pub fn monitor_geometry_available(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
            && self.source.as_deref() == Some("monitor")
            && self.position_available
            && !self.synthetic_layout
            && self.coordinates_available()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub fn unicode_text_available(&self) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        self.eis_input
            .as_ref()
            .is_some_and(|eis| eis.lock().is_ok_and(|mut input| input.has_keyboard()))
    }

    pub fn type_text(&self, text: &str) -> Result<(), String> {
        self.ensure_open()?;
        let Some(eis) = &self.eis_input else {
            return Err("EIS text input is not active".into());
        };
        let mut input = eis.lock().map_err(|_| "EIS input lock is unavailable")?;
        if !input.has_keyboard() {
            return Err("EIS text input is not active".into());
        }
        input.text(text)
    }

    fn ensure_open(&self) -> Result<(), String> {
        if self.closed.load(Ordering::Acquire) {
            Err("desktop portal session was closed".into())
        } else {
            Ok(())
        }
    }
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        close_in_background(self.connection.clone(), self.session.clone());
    }
}

fn close_in_background(connection: Connection, session: OwnedObjectPath) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    runtime.spawn(async move {
        let _ = close_session(&connection, &session).await;
    });
}

async fn start_remote_desktop(connection: &Connection) -> Result<PortalStart, PortalStartError> {
    let remote = portal_proxy(connection, "org.freedesktop.portal.RemoteDesktop").await?;
    let screen = portal_proxy(connection, "org.freedesktop.portal.ScreenCast").await?;

    let (token, options) = options_with_token()?;
    let mut options = options;
    let session_token = uuid::Uuid::new_v4().simple().to_string();
    options.insert(
        "session_handle_token".into(),
        OwnedValue::try_from(Value::from(session_token.as_str()))
            .map_err(|error| error.to_string())?,
    );
    let session_results =
        request(connection, &remote, "CreateSession", &token, &(options,)).await?;
    let session = object_path_result(&session_results, "session_handle")?;
    let mut session_guard = PortalSessionGuard::new(connection, session.clone());

    let result = async {
        let (token, device_options) = options_with_token()?;
        let mut device_options = device_options;
        device_options.insert("types".into(), OwnedValue::from(3u32));
        request(
            connection,
            &remote,
            "SelectDevices",
            &token,
            &(&session, &device_options),
        )
        .await?;

        let (token, source_options) = options_with_token()?;
        let mut source_options = source_options;
        source_options.insert("types".into(), OwnedValue::from(3u32));
        let multiple = compositor_available_async().await?;
        source_options.insert("multiple".into(), OwnedValue::from(multiple));
        let mode = cursor_mode(&screen).await?;
        if let Some(mode) = mode {
            source_options.insert("cursor_mode".into(), OwnedValue::from(mode));
        }
        request(
            connection,
            &screen,
            "SelectSources",
            &token,
            &(&session, &source_options),
        )
        .await?;

        let (token, start_options) = options_with_token()?;
        let result = request(
            connection,
            &remote,
            "Start",
            &token,
            &(&session, "", &start_options),
        )
        .await?;
        let devices = result
            .get("devices")
            .ok_or("RemoteDesktop portal omitted granted devices")?
            .try_clone()
            .map_err(|error| error.to_string())?;
        let devices: u32 = devices
            .try_into()
            .map_err(|error: zbus::zvariant::Error| error.to_string())?;
        let mut remote_keyboard = devices & 1 != 0;
        let mut remote_pointer = devices & 2 != 0;
        let streams = stream_result(&result)?;
        let mut streams = streams;
        let layout = portal_layout(&mut streams, multiple)?;
        let mapping_ids = streams
            .iter()
            .map(|stream| stream.mapping_id.clone())
            .collect::<Option<Vec<_>>>()
            .filter(|ids| (0..ids.len()).all(|index| !ids[..index].contains(&ids[index])));
        let mut eis_error = None;
        let eis_input = if mapping_ids.as_ref().is_some_and(|ids| !ids.is_empty())
            && crate::eis::EisInput::available()
        {
            let eis_options = HashMap::<String, OwnedValue>::new();
            match timeout(
                PORTAL_CALL_TIMEOUT,
                remote.call::<_, _, zbus::zvariant::OwnedFd>(
                    "ConnectToEIS",
                    &(&session, &eis_options),
                ),
            )
            .await
            {
                Err(_) => {
                    remote_pointer = false;
                    remote_keyboard = false;
                    eis_error = Some(
                        "portal ConnectToEIS call timed out; input is disabled for this session"
                            .into(),
                    );
                    None
                }
                Ok(Err(_)) => None,
                Ok(Ok(fd)) => {
                    match OwnedFd::try_from(fd)
                        .map_err(|error| error.to_string())
                        .and_then(crate::eis::EisInput::connect)
                    {
                        Ok(input) => Some(Arc::new(Mutex::new(input))),
                        Err(error) => {
                            remote_pointer = false;
                            remote_keyboard = false;
                            eis_error = Some(error);
                            None
                        }
                    }
                }
            }
        } else {
            None
        };
        let pipewire_fds = open_pipewire_remotes(&screen, &session, streams.len()).await?;
        Ok::<_, PortalStartError>(PortalStart {
            session: session.clone(),
            streams,
            layout,
            remote_desktop_active: true,
            remote_pointer,
            remote_keyboard,
            pipewire_fds,
            eis_input,
            eis_error,
            cursor_metadata: mode == Some(4),
        })
    }
    .await;
    if result.is_ok() {
        session_guard.disarm();
    } else {
        session_guard.close().await;
    }
    result
}

async fn start_screencast(
    connection: &Connection,
    monitor_only: bool,
) -> Result<PortalStart, PortalStartError> {
    let screen = portal_proxy(connection, "org.freedesktop.portal.ScreenCast").await?;
    let (token, mut options) = options_with_token()?;
    let session_token = uuid::Uuid::new_v4().simple().to_string();
    options.insert(
        "session_handle_token".into(),
        OwnedValue::try_from(Value::from(session_token.as_str()))
            .map_err(|error| error.to_string())?,
    );
    let created = request(connection, &screen, "CreateSession", &token, &(options,)).await?;
    let session = object_path_result(&created, "session_handle")?;
    let mut session_guard = PortalSessionGuard::new(connection, session.clone());

    let result = async {
        let (token, source_options) = options_with_token()?;
        let mut source_options = source_options;
        source_options.insert(
            "types".into(),
            OwnedValue::from(if monitor_only { 1u32 } else { 3u32 }),
        );
        let multiple = compositor_available_async().await?;
        source_options.insert("multiple".into(), OwnedValue::from(multiple));
        let mode = cursor_mode(&screen).await?;
        if let Some(mode) = mode {
            source_options.insert("cursor_mode".into(), OwnedValue::from(mode));
        }
        request(
            connection,
            &screen,
            "SelectSources",
            &token,
            &(&session, &source_options),
        )
        .await?;

        let (token, start_options) = options_with_token()?;
        let result = request(
            connection,
            &screen,
            "Start",
            &token,
            &(&session, "", &start_options),
        )
        .await?;
        let mut streams = stream_result(&result)?;
        let layout = portal_layout(&mut streams, multiple)?;
        let pipewire_fds = open_pipewire_remotes(&screen, &session, streams.len()).await?;
        Ok::<_, PortalStartError>(PortalStart {
            session: session.clone(),
            streams,
            layout,
            remote_desktop_active: false,
            remote_pointer: false,
            remote_keyboard: false,
            pipewire_fds,
            eis_input: None,
            eis_error: None,
            cursor_metadata: mode == Some(4),
        })
    }
    .await;
    if result.is_ok() {
        session_guard.disarm();
    } else {
        session_guard.close().await;
    }
    result
}

async fn close_session(connection: &Connection, session: &OwnedObjectPath) -> Result<(), String> {
    let proxy = session_proxy(connection, session).await?;
    timeout(PORTAL_CALL_TIMEOUT, proxy.call::<_, _, ()>("Close", &()))
        .await
        .map_err(|_| "closing failed portal session timed out")?
        .map_err(|error| format!("closing failed portal session: {error}"))
}

async fn open_pipewire_remotes(
    screen: &Proxy<'_>,
    session: &OwnedObjectPath,
    count: usize,
) -> Result<Vec<OwnedFd>, String> {
    let mut fds = Vec::with_capacity(count);
    for _ in 0..count {
        let options = HashMap::<String, OwnedValue>::new();
        let fd: zbus::zvariant::OwnedFd = timeout(
            PORTAL_CALL_TIMEOUT,
            screen.call("OpenPipeWireRemote", &(session, &options)),
        )
        .await
        .map_err(|_| "opening portal PipeWire remote timed out")?
        .map_err(|error| format!("opening portal PipeWire remote: {error}"))?;
        fds.push(
            OwnedFd::try_from(fd)
                .map_err(|error| format!("converting portal PipeWire fd: {error}"))?,
        );
    }
    Ok(fds)
}

async fn portal_proxy<'a>(
    connection: &'a Connection,
    interface: &'a str,
) -> Result<Proxy<'a>, String> {
    timeout(
        PORTAL_CALL_TIMEOUT,
        Proxy::new(connection, DESTINATION, ROOT, interface),
    )
    .await
    .map_err(|_| format!("portal {interface} proxy timed out"))?
    .map_err(|error| format!("portal {interface} unavailable: {error}"))
}

async fn session_proxy<'a>(
    connection: &'a Connection,
    session: &OwnedObjectPath,
) -> Result<Proxy<'a>, String> {
    timeout(
        PORTAL_CALL_TIMEOUT,
        Proxy::new(
            connection,
            DESTINATION,
            session.clone(),
            "org.freedesktop.portal.Session",
        ),
    )
    .await
    .map_err(|_| "portal session proxy timed out")?
    .map_err(|error| format!("portal session proxy failed: {error}"))
}

async fn cursor_mode(screen: &Proxy<'_>) -> Result<Option<u32>, String> {
    let modes: Result<u32, _> = match timeout(
        PORTAL_CALL_TIMEOUT,
        screen.get_property("AvailableCursorModes"),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => return Ok(None),
    };
    let Ok(modes) = modes else { return Ok(None) };
    if modes & 2 != 0 {
        return Ok(Some(2));
    }
    if modes & 4 != 0 && cursor_metadata_supported() {
        return Ok(Some(4));
    }
    Ok((modes & 1 != 0).then_some(1))
}

pub fn cursor_metadata_supported() -> bool {
    gstreamer_python().is_some()
}

fn cursor_metadata_capture(
    streams: &[PortalStream],
    stream_fds: &[(i32, i32)],
    quality: u8,
    width: u32,
    height: u32,
    origin_x: i32,
    origin_y: i32,
    logical_width: u32,
    logical_height: u32,
    max_dimension: u32,
) -> Result<(Vec<u8>, Option<(i32, i32)>), String> {
    const HELPER: &str = r#"
import gi, json, struct, sys, time
gi.require_version('Gst', '1.0')
gi.require_version('GstVideo', '1.0')
from gi.repository import Gst, GstVideo
Gst.init(None)
config = json.loads(sys.argv[1])
parts = []
if len(config['sources']) == 1:
 source = config['sources'][0]
 parts.extend([
  f"pipewiresrc fd={source['fd']} {source['target']} num-buffers=1 ! tee name=t0",
  't0. ! queue ! appsink name=metadata0 max-buffers=1 sync=false',
  f"t0. ! queue ! videoconvert ! videoscale ! video/x-raw,width={config['width']},height={config['height']} ! {config['encoder']} {config['encoder_quality']} ! appsink name=jpeg max-buffers=1 sync=false",
 ])
else:
 for index, source in enumerate(config['sources']):
  parts.extend([
   f"pipewiresrc fd={source['fd']} {source['target']} num-buffers=1 ! tee name=t{index}",
   f't{index}. ! queue ! appsink name=metadata{index} max-buffers=1 sync=false',
   f"t{index}. ! queue ! videoconvert ! videoscale ! video/x-raw,width={source['width']},height={source['height']} ! comp.sink_{index}",
   f"comp.sink_{index}::xpos={source['x']} comp.sink_{index}::ypos={source['y']}",
  ])
 parts.append(
  f"compositor name=comp background=1 ! videoscale ! video/x-raw,width={config['width']},height={config['height']} "
  f"! videoconvert ! {config['encoder']} {config['encoder_quality']} ! appsink name=jpeg max-buffers=1 sync=false")
pipeline = Gst.parse_launch(' '.join(parts))
metadata = [pipeline.get_by_name(f'metadata{i}') for i in range(len(config['sources']))]
jpeg = pipeline.get_by_name('jpeg')
pipeline.set_state(Gst.State.PLAYING)
try:
 deadline = time.monotonic() + 3
 cursor = (-2147483648, -2147483648)
 for index, sink in enumerate(metadata):
  sample = sink.try_pull_sample(max(0, int((deadline - time.monotonic()) * Gst.SECOND)))
  if sample is None: raise RuntimeError('PipeWire metadata capture timed out')
  meta = sample.get_buffer().get_meta(GstVideo.video_region_of_interest_meta_api_get_type())
  if cursor[0] == -2147483648 and meta is not None and meta.roi_type == 'cursor':
   structure = sample.get_caps().get_structure(0)
   frame_width = structure.get_value('width')
   frame_height = structure.get_value('height')
   source = config['sources'][index]
   if frame_width > 0 and frame_height > 0 and 0 <= meta.x < frame_width and 0 <= meta.y < frame_height:
    local_x = int(meta.x * source['logical_width'] / frame_width)
    local_y = int(meta.y * source['logical_height'] / frame_height)
    cursor = (source['origin_x'] + local_x, source['origin_y'] + local_y)
 encoded = jpeg.try_pull_sample(max(0, int((deadline - time.monotonic()) * Gst.SECOND)))
 if encoded is None: raise RuntimeError('PipeWire compositor capture timed out')
 jpeg_buffer = encoded.get_buffer()
 ok, mapped = jpeg_buffer.map(Gst.MapFlags.READ)
 if not ok: raise RuntimeError('cannot read encoded PipeWire frame')
 try: sys.stdout.buffer.write(mapped.data + struct.pack('>ii', *cursor))
 finally: jpeg_buffer.unmap(mapped)
finally:
 pipeline.set_state(Gst.State.NULL)
"#;
    let (scale, _, _) = capture_layout_size(logical_width, logical_height, max_dimension);
    let sources = streams
        .iter()
        .zip(stream_fds)
        .map(|(stream, (_, fd))| {
            serde_json::json!({
                "fd": fd,
                "target": stream.target(),
                "width": scaled(stream.width, scale),
                "height": scaled(stream.height, scale),
                "logical_width": stream.width,
                "logical_height": stream.height,
                "origin_x": stream.origin_x,
                "origin_y": stream.origin_y,
                "x": scaled_offset(i64::from(stream.origin_x) - i64::from(origin_x), scale),
                "y": scaled_offset(i64::from(stream.origin_y) - i64::from(origin_y), scale),
            })
        })
        .collect::<Vec<_>>();
    let encoder =
        jpeg_encoder().ok_or("GStreamer needs jpegenc or avenc_mjpeg to capture portal frames")?;
    let config = serde_json::json!({"sources":sources,"encoder":encoder.element(),"encoder_quality":encoder.quality(quality),"width":width,"height":height}).to_string();
    let child_fds = stream_fds.to_vec();
    let python = gstreamer_python().ok_or("Python GI GStreamer video bindings are unavailable")?;
    let mut command = crate::limited_command("5s", python);
    // SAFETY: dup2 is async-signal-safe; the source descriptors stay open until wait completes.
    unsafe {
        command.pre_exec(move || {
            for &(source, target) in &child_fds {
                if dup2(source, target) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    command
        .args(["-c", HELPER, &config])
        .stdin(std::process::Stdio::null());
    let (mut output, status) = crate::bounded_command_output(&mut command, crate::MAX_IMAGE + 8)
        .map_err(|error| format!("PipeWire cursor capture failed: {error}"))?;
    if !status.success() || output.len() < 8 {
        return Err(format!(
            "PipeWire cursor capture failed with status {status}"
        ));
    }
    let metadata = output.split_off(output.len() - 8);
    let (x, y) = (
        i32::from_be_bytes(metadata[..4].try_into().unwrap()),
        i32::from_be_bytes(metadata[4..].try_into().unwrap()),
    );
    let pointer = (x != i32::MIN && y != i32::MIN).then_some((x, y));
    Ok((output, pointer))
}

async fn request<B>(
    connection: &Connection,
    proxy: &Proxy<'_>,
    method: &str,
    token: &str,
    body: &B,
) -> Result<HashMap<String, OwnedValue>, PortalStartError>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let predicted = request_path(connection, token)?;
    let request_proxy = timeout(
        PORTAL_CALL_TIMEOUT,
        Proxy::new(connection, DESTINATION, predicted.as_str(), REQUEST_IFACE),
    )
    .await
    .map_err(|_| PortalStartError::Timeout("portal request proxy timed out".into()))?
    .map_err(|error| PortalStartError::Failed(format!("portal request proxy: {error}")))?;
    let mut responses = timeout(
        PORTAL_CALL_TIMEOUT,
        request_proxy.receive_signal("Response"),
    )
    .await
    .map_err(|_| PortalStartError::Timeout("subscribing to portal response timed out".into()))?
    .map_err(|error| {
        PortalStartError::Failed(format!("subscribing to portal response: {error}"))
    })?;
    let predicted_path = OwnedObjectPath::try_from(predicted.as_str())
        .map_err(|error| PortalStartError::Failed(error.to_string()))?;
    let mut request_guard = PortalRequestGuard {
        connection: connection.clone(),
        request: predicted_path,
        active: true,
    };
    let actual: OwnedObjectPath = match timeout(PORTAL_CALL_TIMEOUT, proxy.call(method, body)).await
    {
        Err(_) => {
            return Err(PortalStartError::Timeout(format!(
                "portal {method} call timed out"
            )));
        }
        Ok(Err(error)) => {
            request_guard.active = false;
            return Err(PortalStartError::Failed(format!(
                "portal {method} failed: {error}"
            )));
        }
        Ok(Ok(actual)) => actual,
    };
    request_guard.request = actual.clone();
    let results = if request_handle_matches(&predicted, &actual) {
        wait_response(&mut responses).await
    } else {
        let actual_proxy = timeout(
            PORTAL_CALL_TIMEOUT,
            Proxy::new(connection, DESTINATION, actual, REQUEST_IFACE),
        )
        .await
        .map_err(|_| PortalStartError::Timeout("portal returned request proxy timed out".into()))?
        .map_err(|error| {
            PortalStartError::Failed(format!("portal returned request proxy: {error}"))
        })?;
        let mut actual_responses =
            timeout(PORTAL_CALL_TIMEOUT, actual_proxy.receive_signal("Response"))
                .await
                .map_err(|_| {
                    PortalStartError::Timeout(
                        "subscribing to returned portal request timed out".into(),
                    )
                })?
                .map_err(|error| {
                    PortalStartError::Failed(format!(
                        "subscribing to returned portal request: {error}"
                    ))
                })?;
        wait_response(&mut actual_responses).await
    };
    if results.is_ok() || matches!(&results, Err(PortalStartError::Denied(_))) {
        request_guard.active = false;
    }
    results
}

fn request_handle_matches(predicted: &str, actual: &OwnedObjectPath) -> bool {
    actual.as_str() == predicted
}

#[cfg(test)]
mod tests {
    use super::{
        DESTINATION, PortalSession, PortalStartError, PortalStream, REQUEST_IFACE, ROOT,
        portal_layout, portal_response as decode_portal_response, request_handle_matches,
    };
    use std::{
        collections::HashMap,
        fs::File,
        os::fd::OwnedFd,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use zbus::{
        Connection,
        message::Header,
        object_server::SignalEmitter,
        zvariant::{OwnedObjectPath, OwnedValue, StructureBuilder, Value},
    };

    static PORTAL_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct MockScreenCast(Arc<AtomicUsize>);

    #[zbus::interface(name = "org.freedesktop.portal.ScreenCast")]
    impl MockScreenCast {
        #[zbus(property)]
        fn available_cursor_modes(&self) -> u32 {
            2
        }

        async fn create_session(
            &self,
            options: HashMap<String, OwnedValue>,
            #[zbus(connection)] connection: &Connection,
            #[zbus(header)] header: Header<'_>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            let session = OwnedObjectPath::try_from(format!("{ROOT}/session/mock"))
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            connection
                .object_server()
                .at(session.as_str(), MockSession(Arc::clone(&self.0)))
                .await
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            let mut results = HashMap::new();
            results.insert(
                "session_handle".into(),
                OwnedValue::try_from(Value::new(session))
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?,
            );
            emit_response(connection, &header, &options, 0, results).await
        }

        async fn select_sources(
            &self,
            _session: OwnedObjectPath,
            options: HashMap<String, OwnedValue>,
            #[zbus(connection)] connection: &Connection,
            #[zbus(header)] header: Header<'_>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            emit_response(connection, &header, &options, 0, HashMap::new()).await
        }

        async fn start(
            &self,
            _session: OwnedObjectPath,
            _parent_window: String,
            options: HashMap<String, OwnedValue>,
            #[zbus(connection)] connection: &Connection,
            #[zbus(header)] header: Header<'_>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            let mut stream_properties: HashMap<String, OwnedValue> = HashMap::new();
            stream_properties.insert(
                "size".into(),
                OwnedValue::try_from(Value::new(
                    StructureBuilder::new()
                        .add_field(1280i32)
                        .add_field(720i32)
                        .build()
                        .unwrap(),
                ))
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?,
            );
            stream_properties.insert(
                "position".into(),
                OwnedValue::try_from(Value::new(
                    StructureBuilder::new()
                        .add_field(0i32)
                        .add_field(0i32)
                        .build()
                        .unwrap(),
                ))
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?,
            );
            stream_properties.insert("source_type".into(), OwnedValue::from(1u32));
            let streams = vec![(1u32, stream_properties)];
            let mut results = HashMap::new();
            results.insert(
                "streams".into(),
                OwnedValue::try_from(Value::new(streams))
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?,
            );
            emit_response(connection, &header, &options, 0, results).await
        }

        fn open_pipe_wire_remote(
            &self,
            _session: OwnedObjectPath,
            _options: HashMap<String, OwnedValue>,
        ) -> zbus::fdo::Result<zbus::zvariant::OwnedFd> {
            let fd: OwnedFd = File::open("/dev/null")
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                .into();
            Ok(fd.into())
        }
    }

    struct MockSession(Arc<AtomicUsize>);

    #[zbus::interface(name = "org.freedesktop.portal.Session")]
    impl MockSession {
        fn close(&self) -> zbus::fdo::Result<()> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        #[zbus(signal)]
        async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    }

    async fn emit_response(
        connection: &Connection,
        header: &Header<'_>,
        options: &HashMap<String, OwnedValue>,
        code: u32,
        results: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let token: String = options
            .get("handle_token")
            .ok_or_else(|| zbus::fdo::Error::Failed("portal request omitted handle_token".into()))?
            .try_clone()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .try_into()
            .map_err(|error: zbus::zvariant::Error| zbus::fdo::Error::Failed(error.to_string()))?;
        let sender = header.sender().unwrap().to_string();
        let path = format!(
            "{ROOT}/request/{}/{token}",
            sender.trim_start_matches(':').replace([':', '.'], "_")
        );
        connection
            .emit_signal(
                Some(sender.as_str()),
                path.as_str(),
                REQUEST_IFACE,
                "Response",
                &(code, results),
            )
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        OwnedObjectPath::try_from(path).map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    struct MockRequest(Arc<AtomicUsize>);

    #[zbus::interface(name = "org.freedesktop.portal.Request")]
    impl MockRequest {
        fn close(&self) -> zbus::fdo::Result<()> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[tokio::test]
    async fn dropped_portal_request_is_closed_before_cleanup_returns() {
        let _test_lock = PORTAL_TEST_LOCK.lock().await;
        let close_count = Arc::new(AtomicUsize::new(0));
        let path = OwnedObjectPath::try_from(format!("{ROOT}/request/mock/cancelled")).unwrap();
        let _service = zbus::connection::Builder::session()
            .unwrap()
            .name(DESTINATION)
            .unwrap()
            .serve_at(path.as_str(), MockRequest(Arc::clone(&close_count)))
            .unwrap()
            .build()
            .await
            .unwrap();
        let connection = Connection::session().await.unwrap();
        drop(super::PortalRequestGuard {
            connection,
            request: path,
            active: true,
        });

        super::await_request_cleanups().await;

        assert_eq!(close_count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn mock_portal_exercises_screencast_fallback_and_session_close() {
        let _test_lock = PORTAL_TEST_LOCK.lock().await;
        let close_count = Arc::new(AtomicUsize::new(0));
        let _service = zbus::connection::Builder::session()
            .unwrap()
            .name(DESTINATION)
            .unwrap()
            .serve_at(ROOT, MockScreenCast(Arc::clone(&close_count)))
            .unwrap()
            .build()
            .await
            .unwrap();

        let portal = PortalSession::start(false).await.unwrap();
        assert!(!portal.remote_desktop_active);
        assert_eq!((portal.logical_width, portal.logical_height), (1280, 720));
        assert_eq!(portal.source.as_deref(), Some("monitor"));
        portal.close().await.unwrap();
        assert_eq!(close_count.load(Ordering::Relaxed), 1);
    }

    fn stream(width: u32, height: u32, origin: Option<(i32, i32)>) -> PortalStream {
        let (origin_x, origin_y) = origin.unwrap_or((0, 0));
        PortalStream {
            id: 1,
            pipewire_serial: None,
            width,
            height,
            origin_x,
            origin_y,
            position_available: origin.is_some(),
            source: Some("monitor".into()),
            mapping_id: None,
        }
    }

    #[test]
    fn detects_portals_that_return_a_legacy_request_handle() {
        let predicted = "/org/freedesktop/portal/desktop/request/app/token";
        let matching = OwnedObjectPath::try_from(predicted.to_owned()).unwrap();
        let legacy = OwnedObjectPath::try_from(
            "/org/freedesktop/portal/desktop/request/app/legacy".to_owned(),
        )
        .unwrap();
        assert!(request_handle_matches(predicted, &matching));
        assert!(!request_handle_matches(predicted, &legacy));
    }

    #[test]
    fn compositor_fallback_never_swallows_portal_denial_or_timeout() {
        assert!(
            PortalStartError::Failed("portal is unavailable".into()).allows_compositor_fallback()
        );
        assert!(!PortalStartError::Denied("user cancelled".into()).allows_compositor_fallback());
        assert!(!PortalStartError::Timeout("portal timed out".into()).allows_compositor_fallback());
    }

    #[test]
    fn nonzero_portal_response_codes_are_denials() {
        assert!(matches!(decode_portal_response(0, HashMap::new()), Ok(_)));
        let error = decode_portal_response(1, HashMap::new()).unwrap_err();
        assert!(matches!(error, PortalStartError::Denied(_)));
        assert!(!error.allows_compositor_fallback());
    }

    #[test]
    fn computes_layout_bounds_for_positioned_multi_monitor_streams() {
        let mut streams = [
            stream(100, 80, Some((-100, 20))),
            stream(200, 120, Some((0, -10))),
        ];
        let layout = portal_layout(&mut streams, true).unwrap();
        assert_eq!(
            (
                layout.origin_x,
                layout.origin_y,
                layout.width,
                layout.height
            ),
            (-100, -10, 300, 120)
        );
        assert!(layout.position_available);
        assert!(!layout.synthetic_layout);
        assert_eq!(layout.source.as_deref(), Some("monitor"));
    }

    #[test]
    fn tiles_unpositioned_multi_monitor_streams_and_marks_synthetic_layout() {
        let mut streams = [stream(100, 80, None), stream(200, 120, None)];
        let layout = portal_layout(&mut streams, true).unwrap();
        assert_eq!(
            (
                layout.origin_x,
                layout.origin_y,
                layout.width,
                layout.height
            ),
            (0, 0, 300, 120)
        );
        assert!(!layout.position_available);
        assert!(layout.synthetic_layout);
        assert_eq!(streams[1].origin_x, 100);
    }

    #[test]
    fn rejects_multiple_streams_when_compositor_capture_is_unavailable() {
        let mut streams = [
            stream(100, 80, Some((0, 0))),
            stream(100, 80, Some((100, 0))),
        ];
        assert!(portal_layout(&mut streams, false).is_err());
    }
}

async fn session_connection() -> Result<Connection, String> {
    timeout(PORTAL_CALL_TIMEOUT, Connection::session())
        .await
        .map_err(|_| "session D-Bus connection timed out".to_owned())?
        .map_err(|error| format!("session D-Bus unavailable: {error}"))
}

fn request_path(connection: &Connection, token: &str) -> Result<String, String> {
    let sender = connection
        .unique_name()
        .ok_or("D-Bus connection has no unique name")?
        .to_string();
    let sender = sender.trim_start_matches(':').replace([':', '.'], "_");
    Ok(format!("{ROOT}/request/{sender}/{token}"))
}

async fn wait_response<S>(
    responses: &mut S,
) -> Result<HashMap<String, OwnedValue>, PortalStartError>
where
    S: Stream<Item = zbus::Message> + Unpin,
{
    let message = timeout(PORTAL_REQUEST_TIMEOUT, responses.next())
        .await
        .map_err(|_| PortalStartError::Timeout("portal request timed out".into()))?
        .ok_or_else(|| PortalStartError::Failed("portal response stream ended".into()))?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = message
        .body()
        .deserialize()
        .map_err(|error| PortalStartError::Failed(format!("decoding portal response: {error}")))?;
    portal_response(code, results)
}

fn portal_response(
    code: u32,
    results: HashMap<String, OwnedValue>,
) -> Result<HashMap<String, OwnedValue>, PortalStartError> {
    if code == 0 {
        Ok(results)
    } else {
        Err(PortalStartError::Denied(format!(
            "portal request was denied or cancelled (response {code})"
        )))
    }
}

fn options_with_token() -> Result<(String, HashMap<String, OwnedValue>), String> {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let value = Value::from(token.as_str());
    let mut result = HashMap::new();
    result.insert(
        "handle_token".into(),
        OwnedValue::try_from(value).map_err(|error| error.to_string())?,
    );
    Ok((token, result))
}

fn object_path_result(
    results: &HashMap<String, OwnedValue>,
    key: &str,
) -> Result<OwnedObjectPath, String> {
    let value = results
        .get(key)
        .ok_or_else(|| format!("portal response omitted {key}"))?;
    if let Ok(path) =
        OwnedObjectPath::try_from(value.try_clone().map_err(|error| error.to_string())?)
    {
        return Ok(path);
    }
    let path: String = value
        .try_clone()
        .map_err(|error| error.to_string())?
        .try_into()
        .map_err(|error: zbus::zvariant::Error| error.to_string())?;
    OwnedObjectPath::try_from(path)
        .map_err(|error| format!("portal returned an invalid {key}: {error}"))
}

fn stream_result(results: &HashMap<String, OwnedValue>) -> Result<Vec<PortalStream>, String> {
    let value = results
        .get("streams")
        .ok_or("portal response omitted stream list")?
        .try_clone()
        .map_err(|error| error.to_string())?;
    let streams: Vec<(u32, HashMap<String, OwnedValue>)> = value
        .try_into()
        .map_err(|error: zbus::zvariant::Error| error.to_string())?;
    if streams.is_empty() {
        return Err("portal returned no display stream".into());
    }
    streams
        .into_iter()
        .map(|(id, properties)| {
            // Prefer RemoteDesktop's logical input bounds; ScreenCast `size` is compositor-space, not necessarily PipeWire pixel dimensions.
            let (width, height) = match properties
                .get("logical_size")
                .or_else(|| properties.get("size"))
            {
                Some(size) => {
                    let (width, height): (i32, i32) = size
                        .try_clone()
                        .map_err(|error| error.to_string())?
                        .try_into()
                        .map_err(|error: zbus::zvariant::Error| error.to_string())?;
                    if width <= 0 || height <= 0 {
                        return Err("portal returned invalid display dimensions".into());
                    }
                    (width as u32, height as u32)
                }
                None => (0, 0),
            };
            let position = properties
                .get("position")
                .and_then(|value| value.try_clone().ok())
                .and_then(|value| <(i32, i32)>::try_from(value).ok());
            let mapping_id = properties
                .get("mapping_id")
                .and_then(|value| value.try_clone().ok())
                .and_then(|value| String::try_from(value).ok());
            let pipewire_serial = properties
                .get("pipewire-serial")
                .and_then(|value| value.try_clone().ok())
                .and_then(|value| u64::try_from(value).ok());
            let source_type = properties
                .get("source_type")
                .and_then(|value| value.try_clone().ok())
                .and_then(|value| u32::try_from(value).ok());
            let source = match source_type {
                Some(1) => Some("monitor"),
                Some(2) => Some("window"),
                Some(4) => Some("virtual monitor"),
                _ => None,
            }
            .map(str::to_owned);
            let (origin_x, origin_y) = position.unwrap_or((0, 0));
            Ok(PortalStream {
                id,
                pipewire_serial,
                width,
                height,
                origin_x,
                origin_y,
                position_available: position.is_some(),
                source,
                mapping_id,
            })
        })
        .collect()
}

impl PortalStream {
    fn target(&self) -> String {
        self.pipewire_serial.map_or_else(
            || format!("path={}", self.id),
            |serial| format!("target-object={serial}"),
        )
    }
}

async fn compositor_available_async() -> Result<bool, String> {
    tokio::task::spawn_blocking(compositor_available)
        .await
        .map_err(|error| format!("checking GStreamer compositor availability: {error}"))
}

fn portal_layout(
    streams: &mut [PortalStream],
    allow_multiple: bool,
) -> Result<PortalLayout, String> {
    if streams.is_empty() {
        return Err("portal returned no display stream".into());
    }
    if streams.len() > MAX_PORTAL_STREAMS {
        return Err("portal selected more than 16 streams".into());
    }
    if streams.len() > 1 && !allow_multiple {
        return Err("multiple portal streams require the GStreamer compositor plugin".into());
    }
    if streams.len() > 1
        && streams
            .iter()
            .any(|stream| stream.width == 0 || stream.height == 0)
    {
        return Err("portal omitted dimensions for one of the selected streams".into());
    }
    if streams
        .iter()
        .any(|stream| stream.width > 32768 || stream.height > 32768)
    {
        return Err("portal stream dimensions exceed the 32768-pixel safety limit".into());
    }
    let all_positions = streams.iter().all(|stream| stream.position_available);
    if !all_positions {
        let mut x = 0_i32;
        for stream in streams.iter_mut() {
            stream.origin_x = x;
            stream.origin_y = 0;
            x = x
                .checked_add(
                    i32::try_from(stream.width)
                        .map_err(|_| "portal stream width is out of range")?,
                )
                .ok_or("portal stream layout is out of range")?;
        }
    }
    let origin_x = streams
        .iter()
        .map(|stream| stream.origin_x)
        .min()
        .unwrap_or(0);
    let origin_y = streams
        .iter()
        .map(|stream| stream.origin_y)
        .min()
        .unwrap_or(0);
    let right = streams
        .iter()
        .map(|stream| i64::from(stream.origin_x) + i64::from(stream.width))
        .max()
        .unwrap_or(0);
    let bottom = streams
        .iter()
        .map(|stream| i64::from(stream.origin_y) + i64::from(stream.height))
        .max()
        .unwrap_or(0);
    let width = u32::try_from(right - i64::from(origin_x))
        .map_err(|_| "portal layout width is out of range")?;
    let height = u32::try_from(bottom - i64::from(origin_y))
        .map_err(|_| "portal layout height is out of range")?;
    if width > 32768 || height > 32768 {
        return Err("portal layout exceeds the 32768-pixel safety limit".into());
    }
    let source = streams
        .first()
        .and_then(|stream| stream.source.clone())
        .filter(|source| {
            streams
                .iter()
                .all(|stream| stream.source.as_ref() == Some(source))
        });
    Ok(PortalLayout {
        origin_x,
        origin_y,
        width,
        height,
        position_available: all_positions,
        synthetic_layout: streams.len() > 1 && !all_positions,
        source,
    })
}

fn capture_layout_size(width: u32, height: u32, max_dimension: u32) -> (f64, u32, u32) {
    let scale = (f64::from(max_dimension) / f64::from(width.max(height))).min(1.0);
    (scale, scaled(width, scale), scaled(height, scale))
}

fn scaled(dimension: u32, scale: f64) -> u32 {
    (f64::from(dimension) * scale).round().max(1.0) as u32
}

fn scaled_offset(offset: i64, scale: f64) -> i32 {
    (offset as f64 * scale)
        .round()
        .clamp(0.0, f64::from(i32::MAX)) as i32
}

fn compositor_available() -> bool {
    crate::limited_command("3s", "gst-inspect-1.0")
        .arg("compositor")
        .output()
        .is_ok_and(|output| output.status.success())
}
