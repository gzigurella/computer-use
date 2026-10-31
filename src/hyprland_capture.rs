use std::{
    fs::File,
    os::fd::{AsFd, AsRawFd, FromRawFd},
    time::{Duration, Instant},
};

use jpeg_encoder::{ColorType, Encoder};
use memmap2::MmapMut;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

const MAX_CAPTURE_BYTES: usize = 256 * 1024 * 1024;

#[link(name = "c")]
unsafe extern "C" {
    fn memfd_create(name: *const std::ffi::c_char, flags: std::ffi::c_int) -> std::ffi::c_int;
}

#[derive(Default)]
struct State {
    manager: Option<ZwlrScreencopyManagerV1>,
    shm: Option<wl_shm::WlShm>,
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
    buffer: Option<(wl_shm::Format, u32, u32, u32)>,
    buffer_done: bool,
    ready: bool,
    failed: bool,
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
                "zwlr_screencopy_manager_v1" if state.manager.is_none() => {
                    state.manager = Some(registry.bind(name, version.min(3), qh, ()))
                }
                "wl_shm" if state.shm.is_none() => {
                    state.shm = Some(registry.bind(name, version.min(1), qh, ()))
                }
                "wl_output" => state
                    .outputs
                    .push((registry.bind(name, version.min(4), qh, ()), None)),
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some((_, output_name)) = state
                .outputs
                .iter_mut()
                .find(|(proxy, _)| proxy.id() == output.id())
        {
            *output_name = Some(name);
        }
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } if state.buffer.is_none() => state.buffer = Some((format, width, height, stride)),
            zwlr_screencopy_frame_v1::Event::BufferDone => state.buffer_done = true,
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => state.failed = true,
            _ => {}
        }
    }
}

fn buffer_info_complete(state: &State, frame_version: u32) -> bool {
    state.failed || state.buffer_done || frame_version < 3 && state.buffer.is_some()
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);

pub fn available() -> bool {
    let Ok(connection) = Connection::connect_to_env() else {
        return false;
    };
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    let mut state = State::default();
    queue.flush().is_ok()
        && wait_for(&mut queue, &mut state, Duration::from_secs(3), |state| {
            state.manager.is_some() && state.shm.is_some() && !state.outputs.is_empty()
        })
        .is_ok()
        && wait_for(&mut queue, &mut state, Duration::from_secs(3), |state| {
            state.outputs.iter().all(|(_, name)| name.is_some())
        })
        .is_ok()
        && state.manager.is_some()
        && state.shm.is_some()
        && state.outputs.iter().any(|(_, name)| name.is_some())
}

pub fn capture_output(
    name: &str,
    cursor: bool,
    quality: u8,
    max_dimension: u32,
    pointer_overlay: Option<(i32, i32, u32, u32)>,
) -> Result<Vec<u8>, String> {
    let connection = Connection::connect_to_env()
        .map_err(|error| format!("connecting to Wayland for capture: {error}"))?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    let mut state = State::default();
    queue
        .flush()
        .map_err(|error| format!("requesting Wayland capture globals: {error}"))?;
    wait_for(&mut queue, &mut state, Duration::from_secs(3), |state| {
        state.manager.is_some() && state.shm.is_some() && !state.outputs.is_empty()
    })
    .map_err(|error| format!("discovering Wayland capture globals: {error}"))?;
    wait_for(&mut queue, &mut state, Duration::from_secs(3), |state| {
        state.outputs.iter().all(|(_, name)| name.is_some())
    })
    .map_err(|error| format!("reading Wayland output names: {error}"))?;
    let manager = state
        .manager
        .as_ref()
        .ok_or("Hyprland does not expose wlr screencopy")?;
    let shm = state
        .shm
        .as_ref()
        .ok_or("Wayland does not expose shared-memory buffers")?
        .clone();
    let output = state
        .outputs
        .iter()
        .find_map(|(output, output_name)| {
            (output_name.as_deref() == Some(name)).then(|| output.clone())
        })
        .ok_or_else(|| format!("Hyprland output {name:?} is unavailable to Wayland screencopy"))?;
    let frame = manager.capture_output(
        i32::from(cursor && pointer_overlay.is_none()),
        &output,
        &qh,
        (),
    );
    let frame_version = frame.version();
    wait_for(&mut queue, &mut state, Duration::from_secs(5), |state| {
        buffer_info_complete(state, frame_version)
    })?;
    if state.failed {
        return Err("Hyprland rejected the screencopy request".into());
    }
    let (format, width, height, stride) = state
        .buffer
        .ok_or("Hyprland screencopy did not provide a shared-memory format")?;
    if width == 0
        || height == 0
        || stride
            < width
                .checked_mul(4)
                .ok_or("screencopy width is too large")?
    {
        return Err("Hyprland screencopy returned invalid frame geometry".into());
    }
    let size = usize::try_from(stride)
        .ok()
        .and_then(|stride| stride.checked_mul(height as usize))
        .filter(|size| *size <= MAX_CAPTURE_BYTES)
        .ok_or("Hyprland screencopy frame exceeds 256 MiB")?;
    let size_i32 = i32::try_from(size).map_err(|_| "Hyprland screencopy frame is too large")?;
    let fd = create_shm_file(size)?;
    let mapping = unsafe { MmapMut::map_mut(&fd) }
        .map_err(|error| format!("mapping the screencopy buffer: {error}"))?;
    let pool = shm.create_pool(fd.as_fd(), size_i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        format,
        &qh,
        (),
    );
    pool.destroy();
    frame.copy(&buffer);
    queue
        .flush()
        .map_err(|error| format!("requesting the Hyprland frame: {error}"))?;
    wait_for(&mut queue, &mut state, Duration::from_secs(5), |state| {
        state.ready || state.failed
    })?;
    buffer.destroy();
    frame.destroy();
    if state.failed {
        return Err("Hyprland failed to copy the requested frame".into());
    }
    let pointer_position =
        pointer_overlay.and_then(|(origin_x, origin_y, logical_width, logical_height)| {
            let (x, y) = crate::hyprland_cursor_position()?;
            let x = i64::from(x) - i64::from(origin_x);
            let y = i64::from(y) - i64::from(origin_y);
            if x < 0 || y < 0 || x >= i64::from(logical_width) || y >= i64::from(logical_height) {
                None
            } else {
                Some((x as u32, y as u32, logical_width, logical_height))
            }
        });
    encode_jpeg(
        &mapping,
        width,
        height,
        stride,
        format,
        quality,
        max_dimension,
        pointer_position,
    )
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
            .map_err(|error| format!("dispatching screencopy events: {error}"))?;
        if complete(state) {
            return Ok(());
        }
        queue
            .flush()
            .map_err(|error| format!("flushing screencopy requests: {error}"))?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("timed out waiting for the Hyprland screencopy frame".into());
        }
        let mut fd = crate::virtual_pointer::PollFd {
            fd: queue.as_fd().as_raw_fd(),
            events: 1,
            revents: 0,
        };
        let milliseconds = remaining.as_millis().min(i32::MAX as u128) as i32;
        let ready = unsafe { crate::virtual_pointer::poll(&mut fd, 1, milliseconds) };
        if ready == 0 {
            return Err("timed out waiting for the Hyprland screencopy frame".into());
        }
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!(
                "waiting for the Wayland screencopy socket: {error}"
            ));
        }
        queue
            .blocking_dispatch(state)
            .map_err(|error| format!("receiving the Hyprland screencopy frame: {error}"))?;
    }
}

fn create_shm_file(size: usize) -> Result<File, String> {
    let name = c"computer-use-screencopy";
    let raw_fd = unsafe { memfd_create(name.as_ptr(), 1) };
    if raw_fd < 0 {
        return Err(format!(
            "creating the screencopy shared-memory file: {}",
            std::io::Error::last_os_error()
        ));
    }
    let file = unsafe { File::from_raw_fd(raw_fd) };
    file.set_len(size as u64)
        .map_err(|error| format!("sizing the screencopy shared-memory file: {error}"))?;
    Ok(file)
}

fn encode_jpeg(
    raw: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    quality: u8,
    max_dimension: u32,
    pointer_position: Option<(u32, u32, u32, u32)>,
) -> Result<Vec<u8>, String> {
    if !matches!(
        format,
        wl_shm::Format::Argb8888
            | wl_shm::Format::Xrgb8888
            | wl_shm::Format::Argb2101010
            | wl_shm::Format::Xrgb2101010
    ) {
        return Err(format!(
            "Hyprland screencopy returned unsupported shared-memory format {format:?}"
        ));
    }
    let scale = (f64::from(max_dimension) / f64::from(width.max(height))).min(1.0);
    let output_width = (f64::from(width) * scale).round().max(1.0) as u16;
    let output_height = (f64::from(height) * scale).round().max(1.0) as u16;
    let mut rgb = resize_rgb_area(
        raw,
        width,
        height,
        stride,
        format,
        usize::from(output_width),
        usize::from(output_height),
    )?;
    if let Some((pointer_x, pointer_y, logical_width, logical_height)) = pointer_position {
        composite_pointer(
            &mut rgb,
            usize::from(output_width),
            usize::from(output_height),
            pointer_x,
            pointer_y,
            logical_width,
            logical_height,
            f64::from(output_width) / f64::from(width),
            f64::from(output_height) / f64::from(height),
        )?;
    }
    let mut jpeg = Vec::new();
    Encoder::new(&mut jpeg, quality)
        .encode(&rgb, output_width, output_height, ColorType::Rgb)
        .map_err(|error| format!("encoding the Hyprland screenshot: {error}"))?;
    if jpeg.len() > crate::MAX_IMAGE {
        return Err(format!("screenshot exceeds {} bytes", crate::MAX_IMAGE));
    }
    Ok(jpeg)
}

pub(crate) fn composite_pointer(
    rgb: &mut [u8],
    width: usize,
    height: usize,
    cursor_x: u32,
    cursor_y: u32,
    logical_width: u32,
    logical_height: u32,
    scale_x: f64,
    scale_y: f64,
) -> Result<(), String> {
    if logical_width == 0
        || logical_height == 0
        || !scale_x.is_finite()
        || !scale_y.is_finite()
        || scale_x <= 0.0
        || scale_y <= 0.0
    {
        return Err("pointer overlay geometry is invalid".into());
    }
    let expected_len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or("pointer overlay screenshot dimensions overflow")?;
    if rgb.len() != expected_len {
        return Err("pointer overlay screenshot buffer has an invalid size".into());
    }
    let (source_width, source_height, hotspot_x, hotspot_y, source) =
        crate::hyprland_overlay::pointer_image();
    let draw_width = (f64::from(source_width) * scale_x).round().max(1.0) as usize;
    let draw_height = (f64::from(source_height) * scale_y).round().max(1.0) as usize;
    let cursor_pixel_x = i64::from(cursor_x) * width as i64 / i64::from(logical_width);
    let cursor_pixel_y = i64::from(cursor_y) * height as i64 / i64::from(logical_height);
    let left = cursor_pixel_x - (f64::from(hotspot_x) * scale_x).round() as i64;
    let top = cursor_pixel_y - (f64::from(hotspot_y) * scale_y).round() as i64;
    for draw_y in 0..draw_height {
        let y = top + draw_y as i64;
        if !(0..height as i64).contains(&y) {
            continue;
        }
        let source_y = ((draw_y as f64 + 0.5) / scale_y).floor() as usize;
        if source_y >= source_height as usize {
            continue;
        }
        for draw_x in 0..draw_width {
            let x = left + draw_x as i64;
            if !(0..width as i64).contains(&x) {
                continue;
            }
            let source_x = ((draw_x as f64 + 0.5) / scale_x).floor() as usize;
            if source_x >= source_width as usize {
                continue;
            }
            let source_index = (source_y * source_width as usize + source_x) * 4;
            let alpha = u32::from(source[source_index + 3]);
            if alpha == 0 {
                continue;
            }
            let inverse_alpha = 255 - alpha;
            let destination_index = (y as usize * width + x as usize) * 3;
            for (channel, source_channel) in [(0, 2), (1, 1), (2, 0)] {
                rgb[destination_index + channel] =
                    (u32::from(source[source_index + source_channel])
                        + u32::from(rgb[destination_index + channel]) * inverse_alpha / 255)
                        .min(255) as u8;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn pointer_composite_pixel(rgb: &[u8], width: usize, x: usize, y: usize) -> [u8; 3] {
    let index = (y * width + x) * 3;
    [rgb[index], rgb[index + 1], rgb[index + 2]]
}

fn resize_rgb_area(
    raw: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    output_width: usize,
    output_height: usize,
) -> Result<Vec<u8>, String> {
    let ten_bit = matches!(
        format,
        wl_shm::Format::Argb2101010 | wl_shm::Format::Xrgb2101010
    );
    let output_pixels = output_width
        .checked_mul(output_height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or("scaled screencopy frame is too large")?;
    let mut rgb = Vec::with_capacity(output_pixels);
    if output_width == width as usize && output_height == height as usize {
        for y in 0..height as usize {
            let row_start = y
                .checked_mul(stride as usize)
                .ok_or("screencopy row offset overflow")?;
            let row = raw
                .get(row_start..row_start + width as usize * 4)
                .ok_or("screencopy frame is truncated")?;
            for pixel in row.chunks_exact(4) {
                rgb.extend_from_slice(&pixel_rgb(pixel, ten_bit));
            }
        }
        return Ok(rgb);
    }
    let scale_x = f64::from(width) / output_width as f64;
    let scale_y = f64::from(height) / output_height as f64;
    for output_y in 0..output_height {
        let top = output_y as f64 * scale_y;
        let bottom = (output_y + 1) as f64 * scale_y;
        for output_x in 0..output_width {
            let left = output_x as f64 * scale_x;
            let right = (output_x + 1) as f64 * scale_x;
            let mut channels = [0.0_f64; 3];
            let mut total_weight = 0.0;
            for source_y in top.floor() as usize..bottom.ceil() as usize {
                let vertical_weight =
                    (bottom.min(source_y as f64 + 1.0) - top.max(source_y as f64)).max(0.0);
                let row_start = source_y
                    .checked_mul(stride as usize)
                    .ok_or("screencopy row offset overflow")?;
                let row = raw
                    .get(row_start..row_start + width as usize * 4)
                    .ok_or("screencopy frame is truncated")?;
                for source_x in left.floor() as usize..right.ceil() as usize {
                    let horizontal_weight =
                        (right.min(source_x as f64 + 1.0) - left.max(source_x as f64)).max(0.0);
                    let weight = horizontal_weight * vertical_weight;
                    let pixel = &row[source_x * 4..source_x * 4 + 4];
                    let [red, green, blue] = pixel_components(pixel, ten_bit);
                    channels[0] += f64::from(red) * weight;
                    channels[1] += f64::from(green) * weight;
                    channels[2] += f64::from(blue) * weight;
                    total_weight += weight;
                }
            }
            if total_weight == 0.0 {
                return Err("screencopy resize produced an empty source region".into());
            }
            rgb.extend(
                channels.map(|channel| to_eight_bit((channel / total_weight).round() as u32)),
            );
        }
    }
    Ok(rgb)
}

fn pixel_rgb(pixel: &[u8], ten_bit: bool) -> [u8; 3] {
    pixel_components(pixel, ten_bit).map(|component| to_eight_bit(u32::from(component)))
}

fn pixel_components(pixel: &[u8], ten_bit: bool) -> [u16; 3] {
    if ten_bit {
        let packed = u32::from_le_bytes(pixel.try_into().expect("screencopy pixel is 4 bytes"));
        [
            ((packed >> 20) & 0x3ff) as u16,
            ((packed >> 10) & 0x3ff) as u16,
            (packed & 0x3ff) as u16,
        ]
    } else {
        [pixel[2], pixel[1], pixel[0]]
            .map(|component| ((u32::from(component) * 1023 + 127) / 255) as u16)
    }
}

fn to_eight_bit(component: u32) -> u8 {
    ((component * 255 + 511) / 1023) as u8
}

#[cfg(test)]
mod tests {
    use super::{
        State, buffer_info_complete, composite_pointer, encode_jpeg, pointer_composite_pixel,
        resize_rgb_area,
    };

    #[test]
    fn composites_the_shared_arrow_at_the_capture_cursor_hotspot() {
        let mut rgb = vec![0; 32 * 32 * 3];
        composite_pointer(&mut rgb, 32, 32, 16, 16, 32, 32, 1.0, 1.0).unwrap();
        let hotspot = pointer_composite_pixel(&rgb, 32, 16, 16);
        assert!(
            hotspot[0] > 100
                && hotspot[1] > 150
                && hotspot[2] > 180
                && hotspot[0] < hotspot[1]
                && hotspot[1] < hotspot[2],
            "hotspot is not the translucent light-blue arrow: {hotspot:?}"
        );
        assert_eq!(pointer_composite_pixel(&rgb, 32, 0, 0), [0, 0, 0]);
    }

    #[test]
    fn downsizes_and_clips_the_composited_arrow() {
        let mut rgb = vec![0; 16 * 16 * 3];
        composite_pointer(&mut rgb, 16, 16, 0, 0, 16, 16, 0.5, 0.5).unwrap();
        assert!(rgb.iter().any(|channel| *channel > 0));
        assert!(composite_pointer(&mut rgb, 16, 16, 0, 0, 16, 16, 0.0, 1.0).is_err());
    }
    use wayland_client::protocol::wl_shm;

    #[test]
    fn area_resize_averages_bgra_pixels_into_rgb() {
        let pixels = [
            30, 20, 10, 255, 60, 50, 40, 255, 90, 80, 70, 255, 120, 110, 100, 255,
        ];
        let rgb = resize_rgb_area(&pixels, 2, 2, 8, wl_shm::Format::Xrgb8888, 1, 1).unwrap();
        assert_eq!(rgb, [55, 65, 75]);
    }

    #[test]
    fn area_resize_weights_partial_source_pixels() {
        let pixels = [0, 0, 0, 255, 0, 0, 30, 255, 0, 0, 60, 255];
        let rgb = resize_rgb_area(&pixels, 3, 1, 12, wl_shm::Format::Xrgb8888, 2, 1).unwrap();
        assert_eq!(rgb, [10, 0, 0, 50, 0, 0]);
    }

    #[test]
    fn area_resize_preserves_pixels_without_downscaling() {
        let pixels = [
            30, 20, 10, 255, 60, 50, 40, 255, 0, 0, 0, 0, 90, 80, 70, 255, 120, 110, 100, 255, 0,
            0, 0, 0,
        ];
        let rgb = resize_rgb_area(&pixels, 2, 2, 12, wl_shm::Format::Xrgb8888, 2, 2).unwrap();
        assert_eq!(rgb, [10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120]);
    }

    #[test]
    fn jpeg_encoding_uses_the_resized_dimensions() {
        let pixels = [0, 0, 0, 255, 0, 0, 30, 255, 0, 0, 60, 255];
        let jpeg = encode_jpeg(&pixels, 3, 1, 12, wl_shm::Format::Xrgb8888, 80, 2, None).unwrap();
        let frame = jpeg
            .windows(2)
            .position(|marker| marker == [0xff, 0xc0])
            .expect("JPEG should contain a baseline frame header");
        assert_eq!(u16::from_be_bytes([jpeg[frame + 5], jpeg[frame + 6]]), 1);
        assert_eq!(u16::from_be_bytes([jpeg[frame + 7], jpeg[frame + 8]]), 2);
    }

    #[test]
    fn ten_bit_xrgb_pixels_are_converted_to_eight_bit_rgb() {
        let packed = (3_u32 << 30) | (1023 << 20) | (512 << 10);
        let pixel = packed.to_le_bytes();
        assert_eq!(super::pixel_rgb(&pixel, true), [255, 128, 0]);
    }

    #[test]
    fn ten_bit_area_resize_averages_before_eight_bit_conversion() {
        let mut pixels = Vec::new();
        for red in [0_u32, 512, 1023] {
            pixels.extend_from_slice(&((3 << 30) | (red << 20)).to_le_bytes());
        }
        let rgb = resize_rgb_area(&pixels, 3, 1, 12, wl_shm::Format::Xrgb2101010, 2, 1).unwrap();
        assert_eq!(rgb, [43, 0, 0, 213, 0, 0]);
    }

    #[test]
    fn jpeg_encoding_accepts_ten_bit_rgb_and_argb_frames() {
        let packed = ((3_u32 << 30) | (1023 << 20) | (512 << 10)).to_le_bytes();
        for format in [wl_shm::Format::Xrgb2101010, wl_shm::Format::Argb2101010] {
            let jpeg = encode_jpeg(&packed, 1, 1, 4, format, 80, 1, None).unwrap();
            assert!(jpeg.starts_with(&[0xff, 0xd8, 0xff]));
        }
    }

    #[test]
    fn screencopy_buffer_enumeration_obeys_protocol_version() {
        let mut state = State::default();
        assert!(!buffer_info_complete(&state, 2));
        assert!(!buffer_info_complete(&state, 3));
        state.buffer = Some((wl_shm::Format::Xrgb8888, 1, 1, 4));
        assert!(buffer_info_complete(&state, 2));
        assert!(!buffer_info_complete(&state, 3));
        state.buffer_done = true;
        assert!(buffer_info_complete(&state, 3));
        state.failed = true;
        assert!(buffer_info_complete(&state, 3));
    }
}
