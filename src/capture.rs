//! Screenshot capture for each Backend.

use super::*;

pub(crate) struct CaptureCandidate {
    pub(crate) image: Vec<u8>,
    pub(crate) origin_x: i32,
    pub(crate) origin_y: i32,
    pub(crate) origin_known: bool,
    pub(crate) source: Option<String>,
    pub(crate) logical_width: u32,
    pub(crate) logical_height: u32,
    pub(crate) pointer: Option<(i32, i32)>,
    pub(crate) layout: Option<(i32, i32, u32, u32)>,
}

pub(crate) struct CapturedFrame {
    pub(crate) image: Vec<u8>,
    pub(crate) pixel_width: u32,
    pub(crate) pixel_height: u32,
    pub(crate) origin_x: i32,
    pub(crate) origin_y: i32,
    pub(crate) origin_known: bool,
    pub(crate) source: Option<String>,
    pub(crate) logical_width: u32,
    pub(crate) logical_height: u32,
    pub(crate) pointer: Option<(i32, i32)>,
    pub(crate) layout: Option<(i32, i32, u32, u32)>,
}

/// Size limits for the Screenshots a Session returns.
///
/// Claude reports coordinates in the image it processes, after the API downsizes anything
/// beyond the model's limits. Capturing within those limits keeps the model's coordinates
/// identical to screenshot pixels. The rules follow
/// <https://platform.claude.com/docs/en/build-with-claude/vision-coordinates>.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImageLimits {
    pub(crate) profile: &'static str,
    pub(crate) max_edge: u32,
    /// Maximum visual tokens, one per 28×28 patch; `None` disables the token rule.
    pub(crate) max_tokens: Option<u32>,
}

pub(crate) const IMAGE_PROFILES: &[ImageLimits] = &[
    ImageLimits {
        profile: "high-resolution",
        max_edge: 2576,
        max_tokens: Some(4784),
    },
    ImageLimits {
        profile: "standard",
        max_edge: 1568,
        max_tokens: Some(1568),
    },
    ImageLimits {
        profile: "full",
        max_edge: 3200,
        max_tokens: None,
    },
];
pub(crate) const IMAGE_PROFILE_ENV: &str = "COMPUTER_USE_LINUX_IMAGE_PROFILE";
const PATCH: u32 = 28;

impl ImageLimits {
    pub(crate) fn named(profile: &str) -> Option<Self> {
        IMAGE_PROFILES
            .iter()
            .copied()
            .find(|limits| limits.profile == profile)
    }

    /// The `image_profile` argument, else the environment default, else high-resolution.
    pub(crate) fn from_request(args: &Value) -> Result<Self, String> {
        let names = || {
            IMAGE_PROFILES
                .iter()
                .map(|limits| limits.profile)
                .collect::<Vec<_>>()
                .join(", ")
        };
        match args.get("image_profile") {
            None | Some(Value::Null) => {}
            Some(Value::String(name)) => {
                return Self::named(name)
                    .ok_or_else(|| format!("image_profile must be one of {}", names()));
            }
            Some(_) => return Err("image_profile must be a string".into()),
        }
        match std::env::var(IMAGE_PROFILE_ENV) {
            Ok(name) if !name.is_empty() => Self::named(&name)
                .ok_or_else(|| format!("{IMAGE_PROFILE_ENV} must be one of {}", names())),
            _ => Ok(IMAGE_PROFILES[0]),
        }
    }

    pub(crate) fn fits(&self, width: u32, height: u32) -> bool {
        let Some(tokens) = self.max_tokens else {
            // Not a model profile: a plain pixel cap.
            return width <= self.max_edge && height <= self.max_edge;
        };
        let patches = |side: u32| side.div_ceil(PATCH);
        patches(width) * PATCH <= self.max_edge
            && patches(height) * PATCH <= self.max_edge
            && patches(width) * patches(height) <= tokens
    }

    /// The largest long edge at this aspect ratio that fits, leaving one pixel of
    /// rounding slack on the short edge for the capture backends' own scaling.
    pub(crate) fn fitted_long_edge(&self, width: u32, height: u32) -> u32 {
        let (long, short) = (width.max(height).max(1), width.min(height).max(1));
        let fits = |candidate: u32| {
            let other =
                ((f64::from(candidate) * f64::from(short) / f64::from(long)).round() as u32).max(1);
            self.fits(candidate, other + 1)
        };
        let (mut lo, mut hi) = (1, long.min(self.max_edge) + 1);
        if fits(hi - 1) {
            return hi - 1;
        }
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if fits(mid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }

    pub(crate) fn describe(&self) -> Value {
        json!({"profile":self.profile,"max_edge":self.max_edge,"max_visual_tokens":self.max_tokens,"coordinate_space":"screenshot pixels match the pixels the model sees"})
    }
}

pub(crate) fn capture(
    backend: &Backend,
    composite_pointer_overlay: bool,
    limits: ImageLimits,
) -> Result<CapturedFrame, String> {
    let caps = [3200, 2560, 2048]
        .into_iter()
        .map(|cap: u32| cap.min(limits.max_edge))
        .fold(Vec::new(), |mut caps, cap| {
            if !caps.contains(&cap) {
                caps.push(cap);
            }
            caps
        });
    let frame = capture_within(backend, composite_pointer_overlay, &caps)?;
    if limits.fits(frame.pixel_width, frame.pixel_height) {
        return Ok(frame);
    }
    let long_edge = limits.fitted_long_edge(frame.pixel_width, frame.pixel_height);
    let frame = capture_within(backend, composite_pointer_overlay, &[long_edge])?;
    if limits.fits(frame.pixel_width, frame.pixel_height) {
        Ok(frame)
    } else {
        Err(format!(
            "capture backend returned {}x{}, which exceeds the {} image profile",
            frame.pixel_width, frame.pixel_height, limits.profile
        ))
    }
}

fn capture_within(
    backend: &Backend,
    composite_pointer_overlay: bool,
    caps: &[u32],
) -> Result<CapturedFrame, String> {
    let mut captured = None;
    let mut size_error = String::new();
    let mut frame_error = None;
    for max_dimension in caps.iter().copied() {
        let qualities: &[u8] = if max_dimension == caps[0] {
            &[85, 60, 35]
        } else if max_dimension > 2048 {
            &[85]
        } else {
            &[85, 35]
        };
        for quality in qualities.iter().copied() {
            let attempt = match backend {
                Backend::Wayland {
                    portal, hyprland, ..
                } => portal.capture_with_cursor(quality, max_dimension).map(
                    |(image, metadata_pointer)| {
                        let pointer = if *hyprland && portal.position_available {
                            hyprland_cursor_position()
                                .and_then(|position| {
                                    captured_pointer_position(
                                        position,
                                        portal.origin_x,
                                        portal.origin_y,
                                        portal.logical_width,
                                        portal.logical_height,
                                        &image,
                                    )
                                })
                                .or(metadata_pointer)
                        } else {
                            metadata_pointer
                        };
                        CaptureCandidate {
                            image,
                            origin_x: portal.origin_x,
                            origin_y: portal.origin_y,
                            origin_known: portal.position_available
                                && portal.coordinates_available(),
                            source: portal.source.clone(),
                            logical_width: portal.logical_width,
                            logical_height: portal.logical_height,
                            pointer,
                            layout: None,
                        }
                    },
                ),
                Backend::Hyprland {
                    monitor,
                    native_capture,
                    cursor_capture,
                    ..
                } => {
                    let geometry = hyprland_monitor_geometry(monitor)?;
                    let layout = (geometry.0, geometry.1, geometry.2, geometry.3);
                    let frame = if *native_capture {
                        hyprland_capture::capture_output(
                            monitor,
                            *cursor_capture,
                            quality,
                            max_dimension,
                            composite_pointer_overlay.then_some(layout),
                        )
                    } else {
                        let pointer = composite_pointer_overlay
                            .then(hyprland_cursor_position)
                            .flatten()
                            .and_then(|(x, y)| {
                                let (x, y) = (
                                    i64::from(x) - i64::from(layout.0),
                                    i64::from(y) - i64::from(layout.1),
                                );
                                (x >= 0
                                    && y >= 0
                                    && x < i64::from(layout.2)
                                    && y < i64::from(layout.3))
                                .then_some((x as u32, y as u32, layout.2, layout.3, geometry.4))
                            });
                        grim_image(
                            quality,
                            *cursor_capture,
                            Some((layout.2, layout.3)),
                            Some(monitor),
                            Some(geometry.4),
                            max_dimension,
                            pointer,
                        )
                    };
                    frame.map(|image| {
                        let pointer = hyprland_cursor_position().and_then(|position| {
                            captured_pointer_position(
                                position, layout.0, layout.1, layout.2, layout.3, &image,
                            )
                        });
                        CaptureCandidate {
                            image,
                            origin_x: layout.0,
                            origin_y: layout.1,
                            origin_known: true,
                            source: None,
                            logical_width: layout.2,
                            logical_height: layout.3,
                            pointer,
                            layout: Some(layout),
                        }
                    })
                }
                Backend::Sway { cursor_capture, .. } => {
                    let layout = sway::sway_layout()?;
                    grim_image(
                        quality,
                        *cursor_capture,
                        Some((layout.2, layout.3)),
                        None,
                        None,
                        max_dimension,
                        None,
                    )
                    .map(|image| CaptureCandidate {
                        image,
                        origin_x: layout.0,
                        origin_y: layout.1,
                        origin_known: true,
                        source: None,
                        logical_width: layout.2,
                        logical_height: layout.3,
                        pointer: None,
                        layout: Some(layout),
                    })
                }
                Backend::WaylandGrim {
                    cursor_capture,
                    layout_capture,
                    ..
                } => {
                    if *layout_capture {
                        let layout = wlroots_layout()?;
                        grim_image(
                            quality,
                            *cursor_capture,
                            Some((layout.2, layout.3)),
                            None,
                            None,
                            max_dimension,
                            None,
                        )
                        .map(|image| CaptureCandidate {
                            image,
                            origin_x: layout.0,
                            origin_y: layout.1,
                            origin_known: true,
                            source: None,
                            logical_width: layout.2,
                            logical_height: layout.3,
                            pointer: None,
                            layout: Some(layout),
                        })
                    } else {
                        grim_image(
                            quality,
                            *cursor_capture,
                            None,
                            None,
                            None,
                            max_dimension,
                            None,
                        )
                        .map(|image| CaptureCandidate {
                            image,
                            origin_x: 0,
                            origin_y: 0,
                            origin_known: false,
                            source: None,
                            logical_width: 0,
                            logical_height: 0,
                            pointer: None,
                            layout: None,
                        })
                    }
                }
                Backend::X11 { cursor_capture } => {
                    let screen = x11::x11_screen_size();
                    x11::x11_image(quality, *cursor_capture, screen, max_dimension).map(|image| {
                        let (width, height) = jpeg_dimensions(&image).unwrap_or((0, 0));
                        let (logical_width, logical_height) = screen.unwrap_or((width, height));
                        let pointer = x11::x11_pointer_position().map(|(x, y)| {
                            (
                                (i64::from(x) * i64::from(width) / i64::from(logical_width.max(1)))
                                    as i32,
                                (i64::from(y) * i64::from(height)
                                    / i64::from(logical_height.max(1)))
                                    as i32,
                            )
                        });
                        CaptureCandidate {
                            image,
                            origin_x: 0,
                            origin_y: 0,
                            origin_known: true,
                            source: None,
                            logical_width,
                            logical_height,
                            pointer,
                            layout: screen.map(|(width, height)| (0, 0, width, height)),
                        }
                    })
                }
                Backend::Unsupported => return Err("capture is unsupported on this session".into()),
            };
            match attempt {
                Ok(result) if jpeg_dimensions(&result.image).is_some() => {
                    captured = Some(result);
                    break;
                }
                Ok(_) => {
                    frame_error = Some(
                        "capture backend returned an invalid or incomplete JPEG frame".to_owned(),
                    );
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) if error.contains("exceeds") => size_error = error,
                Err(error) => {
                    frame_error = Some(error);
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        if captured.is_some() {
            break;
        }
    }
    let candidate = captured.ok_or_else(|| {
        frame_error.map_or_else(
            || {
                if size_error.is_empty() {
                    "screenshot capture failed".to_owned()
                } else {
                    format!("{size_error}; screenshot remains too large at minimum JPEG quality")
                }
            },
            |error| format!("capture failed after retries: {error}"),
        )
    })?;
    let (pixel_width, pixel_height) =
        jpeg_dimensions(&candidate.image).ok_or("capture backend did not return a JPEG image")?;
    let logical_width = if candidate.logical_width == 0 {
        pixel_width
    } else {
        candidate.logical_width
    };
    let logical_height = if candidate.logical_height == 0 {
        pixel_height
    } else {
        candidate.logical_height
    };
    Ok(CapturedFrame {
        image: candidate.image,
        pixel_width,
        pixel_height,
        origin_x: candidate.origin_x,
        origin_y: candidate.origin_y,
        origin_known: candidate.origin_known,
        source: candidate.source,
        logical_width,
        logical_height,
        pointer: candidate.pointer,
        layout: candidate.layout,
    })
}

pub(crate) fn grim_image(
    quality: u8,
    cursor_capture: bool,
    logical_size: Option<(u32, u32)>,
    output: Option<&str>,
    output_scale: Option<f64>,
    max_dimension: u32,
    pointer_overlay: Option<(u32, u32, u32, u32, f64)>,
) -> Result<Vec<u8>, String> {
    let mut command = limited_command("4s", "grim");
    let scale = logical_size
        .map(|(width, height)| {
            (f64::from(max_dimension) / width.max(height).max(1) as f64)
                .min(output_scale.unwrap_or(1.0))
        })
        .unwrap_or_else(|| (f64::from(max_dimension) / 3200.0).min(1.0));
    let scale = format!("{scale:.6}");
    command.args(["-s", &scale]);
    if pointer_overlay.is_some() {
        command.args(["-t", "ppm"]);
    } else {
        command.args(["-t", "jpeg", "-q", &quality.to_string()]);
    }
    if let Some(output) = output {
        command.args(["-o", output]);
    }
    if cursor_capture && pointer_overlay.is_none() {
        command.arg("-c");
    }
    command.arg("-");
    let (image, status) = bounded_command_output(
        &mut command,
        if pointer_overlay.is_some() {
            64 * 1024 * 1024
        } else {
            MAX_IMAGE
        },
    )
    .map_err(|error| format!("grim capture failed: {error}"))?;
    if !status.success() {
        return Err(format!("grim capture failed with status {status}"));
    }
    if let Some((cursor_x, cursor_y, logical_width, logical_height, output_scale)) = pointer_overlay
    {
        let (width, height, mut rgb) = compact_ppm_raster(image)?;
        let scale_x = f64::from(width) / (f64::from(logical_width) * output_scale);
        let scale_y = f64::from(height) / (f64::from(logical_height) * output_scale);
        hyprland_capture::composite_pointer(
            &mut rgb,
            width as usize,
            height as usize,
            cursor_x,
            cursor_y,
            logical_width,
            logical_height,
            scale_x,
            scale_y,
        )?;
        let mut jpeg = Vec::new();
        Encoder::new(&mut jpeg, quality)
            .encode(
                &rgb,
                u16::try_from(width).map_err(|_| "Grim screenshot width exceeds JPEG limits")?,
                u16::try_from(height).map_err(|_| "Grim screenshot height exceeds JPEG limits")?,
                ColorType::Rgb,
            )
            .map_err(|error| format!("encoding the Grim pointer screenshot: {error}"))?;
        if jpeg.len() > MAX_IMAGE {
            return Err(format!("screenshot exceeds {MAX_IMAGE} bytes"));
        }
        Ok(jpeg)
    } else {
        Ok(image)
    }
}

pub(crate) fn compact_ppm_raster(mut bytes: Vec<u8>) -> Result<(u32, u32, Vec<u8>), String> {
    fn token<'a>(bytes: &'a [u8], offset: &mut usize) -> Result<&'a [u8], String> {
        loop {
            while bytes.get(*offset).is_some_and(u8::is_ascii_whitespace) {
                *offset += 1;
            }
            if bytes.get(*offset) != Some(&b'#') {
                break;
            }
            while bytes.get(*offset).is_some_and(|byte| *byte != b'\n') {
                *offset += 1;
            }
        }
        let start = *offset;
        while bytes
            .get(*offset)
            .is_some_and(|byte| !byte.is_ascii_whitespace() && *byte != b'#')
        {
            *offset += 1;
        }
        if *offset == start {
            return Err("Grim returned an incomplete PPM header".into());
        }
        Ok(&bytes[start..*offset])
    }
    let mut offset: usize = 0;
    if token(&bytes, &mut offset)? != b"P6" {
        return Err("Grim returned an unsupported PPM format".into());
    }
    let width = std::str::from_utf8(token(&bytes, &mut offset)?)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .ok_or("Grim returned an invalid PPM width")?;
    let height = std::str::from_utf8(token(&bytes, &mut offset)?)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .ok_or("Grim returned an invalid PPM height")?;
    if token(&bytes, &mut offset)? != b"255" {
        return Err("Grim returned a PPM with an unsupported channel depth".into());
    }
    match bytes.get(offset) {
        Some(b'\r') if bytes.get(offset + 1) == Some(&b'\n') => offset += 2,
        Some(byte) if byte.is_ascii_whitespace() => offset += 1,
        _ => return Err("Grim returned a malformed PPM raster separator".into()),
    }
    let raster_size = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(3))
        .filter(|n| *n <= 64 * 1024 * 1024)
        .ok_or("Grim PPM raster is too large")?;
    let end = offset
        .checked_add(raster_size)
        .filter(|end| *end <= bytes.len())
        .ok_or("Grim returned a truncated PPM raster")?;
    bytes.copy_within(offset..end, 0);
    bytes.truncate(raster_size);
    Ok((width, height, bytes))
}

pub(crate) fn bounded_command_output(
    command: &mut LimitedCommand,
    max_bytes: usize,
) -> Result<(Vec<u8>, std::process::ExitStatus), String> {
    let child = command
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("starting capture process: {error}"))?;
    bounded_child_output(child, max_bytes)
}

pub(crate) fn bounded_child_output(
    mut child: LimitedChild,
    max_bytes: usize,
) -> Result<(Vec<u8>, std::process::ExitStatus), String> {
    let reader = spawn_output_reader(child.stdout.take(), Some(max_bytes));
    let status = child.wait();
    let bytes = join_output_reader(reader, child.pid)
        .map_err(|error| format!("reading capture output: {error}"))?;
    if bytes.len() > max_bytes {
        return Err(format!("capture output exceeds {max_bytes} bytes"));
    }
    let status = status.map_err(|error| format!("waiting for capture process: {error}"))?;
    Ok((bytes, status))
}

pub(crate) fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[..2] != [0xff, 0xd8] {
        return None;
    }
    let mut offset = 2;
    while offset + 4 <= bytes.len() {
        if bytes[offset] != 0xff {
            return None;
        }
        while bytes.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = *bytes.get(offset)?;
        offset += 1;
        if marker == 0xd9 || marker == 0xda {
            return None;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let segment_length = usize::from(u16::from_be_bytes(
            bytes.get(offset..offset + 2)?.try_into().ok()?,
        ));
        if segment_length < 2 {
            return None;
        }
        let end = offset.checked_add(segment_length)?;
        if end > bytes.len() {
            return None;
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            if segment_length < 7 {
                return None;
            }
            let height = u32::from(u16::from_be_bytes(
                bytes[offset + 3..offset + 5].try_into().ok()?,
            ));
            let width = u32::from(u16::from_be_bytes(
                bytes[offset + 5..offset + 7].try_into().ok()?,
            ));
            return (width > 0 && height > 0).then_some((width, height));
        }
        offset = end;
    }
    None
}
