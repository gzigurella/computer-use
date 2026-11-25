use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Write},
    os::fd::{AsFd, AsRawFd},
    os::unix::net::UnixStream,
    sync::{OnceLock, mpsc},
    thread,
    time::{Duration, Instant},
};

use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop,
    protocol::{wl_registry, wl_seat},
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1::ExtDataControlSourceV1,
};

const MAX_CLIPBOARD_BYTES: usize = 64 * 1024;
const CLIPBOARD_MIMES: [&str; 3] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];

enum Request {
    Read(mpsc::SyncSender<Result<String, String>>),
    Write(String, mpsc::SyncSender<Result<(), String>>),
}

static WORKER: OnceLock<Result<mpsc::Sender<Request>, String>> = OnceLock::new();

#[derive(Default)]
struct State {
    manager: Option<ExtDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    selection: Option<ExtDataControlOfferV1>,
    offers: HashMap<wayland_client::backend::ObjectId, Vec<String>>,
    sources: Vec<ExtDataControlSourceV1>,
    local_text: Option<Vec<u8>>,
    setting_selection: bool,
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
                "ext_data_control_manager_v1" if state.manager.is_none() => {
                    state.manager = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(9), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    wayland_client::event_created_child!(
        State,
        ExtDataControlDeviceV1,
        [0 => (ExtDataControlOfferV1, ())]
    );

    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: <ExtDataControlDeviceV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::Event::DataOffer { id } => {
                state.offers.entry(id.id()).or_default();
            }
            wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::Event::Selection { id } => {
                if let Some(previous) = state.selection.take() {
                    state.offers.remove(&previous.id());
                    previous.destroy();
                }
                state.selection = id;
                if !state.setting_selection {
                    state.local_text = None;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: <ExtDataControlOfferV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offers.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, Vec<u8>> for State {
    fn event(
        _: &mut Self,
        source: &ExtDataControlSourceV1,
        event: <ExtDataControlSourceV1 as wayland_client::Proxy>::Event,
        data: &Vec<u8>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wayland_protocols::ext::data_control::v1::client::ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                if text_mime(&mime_type) {
                    let mut file = File::from(fd);
                    let _ = file.write_all(data);
                }
            }
            wayland_protocols::ext::data_control::v1::client::ext_data_control_source_v1::Event::Cancelled => source.destroy(),
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ExtDataControlManagerV1);

pub fn available() -> bool {
    discover().is_ok()
}

pub fn read_text() -> Result<String, String> {
    request(Request::Read)
}

pub fn write_text(text: &str) -> Result<(), String> {
    if text.len() > MAX_CLIPBOARD_BYTES {
        return Err("clipboard text exceeds 64 KiB".into());
    }
    request(|reply| Request::Write(text.to_owned(), reply))
}

fn request<T>(
    make_request: impl FnOnce(mpsc::SyncSender<Result<T, String>>) -> Request,
) -> Result<T, String> {
    let sender = WORKER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel();
            thread::Builder::new()
                .name("wayland-clipboard".into())
                .spawn(move || run_worker(receiver))
                .map(|_| sender)
                .map_err(|error| format!("could not start Wayland clipboard worker: {error}"))
        })
        .as_ref()
        .map_err(Clone::clone)?;
    let (reply, result) = mpsc::sync_channel(1);
    sender
        .send(make_request(reply))
        .map_err(|_| "native Wayland clipboard worker stopped".to_owned())?;
    result
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "native Wayland clipboard request timed out".to_owned())?
}

fn run_worker(receiver: mpsc::Receiver<Request>) {
    let (connection, mut queue, mut state) = match discover() {
        Ok(result) => result,
        Err(error) => {
            for request in receiver {
                fail_request(request, error.clone());
            }
            return;
        }
    };
    let qh = queue.handle();
    let seat = state
        .seat
        .as_ref()
        .expect("discovery requires a seat")
        .clone();
    let manager = state
        .manager
        .as_ref()
        .expect("discovery requires a data-control manager")
        .clone();
    let device = manager.get_data_device(&seat, &qh, ());
    if queue.roundtrip(&mut state).is_err() {
        return;
    }

    loop {
        match receiver.try_recv() {
            Ok(request) => handle_request(
                request,
                &connection,
                &mut queue,
                &mut state,
                &device,
                &manager,
                &qh,
            ),
            Err(mpsc::TryRecvError::Disconnected) => return,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if queue.dispatch_pending(&mut state).is_err() || queue.flush().is_err() {
            return;
        }
        state.sources.retain(|source| source.is_alive());
        let mut fd = crate::virtual_pointer::PollFd {
            fd: queue.as_fd().as_raw_fd(),
            events: 1,
            revents: 0,
        };
        let ready = unsafe { crate::virtual_pointer::poll(&mut fd, 1, 25) };
        if ready > 0 && queue.blocking_dispatch(&mut state).is_err() {
            return;
        }
        if ready < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
    }
}

fn handle_request(
    request: Request,
    connection: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    device: &ExtDataControlDeviceV1,
    manager: &ExtDataControlManagerV1,
    qh: &QueueHandle<State>,
) {
    match request {
        Request::Read(reply) => {
            let result = read_selection(connection, queue, state);
            let _ = reply.send(result);
        }
        Request::Write(text, reply) => {
            let result = set_selection(queue, state, device, manager, qh, text.into_bytes());
            let _ = reply.send(result);
        }
    }
}

fn set_selection(
    queue: &mut EventQueue<State>,
    state: &mut State,
    device: &ExtDataControlDeviceV1,
    manager: &ExtDataControlManagerV1,
    qh: &QueueHandle<State>,
    text: Vec<u8>,
) -> Result<(), String> {
    let source = manager.create_data_source(qh, text.clone());
    for mime in CLIPBOARD_MIMES {
        source.offer(mime.to_owned());
    }
    device.set_selection(Some(&source));
    state.sources.push(source);
    state.setting_selection = true;
    let result = queue
        .roundtrip(state)
        .map(|_| ())
        .map_err(|error| format!("setting Wayland clipboard selection: {error}"));
    state.setting_selection = false;
    result?;
    state.local_text = Some(text);
    Ok(())
}

fn read_selection(
    connection: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
) -> Result<String, String> {
    if let Some(text) = &state.local_text {
        return String::from_utf8(text.clone())
            .map_err(|_| "clipboard text is not valid UTF-8".into());
    }
    queue
        .roundtrip(state)
        .map_err(|error| format!("reading Wayland clipboard selection: {error}"))?;
    let offer = state
        .selection
        .as_ref()
        .ok_or("clipboard is empty")?
        .clone();
    let types = state
        .offers
        .get(&offer.id())
        .ok_or("clipboard has no offered types")?;
    let mime = CLIPBOARD_MIMES
        .iter()
        .find(|candidate| {
            types
                .iter()
                .any(|value| value.eq_ignore_ascii_case(candidate))
        })
        .ok_or("clipboard does not contain a supported text format")?;
    let (mut read_end, write_end) = UnixStream::pair()
        .map_err(|error| format!("creating clipboard transfer stream: {error}"))?;
    read_end
        .set_nonblocking(true)
        .map_err(|error| format!("configuring clipboard transfer stream: {error}"))?;
    offer.receive((*mime).to_owned(), write_end.as_fd());
    connection
        .flush()
        .map_err(|error| format!("requesting Wayland clipboard data: {error}"))?;
    drop(write_end);
    read_stream(&mut read_end, queue, state)
}

fn read_stream(
    stream: &mut UnixStream,
    queue: &mut EventQueue<State>,
    state: &mut State,
) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => {
                if data.len() + size > MAX_CLIPBOARD_BYTES {
                    return Err("clipboard text exceeds 64 KiB".into());
                }
                data.extend_from_slice(&buffer[..size]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err("timed out receiving Wayland clipboard data".into());
                }
                let mut fds = [
                    crate::virtual_pointer::PollFd {
                        fd: stream.as_fd().as_raw_fd(),
                        events: 1,
                        revents: 0,
                    },
                    crate::virtual_pointer::PollFd {
                        fd: queue.as_fd().as_raw_fd(),
                        events: 1,
                        revents: 0,
                    },
                ];
                let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
                let ready = unsafe {
                    crate::virtual_pointer::poll(fds.as_mut_ptr(), fds.len(), timeout_ms)
                };
                if ready == 0 {
                    return Err("timed out receiving Wayland clipboard data".into());
                }
                if ready < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    return Err(format!(
                        "waiting for clipboard data: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                if fds[1].revents != 0 {
                    queue
                        .dispatch_pending(state)
                        .map_err(|error| format!("serving Wayland clipboard data: {error}"))?;
                }
            }
            Err(error) => return Err(format!("receiving Wayland clipboard data: {error}")),
        }
    }
    String::from_utf8(data).map_err(|_| "clipboard text is not valid UTF-8".into())
}

fn discover() -> Result<(Connection, EventQueue<State>, State), String> {
    let connection = Connection::connect_to_env()
        .map_err(|error| format!("connecting to Wayland for clipboard access: {error}"))?;
    let mut queue = connection.new_event_queue();
    connection.display().get_registry(&queue.handle(), ());
    let mut state = State::default();
    queue
        .roundtrip(&mut state)
        .map_err(|error| format!("discovering Wayland clipboard protocols: {error}"))?;
    if state.manager.is_none() || state.seat.is_none() {
        return Err("compositor does not expose the ext-data-control clipboard protocol".into());
    }
    Ok((connection, queue, state))
}

fn fail_request(request: Request, error: String) {
    match request {
        Request::Read(reply) => {
            let _ = reply.send(Err(error));
        }
        Request::Write(_, reply) => {
            let _ = reply.send(Err(error));
        }
    }
}

fn text_mime(mime: &str) -> bool {
    CLIPBOARD_MIMES
        .iter()
        .any(|candidate| mime.eq_ignore_ascii_case(candidate))
}
