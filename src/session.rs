//! Session lifecycle (`computer_start`, `computer_stop`) and Observations (`computer_observe`).

use super::*;

pub(crate) async fn start_session(args: &Value, state: SharedState) -> Result<Value, String> {
    let start_lock = state.lock().await.start_lock.clone();
    let _start_guard = start_lock.lock().await;
    let action_lock = state.lock().await.action_lock.clone();
    let _action_guard = action_lock.lock().await;
    let mut guard = state.lock().await;
    let closed_portal = guard.session.as_ref().is_some_and(
        |session| matches!(&session.backend, Backend::Wayland { portal, .. } if portal.is_closed()),
    );
    if guard.session.is_some() && !closed_portal {
        return Err("a desktop session is already open".into());
    }
    if closed_portal {
        guard.session = None;
    }
    drop(guard);
    drop(_action_guard);
    let info = desktop_status().await;
    let requested_backend = resolve_requested_backend(args, &info)?;
    let requested_backend = requested_backend.as_str();
    if !matches!(
        requested_backend,
        "auto"
            | "hyprland-native"
            | "hyprland-grim"
            | "wayland-portal"
            | "sway-grim"
            | "wayland-grim"
            | "x11"
    ) {
        return Err("backend must be auto, hyprland-native, hyprland-grim, wayland-portal, sway-grim, wayland-grim, or x11".into());
    }
    if requested_backend == "x11" && !cfg!(feature = "backend-x11") {
        return Err(
            "the X11 backend was excluded from this build; rebuild with --features backend-x11"
                .into(),
        );
    }
    if requested_backend == "sway-grim" && !cfg!(feature = "backend-sway") {
        return Err(
            "the Sway backend was excluded from this build; rebuild with --features backend-sway"
                .into(),
        );
    }
    if requested_backend == "wayland-portal" && !cfg!(feature = "backend-portal") {
        return Err("the portal backend was excluded from this build; rebuild with --features backend-portal".into());
    }
    let legacy_mode = args
        .get("wayland_mode")
        .and_then(Value::as_str)
        .unwrap_or("auto");
    if legacy_mode == "portal" && !cfg!(feature = "backend-portal") {
        return Err("the portal backend was excluded from this build; rebuild with --features backend-portal".into());
    }
    if !matches!(legacy_mode, "auto" | "portal" | "compositor") {
        return Err("wayland_mode must be auto, portal, or compositor".into());
    }
    if requested_backend != "auto" && legacy_mode != "auto" {
        return Err("choose either backend or wayland_mode, not both".into());
    }
    let wayland_mode = match requested_backend {
        "wayland-portal" => "portal",
        "hyprland-native" | "hyprland-grim" | "sway-grim" | "wayland-grim" => "compositor",
        _ => legacy_mode,
    };
    if info["capabilities"]["screenshot"] != true {
        return Err(
            "no usable screenshot backend is available; computer_start requires screen capture"
                .into(),
        );
    }
    if wayland_mode != "auto" && info["session_type"] != "wayland" {
        return Err("wayland_mode can only be set in a Wayland session".into());
    }
    if requested_backend == "x11" && info["session_type"] != "x11" {
        return Err("the x11 backend requires an X11 session".into());
    }
    match requested_backend {
        "wayland-portal"
            if info["capabilities"]["screencast_portal"] != true
                || info["capabilities"]["pipewire_capture_pipeline"] != true =>
        {
            return Err("requested backend wayland-portal is unavailable".into());
        }
        "hyprland-native"
            if info["capabilities"]["hyprland_adapter"] != true
                || info["capabilities"]["hyprland_native_screencopy"] != true =>
        {
            return Err("requested backend hyprland-native is unavailable".into());
        }
        "hyprland-grim"
            if info["capabilities"]["hyprland_adapter"] != true
                || info["capabilities"]["compositor_layout_capture"] != true =>
        {
            return Err("requested backend hyprland-grim is unavailable".into());
        }
        "sway-grim"
            if info["capabilities"]["sway_adapter"] != true
                || info["capabilities"]["compositor_layout_capture"] != true =>
        {
            return Err("requested backend sway-grim is unavailable".into());
        }
        "wayland-grim"
            if info["session_type"] == "wayland"
                && info["capabilities"]["compositor_layout_capture"] == true
                && info["capabilities"]["hyprland_adapter"] != true
                && info["capabilities"]["sway_adapter"] != true => {}
        "wayland-grim" => return Err("requested backend wayland-grim is unavailable".into()),
        _ => {}
    }
    let requested_output = requested_output(args)?;
    let image_limits = ImageLimits::from_request(args)?;
    let accessibility_detail = match AccessibilityDetail::parse(args.get("accessibility_detail"))? {
        Some(detail) => detail,
        None => match std::env::var(ACCESSIBILITY_DETAIL_ENV) {
            Ok(name) if !name.is_empty() => AccessibilityDetail::parse(Some(&json!(name)))
                .map_err(|_| format!("{ACCESSIBILITY_DETAIL_ENV} must be compact, full, or none"))?
                .unwrap_or(AccessibilityDetail::Compact),
            _ => AccessibilityDetail::Compact,
        },
    };
    let mut notes = info["notes"].clone();
    let hyprland_monitor = if info["capabilities"]["hyprland_adapter"] == true {
        Some(hyprland_overlay::select_monitor(requested_output)?)
    } else {
        None
    };
    let (pointer_overlay, pointer_overlay_monitor) = if let Some(monitor) =
        hyprland_monitor.as_deref()
    {
        if info["capabilities"]["visual_pointer_overlay"] == true {
            match hyprland_overlay::PointerOverlay::start(monitor) {
                Ok(overlay) => (
                    Some(Arc::new(std::sync::Mutex::new(overlay))),
                    Some(monitor.to_owned()),
                ),
                Err(error) => {
                    if let Some(notes) = notes.as_array_mut() {
                        notes.push(json!(format!("The native pointer overlay could not start; shared-pointer computer use remains available: {error}")));
                    }
                    (None, None)
                }
            }
        } else {
            (None, None)
        }
    } else {
        if requested_output.is_some() {
            return Err("output selection is only supported on Hyprland".into());
        }
        (None, None)
    };
    let native_cursor_hidden = pointer_overlay.as_ref().is_some_and(|overlay| {
        overlay
            .lock()
            .is_ok_and(|overlay| overlay.native_cursor_hidden())
    });
    if let (Some(notes), Some(overlay_monitor)) =
        (notes.as_array_mut(), pointer_overlay_monitor.as_deref())
    {
        notes.push(json!(if native_cursor_hidden {
            format!("The click-through AI mouse-arrow overlay is active on {overlay_monitor}; Hyprland reports the native cursor as hidden. Computer Use never changes native cursor visibility.")
        } else {
            format!("The click-through AI mouse-arrow overlay is active on {overlay_monitor}; the native cursor stays visible and may overlap the AI pointer.")
        }));
    } else if info["capabilities"]["hyprland_adapter"] == true
        && info["capabilities"]["visual_pointer_overlay"] != true
        && let Some(notes) = notes.as_array_mut()
    {
        notes.push(json!("Hyprland computer use is active with the shared native pointer; the native mouse-arrow overlay could not connect."));
    }
    if info["capabilities"]["hyprland_adapter"] == true
        && info["capabilities"]["accessibility_tree"] == true
        && let Err(error) = run_blocking_result("AT-SPI activation", enable_atspi).await
        && let Some(notes) = notes.as_array_mut()
    {
        notes.push(json!(format!(
            "Could not enable AT-SPI for Qt applications in this session: {error}"
        )));
    }
    let mut backend = if wayland_mode == "portal" {
        if info["capabilities"]["screencast_portal"] != true
            || info["capabilities"]["pipewire_capture_pipeline"] != true
        {
            return Err(
                "portal mode requires an available ScreenCast portal and PipeWire capture pipeline"
                    .into(),
            );
        }
        let hyprland = info["capabilities"]["hyprland_adapter"] == true;
        let portal = start_portal_session(hyprland)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(error) = &portal.eis_error
            && let Some(notes) = notes.as_array_mut()
        {
            notes.push(json!(format!("The portal established EIS but libei could not start an input device; portal input is disabled for this session: {error}")));
        }
        let virtual_keyboard = info["capabilities"]["wtype_available"] == true;
        if let Some(notes) = notes.as_array_mut() {
            notes.push(json!("Using the explicitly selected Wayland portal route."));
        }
        Backend::Wayland {
            portal: Arc::new(portal),
            hyprland,
            hyprland_virtual_keyboard: hyprland
                && info["capabilities"]["hyprland_virtual_keyboard"] == true,
            xkb: info["capabilities"]["xkbcommon"] == true,
            virtual_keyboard,
            direct_pointer: None,
            direct_keyboard: None,
        }
    } else if wayland_mode == "compositor" {
        let native_capture = requested_backend == "hyprland-native"
            || requested_backend == "auto"
                && info["capabilities"]["hyprland_native_screencopy"] == true;
        if !native_capture && info["capabilities"]["compositor_layout_capture"] != true {
            return Err("compositor mode requires a reachable Wayland display and Grim".into());
        }
        let backend = if requested_backend == "hyprland-native"
            || requested_backend == "hyprland-grim"
            || requested_backend == "auto" && info["capabilities"]["hyprland_adapter"] == true
        {
            let monitor = hyprland_monitor
                .clone()
                .ok_or("Hyprland monitor selection is unavailable")?;
            run_blocking_result("Hyprland monitor", move || {
                hyprland_monitor_layout(&monitor)
            })
            .await?;
            Backend::Hyprland {
                monitor: hyprland_monitor
                    .clone()
                    .ok_or("Hyprland monitor selection is unavailable")?,
                native_capture,
                cursor_capture: if native_capture {
                    true
                } else {
                    info["capabilities"]["grim_cursor_capture"] == true
                },
                native_keyboard: info["capabilities"]["hyprland_virtual_keyboard"] == true,
                wtype_keyboard: info["capabilities"]["wtype_available"] == true,
                direct_pointer: None,
                direct_keyboard: None,
            }
        } else if requested_backend == "sway-grim"
            || requested_backend == "auto" && info["capabilities"]["sway_adapter"] == true
        {
            run_blocking_result("Sway layout", sway::sway_layout).await?;
            Backend::Sway {
                cursor_capture: info["capabilities"]["grim_cursor_capture"] == true,
                virtual_keyboard: info["capabilities"]["virtual_keyboard_protocol"] == true,
                direct_pointer: None,
            }
        } else {
            Backend::WaylandGrim {
                cursor_capture: info["capabilities"]["grim_cursor_capture"] == true,
                virtual_keyboard: info["capabilities"]["virtual_keyboard_protocol"] == true,
                layout_capture: info["capabilities"]["wayland_output_layout"] == true,
                direct_pointer: None,
            }
        };
        if let Some(notes) = notes.as_array_mut() {
            notes.push(json!(if native_capture { "Using the explicitly selected native Hyprland capture and input route." } else { "Using the explicitly selected compositor route; Grim captures the compositor layout when supported." }));
        }
        backend
    } else {
        match info["backend"].as_str().unwrap_or_default() {
            "hyprland" | "hyprland-native" => {
                let native_capture = info["capabilities"]["hyprland_native_screencopy"] == true;
                if !native_capture && info["capabilities"]["compositor_layout_capture"] != true {
                    return Err("Hyprland auto mode requires native screencopy or the Grim helper reported by computer_status".into());
                }
                let monitor = hyprland_monitor
                    .clone()
                    .ok_or("Hyprland monitor selection is unavailable")?;
                run_blocking_result("Hyprland monitor", move || {
                    hyprland_monitor_layout(&monitor)
                })
                .await?;
                if let Some(notes) = notes.as_array_mut() {
                    notes.push(json!(if native_capture { "Using Hyprland's native Wayland screencopy and compositor input adapters; no portal consent prompt is required." } else { "Using Hyprland's local Grim and compositor input adapters; no portal consent prompt is required." }));
                }
                Backend::Hyprland {
                    monitor: hyprland_monitor
                        .clone()
                        .ok_or("Hyprland monitor selection is unavailable")?,
                    native_capture,
                    cursor_capture: if native_capture {
                        true
                    } else {
                        info["capabilities"]["grim_cursor_capture"] == true
                    },
                    native_keyboard: info["capabilities"]["hyprland_virtual_keyboard"] == true,
                    wtype_keyboard: info["capabilities"]["wtype_available"] == true,
                    direct_pointer: None,
                    direct_keyboard: None,
                }
            }
            "hyprland-grim" => {
                let monitor = hyprland_monitor
                    .clone()
                    .ok_or("Hyprland monitor selection is unavailable")?;
                run_blocking_result("Hyprland monitor", move || {
                    hyprland_monitor_layout(&monitor)
                })
                .await?;
                Backend::Hyprland {
                    monitor: hyprland_monitor
                        .clone()
                        .ok_or("Hyprland monitor selection is unavailable")?,
                    native_capture: false,
                    cursor_capture: info["capabilities"]["grim_cursor_capture"] == true,
                    native_keyboard: info["capabilities"]["hyprland_virtual_keyboard"] == true,
                    wtype_keyboard: info["capabilities"]["wtype_available"] == true,
                    direct_pointer: None,
                    direct_keyboard: None,
                }
            }
            "sway-grim" => {
                run_blocking_result("Sway layout", sway::sway_layout).await?;
                Backend::Sway {
                    cursor_capture: info["capabilities"]["grim_cursor_capture"] == true,
                    virtual_keyboard: info["capabilities"]["keyboard_input"] == true,
                    direct_pointer: None,
                }
            }
            "wayland-grim" => Backend::WaylandGrim {
                cursor_capture: info["capabilities"]["grim_cursor_capture"] == true,
                virtual_keyboard: info["capabilities"]["keyboard_input"] == true,
                layout_capture: info["capabilities"]["wayland_output_layout"] == true,
                direct_pointer: None,
            },
            "x11" => Backend::X11 {
                cursor_capture: info["capabilities"]["cursor_capture"] == true,
            },
            "wayland-portal" => match start_portal_session(false).await {
                Ok(portal) => {
                    if let Some(error) = &portal.eis_error
                        && let Some(notes) = notes.as_array_mut()
                    {
                        notes.push(json!(format!("The portal established EIS but libei could not start an input device; portal input is disabled for this session: {error}")));
                    }
                    let virtual_keyboard = info["capabilities"]["wtype_available"] == true;
                    Backend::Wayland {
                        portal: Arc::new(portal),
                        hyprland: false,
                        hyprland_virtual_keyboard: false,
                        xkb: info["capabilities"]["xkbcommon"] == true,
                        virtual_keyboard,
                        direct_pointer: None,
                        direct_keyboard: None,
                    }
                }
                Err(error)
                    if info["capabilities"]["grim_capture"] == true
                        && error.allows_compositor_fallback() =>
                {
                    let virtual_keyboard =
                        info["capabilities"]["virtual_keyboard_protocol"] == true;
                    let fallback = if info["capabilities"]["sway_adapter"] == true {
                        run_blocking_result("Sway layout", sway::sway_layout).await?;
                        Backend::Sway {
                            cursor_capture: info["capabilities"]["grim_cursor_capture"] == true,
                            virtual_keyboard,
                            direct_pointer: None,
                        }
                    } else {
                        Backend::WaylandGrim {
                            cursor_capture: info["capabilities"]["grim_cursor_capture"] == true,
                            virtual_keyboard,
                            layout_capture: info["capabilities"]["wayland_output_layout"] == true,
                            direct_pointer: None,
                        }
                    };
                    if let Some(notes) = notes.as_array_mut() {
                        notes.push(json!(format!("Portal startup failed ({error}); using the available Grim compositor fallback.")));
                    }
                    fallback
                }
                Err(error) => return Err(error.to_string()),
            },
            _ => Backend::Unsupported,
        }
    };
    let direct_pointer_allowed = info["capabilities"]["virtual_pointer_protocol"] == true
        && match &backend {
            Backend::Wayland {
                portal, hyprland, ..
            } => *hyprland && !portal.scroll_available() && portal.monitor_geometry_available(),
            Backend::Hyprland { .. } | Backend::Sway { .. } => true,
            Backend::WaylandGrim { layout_capture, .. } => *layout_capture,
            _ => false,
        };
    if direct_pointer_allowed {
        match tokio::task::spawn_blocking(virtual_pointer::VirtualPointer::connect).await {
            Ok(Ok(pointer)) => {
                let pointer = Some(Arc::new(std::sync::Mutex::new(pointer)));
                match &mut backend {
                    Backend::Wayland { direct_pointer, .. }
                    | Backend::Hyprland { direct_pointer, .. }
                    | Backend::Sway { direct_pointer, .. }
                    | Backend::WaylandGrim { direct_pointer, .. } => *direct_pointer = pointer,
                    _ => {}
                }
            }
            Ok(Err(error)) => {
                if let Some(notes) = notes.as_array_mut() {
                    notes.push(json!(format!("The compositor advertised virtual-pointer support but could not start it: {error}")));
                }
            }
            Err(error) => {
                if let Some(notes) = notes.as_array_mut() {
                    notes.push(json!(format!("The virtual-pointer worker failed: {error}")));
                }
            }
        }
    }
    let direct_keyboard_allowed = info["capabilities"]["virtual_keyboard_protocol"] == true
        && uses_hyprland_keyboard(&backend);
    let mut keyboard_map_info = None;
    if direct_keyboard_allowed {
        match tokio::task::spawn_blocking(virtual_keyboard::VirtualKeyboard::connect).await {
            Ok(Ok(keyboard)) => {
                keyboard_map_info = Some(
                    json!({"layout":keyboard.keymap_layout(),"source":keyboard.keymap_source()}),
                );
                if keyboard.keymap_source() == "embedded-us-fallback"
                    && let Some(notes) = notes.as_array_mut()
                {
                    notes.push(json!("The active Hyprland XKB map could not be compiled; native keypresses use the embedded US fallback. Use type for literal text."));
                }
                let keyboard = Some(Arc::new(std::sync::Mutex::new(keyboard)));
                match &mut backend {
                    Backend::Wayland {
                        direct_keyboard, ..
                    }
                    | Backend::Hyprland {
                        direct_keyboard, ..
                    } => *direct_keyboard = keyboard,
                    _ => {}
                }
            }
            Ok(Err(error)) => {
                if let Some(notes) = notes.as_array_mut() {
                    notes.push(json!(format!("The compositor advertised virtual-keyboard support but could not start a held-key device: {error}")));
                }
            }
            Err(error) => {
                if let Some(notes) = notes.as_array_mut() {
                    notes.push(json!(format!(
                        "The virtual-keyboard worker failed: {error}"
                    )));
                }
            }
        }
    }
    if matches!(backend, Backend::Unsupported) {
        return Err("no supported graphical session is active".into());
    }
    let id = Uuid::new_v4().to_string();
    let mut capabilities = info["capabilities"].clone();
    if let Some(keyboard_map_info) = keyboard_map_info {
        capabilities["keyboard_layout"] = keyboard_map_info;
    }
    if let Backend::Wayland {
        portal,
        hyprland,
        hyprland_virtual_keyboard,
        virtual_keyboard,
        direct_pointer,
        direct_keyboard,
        ..
    } = &backend
    {
        capabilities["screenshot"] = json!(true);
        capabilities["capture_source"] = json!(portal.source);
        let portal_pointer = portal.pointer_available();
        let direct_scroll_pointer = direct_pointer.is_some();
        let portal_keyboard = portal.keyboard_available();
        let hyprland_pointer_fallback =
            *hyprland && !portal_pointer && portal.monitor_geometry_available();
        let hyprland_keyboard_fallback =
            *hyprland && !portal_keyboard && (direct_keyboard.is_some() || *virtual_keyboard);
        capabilities["pointer_input"] = json!(portal_pointer || hyprland_pointer_fallback);
        capabilities["keyboard_input"] = json!(
            hyprland_keyboard_fallback || portal_keyboard || (!*hyprland && *virtual_keyboard)
        );
        capabilities["text_input"] =
            json!(hyprland_keyboard_fallback || portal_keyboard || *virtual_keyboard);
        capabilities["unicode_text_input"] = json!(
            portal.unicode_text_available()
                || *virtual_keyboard
                || (*hyprland && *hyprland_virtual_keyboard)
        );
        capabilities["xdotool_text_input"] = Value::Bool(false);
        capabilities["scroll_input"] = json!(hyprland_portal_scroll_available(
            *hyprland,
            portal.scroll_available(),
            portal.monitor_geometry_available(),
            direct_scroll_pointer
        ));
        capabilities["smooth_scroll_input"] = json!(
            portal.smooth_scroll_available()
                || (*hyprland && portal.monitor_geometry_available() && direct_scroll_pointer)
        );
        capabilities["accessibility_actions"] = json!(
            info["capabilities"]["accessibility_actions"] == true
                && portal.position_available
                && portal.coordinates_available()
        );
        if info["capabilities"]["accessibility_actions"] == true
            && (!portal.position_available || !portal.coordinates_available())
            && let Some(notes) = notes.as_array_mut()
        {
            notes.push(json!("AT-SPI is available, but this portal stream has no compositor position; accessibility controls cannot be mapped to the captured image and actions are disabled."));
        }
        if portal_keyboard
            && !portal.unicode_text_available()
            && let Some(notes) = notes.as_array_mut()
        {
            notes.push(json!("This portal session has key input but no EIS text device; arbitrary Unicode text input is unavailable."));
        }
        if !portal_pointer
            && !hyprland_pointer_fallback
            && let Some(notes) = notes.as_array_mut()
        {
            notes.push(json!(if portal.remote_desktop_active && !portal.remote_pointer { "The RemoteDesktop session did not grant pointer input; pointer movement and scrolling are unavailable." } else { "No pointer input device is available for this session." }));
        }
        if !portal_keyboard
            && !hyprland_keyboard_fallback
            && !*virtual_keyboard
            && let Some(notes) = notes.as_array_mut()
        {
            notes.push(json!(if portal.remote_desktop_active && !portal.remote_keyboard { "The RemoteDesktop session did not grant keyboard input; keypresses and typed text are unavailable." } else { "No keyboard input device is available for this session." }));
        }
        if portal.remote_desktop_active
            && portal.remote_pointer
            && !portal.scroll_available()
            && let Some(notes) = notes.as_array_mut()
        {
            notes.push(json!("The portal connected EIS, but it did not provide a usable scroll device for this session."));
        }
        if *virtual_keyboard && let Some(notes) = notes.as_array_mut() {
            notes.push(json!("ScreenCast is active without RemoteDesktop; wtype provides keyboard input through the compositor virtual-keyboard protocol."));
        }
        if *hyprland
            && !portal_pointer
            && !portal.monitor_geometry_available()
            && let Some(notes) = notes.as_array_mut()
        {
            notes.push(json!("Hyprland keyboard input is available, but native pointer fallback is disabled because the portal did not provide positioned monitor geometry."));
        }
        if hyprland_pointer_fallback
            && portal.remote_desktop_active
            && let Some(notes) = notes.as_array_mut()
        {
            notes.push(json!("The portal did not grant usable pointer input; Hyprland is providing pointer movement and buttons for the positioned monitor streams."));
        }
    }
    if let Backend::Hyprland {
        native_capture,
        native_keyboard,
        wtype_keyboard,
        direct_pointer,
        direct_keyboard,
        ..
    } = &backend
    {
        capabilities["screenshot"] = json!(true);
        capabilities["pointer_input"] = json!(true);
        capabilities["keyboard_input"] = json!(direct_keyboard.is_some() || *wtype_keyboard);
        capabilities["text_input"] = json!(*native_keyboard || *wtype_keyboard);
        capabilities["unicode_text_input"] = json!(*native_keyboard || *wtype_keyboard);
        capabilities["scroll_input"] = json!(direct_pointer.is_some());
        capabilities["smooth_scroll_input"] = json!(direct_pointer.is_some());
        capabilities["xdotool_text_input"] = json!(false);
        capabilities["cursor_capture"] =
            json!(*native_capture || info["capabilities"]["grim_cursor_capture"] == true);
    }
    if let Backend::WaylandGrim {
        virtual_keyboard,
        layout_capture,
        direct_pointer,
        ..
    } = &backend
    {
        capabilities["screenshot"] = json!(true);
        capabilities["pointer_input"] = json!(direct_pointer.is_some());
        capabilities["keyboard_input"] = json!(*virtual_keyboard);
        capabilities["text_input"] = json!(*virtual_keyboard);
        capabilities["unicode_text_input"] = json!(*virtual_keyboard);
        capabilities["scroll_input"] = json!(direct_pointer.is_some());
        capabilities["smooth_scroll_input"] = json!(direct_pointer.is_some());
        capabilities["accessibility_tree"] = json!(false);
        capabilities["accessibility_actions"] = json!(false);
        capabilities["cursor_capture"] = json!(info["capabilities"]["grim_cursor_capture"] == true);
        if let Some(notes) = notes.as_array_mut() {
            notes.push(json!(if direct_pointer.is_some() {
                "Grim screenshots use wlr-randr output geometry; the compositor's virtual-pointer protocol provides pointer and scroll input."
            } else if *layout_capture {
                "Grim screenshots use wlr-randr output geometry, but virtual-pointer input could not be started; pointer and scroll are unavailable."
            } else {
                "Grim provides screenshots on this Wayland compositor; wtype may provide keyboard input if its virtual-keyboard probe succeeds. Pointer and scroll input are unavailable because compositor layout coordinates could not be determined."
            }));
        }
    }
    if let Backend::Sway {
        virtual_keyboard, ..
    } = &backend
    {
        capabilities["screenshot"] = json!(true);
        capabilities["pointer_input"] = json!(true);
        capabilities["keyboard_input"] = json!(*virtual_keyboard);
        capabilities["text_input"] = json!(*virtual_keyboard);
        capabilities["unicode_text_input"] = json!(*virtual_keyboard);
        capabilities["scroll_input"] = json!(true);
        capabilities["smooth_scroll_input"] = json!(direct_pointer(&backend).is_some());
        capabilities["xdotool_text_input"] = json!(false);
        capabilities["cursor_capture"] = json!(info["capabilities"]["grim_cursor_capture"] == true);
        if let Some(notes) = notes.as_array_mut() {
            notes.push(json!(if direct_pointer(&backend).is_some() {
                "Active Sway pointer and scroll input use the wlroots virtual-pointer protocol."
            } else {
                "Active Sway pointer input uses deprecated Sway IPC cursor commands because the virtual-pointer connection is unavailable."
            }));
        }
    }
    if let Backend::Wayland { portal, .. } = &backend
        && portal.synthetic_layout
        && let Some(notes) = notes.as_array_mut()
    {
        notes.push(json!("The portal omitted source positions, so selected streams are tiled horizontally; accessibility bounds are unavailable for this synthetic layout."));
    }
    let modifier_keys_available = match &backend {
        Backend::X11 { .. } => info["capabilities"]["keyboard_input"] == true,
        Backend::Hyprland {
            direct_keyboard, ..
        } => direct_keyboard.is_some(),
        Backend::Wayland { portal, .. } => {
            if uses_hyprland_keyboard(&backend) {
                direct_keyboard(&backend).is_some()
            } else {
                portal.keyboard_available()
            }
        }
        Backend::Sway { .. } | Backend::WaylandGrim { .. } | Backend::Unsupported => false,
    };
    let pointer_position_available = match &backend {
        Backend::Wayland {
            hyprland, portal, ..
        } => {
            (*hyprland
                && portal.position_available
                && portal.coordinates_available()
                && info["capabilities"]["pointer_position"] == true)
                || portal.cursor_position_available()
        }
        Backend::Hyprland { .. } | Backend::X11 { .. } => {
            info["capabilities"]["pointer_position"] == true
        }
        Backend::Sway { .. } | Backend::WaylandGrim { .. } | Backend::Unsupported => false,
    };
    capabilities["pointer_position"] = json!(pointer_position_available);
    capabilities["cursor_metadata_capture"] = json!(
        matches!(&backend, Backend::Wayland { portal, .. } if portal.cursor_position_available())
    );
    capabilities["mouse_modifiers"] =
        json!(capabilities["pointer_input"] == true && modifier_keys_available);
    capabilities["click_modifiers"] = capabilities["mouse_modifiers"].clone();
    let active_backend = active_backend_name(&backend);
    let active_target = backend_target(&backend);
    let selected_output = match &backend {
        Backend::Hyprland { monitor, .. } => Some(monitor.clone()),
        _ => None,
    };
    let mut guard = state.lock().await;
    guard.session = Some(DesktopSession {
        id: id.clone(),
        backend,
        observation: None,
        pointer_overlay,
        pointer_overlay_monitor: pointer_overlay_monitor.clone(),
        image_limits,
        accessibility_detail,
    });
    let mut rollback = SessionStartRollback::new(state.clone(), id.clone());
    capabilities["visual_pointer_overlay"] = json!(pointer_overlay_monitor.is_some());
    capabilities["native_cursor_hidden"] = json!(native_cursor_hidden);
    let mut session_info = json!({"session_id":id,"wayland_mode":wayland_mode,"active_backend":active_backend,"active_target":active_target,"capture_validated":true,"capabilities":capabilities,"notes":notes});
    session_info["selected_output"] = json!(selected_output);
    session_info["image_limits"] = image_limits.describe();
    session_info["accessibility_detail"] = json!(accessibility_detail.name());
    drop(guard);
    match observe(&json!({"session_id":id}), state.clone()).await {
        Ok(mut initial_observation) => {
            if initial_observation["structuredContent"]["accessibility_coordinate_mapping_available"]
                == false
            {
                session_info["capabilities"]["accessibility_actions"] = json!(false);
                if let Some(notes) = session_info["notes"].as_array_mut() {
                    notes.push(json!("The active application's AT-SPI bounds do not match compositor window geometry; accessibility actions are disabled until coordinates can be mapped safely."));
                }
            }
            let portal_capabilities = {
                let guard = state.lock().await;
                guard
                    .session
                    .as_ref()
                    .and_then(|session| match &session.backend {
                        Backend::Wayland {
                            portal, hyprland, ..
                        } => Some((
                            portal.cursor_position_available(),
                            (*hyprland
                                && portal.position_available
                                && portal.coordinates_available()
                                && info["capabilities"]["pointer_position"] == true)
                                || portal.cursor_position_available(),
                        )),
                        _ => None,
                    })
            };
            if let Some((cursor_metadata, pointer_position)) = portal_capabilities {
                session_info["capabilities"]["cursor_metadata_capture"] = json!(cursor_metadata);
                session_info["capabilities"]["pointer_position"] = json!(pointer_position);
                if !cursor_metadata && let Some(notes) = session_info["notes"].as_array_mut() {
                    notes.push(json!("Portal cursor metadata capture failed; using screenshots without cursor-position metadata for this session."));
                }
            }
            let mut metadata = initial_observation["structuredContent"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            metadata.extend(
                session_info
                    .as_object()
                    .expect("session info is an object")
                    .clone(),
            );
            let metadata = Value::Object(metadata);
            initial_observation["content"][0]["text"] = Value::String(metadata.to_string());
            initial_observation["structuredContent"] = metadata;
            rollback.disarm();
            Ok(initial_observation)
        }
        Err(error) => {
            if state.lock().await.session.is_none() {
                rollback.disarm();
                return Err(format!(
                    "initial desktop observation failed; the closed session was cleared: {error}"
                ));
            }
            let cleanup_result = stop_session(&json!({"session_id":id}), state).await;
            rollback.disarm();
            match cleanup_result {
                Ok(_) => Err(format!(
                    "initial desktop observation failed; the session was closed: {error}"
                )),
                Err(cleanup) => Err(format!(
                    "initial desktop observation failed: {error}; session cleanup also failed: {cleanup}"
                )),
            }
        }
    }
}

pub(crate) async fn stop_session(args: &Value, state: SharedState) -> Result<Value, String> {
    let id = required_string(args, "session_id")?;
    let action_lock = state.lock().await.action_lock.clone();
    let _action_guard = action_lock.lock().await;
    await_held_input_cleanups().await;
    let mut guard = state.lock().await;
    let Some(session) = guard.session.as_ref() else {
        return Err("no desktop session is open".into());
    };
    if session.id != id {
        return Err("unknown desktop session".into());
    }
    let session = guard.session.take().expect("session checked above");
    let close_result = if let Backend::Wayland { portal, .. } = &session.backend {
        match timeout(Duration::from_secs(5), portal.close()).await {
            Ok(result) => result,
            Err(_) => Err("timed out closing the desktop portal session".into()),
        }
    } else {
        Ok(())
    };
    drop(guard);
    let overlay_result = if let Some(overlay) = session.pointer_overlay {
        tokio::task::spawn_blocking(move || {
            overlay
                .lock()
                .map_err(|_| "pointer overlay lock was poisoned".to_owned())?
                .stop()
        })
        .await
        .map_err(|error| format!("pointer overlay cleanup worker failed: {error}"))?
    } else {
        Ok(())
    };
    close_result?;
    overlay_result?;
    Ok(tool_result(json!({"closed":true}), Vec::new()))
}

pub(crate) async fn observe(args: &Value, state: SharedState) -> Result<Value, String> {
    let id = required_string(args, "session_id")?;
    let requested_detail = AccessibilityDetail::parse(args.get("accessibility_detail"))?;
    let action_lock = state.lock().await.action_lock.clone();
    let _action_guard = action_lock.lock().await;
    await_held_input_cleanups().await;
    let (backend, pointer_overlay, pointer_overlay_monitor, image_limits, accessibility_detail) = {
        let mut guard = state.lock().await;
        let session = guard.session.as_mut().ok_or("no desktop session is open")?;
        if session.id != id {
            return Err("unknown desktop session".into());
        }
        session.observation = None;
        (
            session.backend.clone(),
            session.pointer_overlay.clone(),
            session.pointer_overlay_monitor.clone(),
            session.image_limits,
            requested_detail.unwrap_or(session.accessibility_detail),
        )
    };
    let captured_at = std::time::Instant::now();
    let active_backend = backend.clone();
    let active_before = tokio::task::spawn_blocking(move || active_window_now(&active_backend))
        .await
        .ok()
        .and_then(Result::ok)
        .flatten();
    let capture_backend = backend.clone();
    let composite_pointer_overlay = pointer_overlay.is_some();
    let capture = tokio::task::spawn_blocking(move || {
        capture(&capture_backend, composite_pointer_overlay, image_limits)
    });
    let accessibility = tokio::task::spawn_blocking(accessibility_snapshot);
    let (capture, accessibility) = tokio::join!(capture, accessibility);
    let capture = match capture {
        Err(error) => return Err(format!("capture worker failed: {error}")),
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            let portal_closed =
                matches!(&backend, Backend::Wayland { portal, .. } if portal.is_closed());
            if portal_closed {
                let mut guard = state.lock().await;
                if guard
                    .session
                    .as_ref()
                    .is_some_and(|session| session.id == id)
                {
                    guard.session = None;
                }
                return Err(format!(
                    "desktop portal session closed; its state was cleared; start a new session ({error})"
                ));
            }
            return Err(error);
        }
    };
    let CapturedFrame {
        image,
        pixel_width: width,
        pixel_height: height,
        origin_x,
        origin_y,
        origin_known,
        source,
        logical_width,
        logical_height,
        pointer,
        layout,
    } = capture;
    if image.len() > MAX_IMAGE {
        return Err("screenshot exceeds 3 MiB".into());
    }
    let observation_id = Uuid::new_v4().to_string();
    let mut snapshot =
        accessibility.map_err(|error| format!("accessibility worker failed: {error}"))?;
    let active_backend = backend.clone();
    let active_after = tokio::task::spawn_blocking(move || active_window_now(&active_backend))
        .await
        .ok()
        .and_then(Result::ok)
        .flatten();
    if !same_active_window(&active_before, &active_after) {
        return Err(
            "active window changed or could not be verified during observation; observe again"
                .into(),
        );
    }
    let snapshot_focus = focused_element_from_snapshot(&snapshot);
    let live_focus = tokio::task::spawn_blocking(focused_element_now)
        .await
        .ok()
        .and_then(Result::ok)
        .flatten();
    if snapshot_focus.is_some() && !same_focused_element(&snapshot_focus, &live_focus) {
        return Err(
            "keyboard focus changed or could not be verified during observation; observe again"
                .into(),
        );
    }
    let accessibility_mapping = accessibility_bounds_mapping(&snapshot, active_after.as_ref());
    add_image_bounds(
        &mut snapshot,
        ImageGeometry {
            pixel_width: width,
            pixel_height: height,
            origin_x,
            origin_y,
            origin_known,
            logical_width,
            logical_height,
        },
        accessibility_mapping,
    );
    let accessibility_actions_reason = if snapshot["available"] != true {
        Some(
            snapshot["error"]
                .as_str()
                .unwrap_or("AT-SPI tree is unavailable")
                .to_owned(),
        )
    } else if !origin_known {
        Some("captured output has no known desktop coordinate origin".to_owned())
    } else if let Some(process_id) = accessibility_mapping.unmapped_process {
        let reason = accessibility_mapping
            .unmapped_reason
            .unwrap_or("AT-SPI bounds cannot be mapped safely to the captured output");
        Some(match accessibility_mapping.unmapped_geometry {
            Some([wx, wy, ww, wh, ax, ay, aw, ah]) => format!(
                "{reason} (process {process_id}; Hyprland window {wx},{wy} {ww}x{wh}; AT-SPI root {ax},{ay} {aw}x{ah})"
            ),
            _ => format!("{reason} (process {process_id})"),
        })
    } else {
        None
    };
    let focused_element = live_focus.or(snapshot_focus);
    bound_accessibility_snapshot(&mut snapshot);
    let focus_identity = active_after.or_else(|| active_window_from_snapshot(&snapshot));
    let visible_active_window = active_window_from_snapshot(&snapshot);
    let elements = snapshot["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|node| {
            Some((
                node["element_id"].as_str()?.to_owned(),
                AccessibleElement {
                    name: node["name"].as_str()?.to_owned(),
                    role: node["role"].as_str()?.to_owned(),
                    process_id: node["process_id"]
                        .as_u64()
                        .and_then(|value| u32::try_from(value).ok()),
                    accessible_id: node["accessible_id"].as_str().map(str::to_owned),
                    bounds: node["bounds"].clone(),
                    actions: node["actions"]
                        .as_array()?
                        .iter()
                        .map(|action| action.as_str().map(str::to_owned))
                        .collect(),
                    editable: node["states"]
                        .as_array()
                        .is_some_and(|states| states.iter().any(|state| state == "editable")),
                    on_screen: node["image_bounds"].is_object(),
                },
            ))
        })
        .collect();
    let observation = Observation {
        captured_at,
        width,
        height,
        origin_x,
        origin_y,
        origin_known,
        source,
        pointer,
        layout,
        active_window: focus_identity,
        focused_element,
        elements,
        logical_width,
        logical_height,
    };
    let mut metadata = json!({
        "session_id":id,
        "observation_id":observation_id,
        "display":{"width":width,"height":height,"image_profile":image_limits.profile,"logical_width":logical_width,"logical_height":logical_height,"coordinate_space":"captured output pixels","origin_x":origin_x,"origin_y":origin_y,"origin_known":origin_known,"source":observation.source,"pointer":observation.pointer.map(|(x,y)|json!({"x":x,"y":y,"coordinate_space":"captured output pixels"}))},
        "active_window":visible_active_window,
        "accessibility_actions_available":snapshot["available"] == true && origin_known && accessibility_mapping.unmapped_process.is_none(),
        "accessibility_actions_reason":accessibility_actions_reason,
        "accessibility_coordinate_mapping_available":origin_known && accessibility_mapping.unmapped_process.is_none(),
        "accessibility":accessibility_detail.present(&snapshot)
    });
    let pointer_overlay_alive = if let Some(overlay) = pointer_overlay.as_ref() {
        let overlay = overlay.clone();
        let monitor = pointer_overlay_monitor.clone();
        match tokio::task::spawn_blocking(move || {
            let monitor = monitor.ok_or_else(|| {
                "selected output is unavailable for pointer overlay recovery".to_owned()
            })?;
            overlay
                .lock()
                .map_err(|_| "pointer overlay lock was poisoned".to_owned())?
                .ensure_running(&monitor)
        })
        .await
        {
            Ok(Ok(restarted)) => {
                if restarted {
                    metadata["notes"] = json!([
                        "The pointer overlay process exited and was restarted on the selected output."
                    ]);
                }
                true
            }
            Ok(Err(error)) => {
                metadata["notes"] = json!([format!(
                    "The pointer overlay process could not be checked or restarted: {error}"
                )]);
                false
            }
            Err(error) => {
                metadata["notes"] = json!([format!("Pointer overlay recovery failed: {error}")]);
                false
            }
        }
    } else {
        false
    };
    let native_cursor_hidden = pointer_overlay.as_ref().is_some_and(|overlay| {
        overlay
            .lock()
            .is_ok_and(|overlay| overlay.native_cursor_hidden())
    });
    metadata["capabilities"] = json!({
        "visual_pointer_overlay":pointer_overlay_alive,
        "native_cursor_hidden":native_cursor_hidden
    });
    if let Backend::Hyprland { monitor, .. } = &backend {
        metadata["selected_output"] = json!(monitor);
    }
    if let Backend::Wayland { portal, .. } = &backend {
        metadata["input_regions"] = json!(portal.input_regions(width, height));
        metadata["capabilities"]["cursor_metadata_capture"] =
            json!(portal.cursor_position_available());
    }
    {
        let mut guard = state.lock().await;
        let session = guard
            .session
            .as_mut()
            .ok_or("desktop session closed while observing")?;
        if session.id != id {
            return Err("unknown desktop session".into());
        }
        session.observation = Some((observation_id.clone(), observation));
    }
    Ok(observation_result(metadata, image))
}

/// How much of the accessibility tree an Observation returns to the agent.
/// The server always keeps the full tree to validate accessibility actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccessibilityDetail {
    Compact,
    Full,
    None,
}

pub(crate) const ACCESSIBILITY_DETAIL_ENV: &str = "COMPUTER_USE_LINUX_ACCESSIBILITY_DETAIL";

/// Bytes of compact accessibility JSON per Observation, about 7,000 tokens.
const COMPACT_ACCESSIBILITY_BUDGET: usize = 24 * 1024;
const COMPACT_STATES: &[&str] = &[
    "focused", "active", "checked", "selected", "expanded", "editable",
];

impl AccessibilityDetail {
    pub(crate) fn parse(value: Option<&Value>) -> Result<Option<Self>, String> {
        match value {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(name)) => match name.as_str() {
                "compact" => Ok(Some(Self::Compact)),
                "full" => Ok(Some(Self::Full)),
                "none" => Ok(Some(Self::None)),
                _ => Err("accessibility_detail must be compact, full, or none".into()),
            },
            Some(_) => Err("accessibility_detail must be a string".into()),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Full => "full",
            Self::None => "none",
        }
    }

    /// The accessibility JSON shown to the agent for a full snapshot.
    pub(crate) fn present(self, snapshot: &Value) -> Value {
        match self {
            Self::Full => {
                let mut full = snapshot.clone();
                full["detail"] = json!("full");
                full
            }
            Self::None => json!({
                "available":snapshot["available"],
                "detail":"none",
                "node_count":snapshot["nodes"].as_array().map_or(0, Vec::len)
            }),
            Self::Compact => compact_accessibility(snapshot),
        }
    }
}

fn truncated_chars(text: &str, limit: usize) -> (String, bool) {
    let mut chars = text.chars();
    let kept = chars.by_ref().take(limit).collect::<String>();
    (kept, chars.next().is_some())
}

/// On-screen nodes only, with the fields an agent acts on, within a fixed byte budget.
pub(crate) fn compact_accessibility(snapshot: &Value) -> Value {
    let mut compact = json!({
        "available":snapshot["available"],
        "detail":"compact",
        "coordinate_space":"screenshot pixels",
        "nodes":[]
    });
    if let Some(error) = snapshot.get("error").filter(|error| !error.is_null()) {
        compact["error"] = error.clone();
    }
    let mut nodes = Vec::new();
    let (mut used, mut offscreen, mut over_budget) = (0usize, 0usize, 0usize);
    for node in snapshot["nodes"].as_array().into_iter().flatten() {
        let Some(bounds) = node["image_bounds"].as_object() else {
            offscreen += 1;
            continue;
        };
        let (name, _) = truncated_chars(node["name"].as_str().unwrap_or_default(), 100);
        let mut entry = json!({
            "element_id":node["element_id"],
            "role":node["role"],
            "name":name,
            "process_id":node["process_id"],
            "image_bounds":{"x":bounds.get("x"),"y":bounds.get("y"),"width":bounds.get("width"),"height":bounds.get("height")}
        });
        if node["actions"]
            .as_array()
            .is_some_and(|actions| !actions.is_empty())
        {
            entry["actions"] = node["actions"].clone();
        }
        let states = node["states"].as_array().cloned().unwrap_or_default();
        let has = |label: &str| states.iter().any(|state| state == label);
        let mut kept = COMPACT_STATES
            .iter()
            .filter(|label| has(label))
            .map(|label| json!(label))
            .collect::<Vec<_>>();
        if !states.is_empty() && !has("enabled") && !has("sensitive") {
            kept.push(json!("disabled"));
        }
        if !kept.is_empty() {
            entry["states"] = Value::Array(kept);
        }
        if let Some(description) = node["description"].as_str().filter(|text| !text.is_empty()) {
            entry["description"] = json!(truncated_chars(description, 100).0);
        }
        if let Some(text) = node["text"].as_str().filter(|text| !text.is_empty()) {
            let (text, cut) = truncated_chars(text, 200);
            entry["text"] = json!(text);
            if cut || node["text_truncated"] == true {
                entry["text_truncated"] = json!(true);
            }
        }
        let size = serde_json::to_vec(&entry).map_or(usize::MAX, |bytes| bytes.len() + 1);
        if used.saturating_add(size) > COMPACT_ACCESSIBILITY_BUDGET {
            over_budget += 1;
            continue;
        }
        used += size;
        nodes.push(entry);
    }
    compact["nodes"] = Value::Array(nodes);
    compact["omitted_offscreen"] = json!(offscreen);
    compact["omitted_over_budget"] = json!(over_budget);
    compact["truncated"] = json!(over_budget > 0 || snapshot["truncated"] == true);
    compact
}

pub(crate) fn bound_accessibility_snapshot(snapshot: &mut Value) {
    if snapshot["nodes"].as_array().is_none() {
        return;
    }
    let mut encoded_len = serde_json::to_vec(snapshot).map_or(usize::MAX, |bytes| bytes.len());
    let mut removed = false;
    while encoded_len > MAX_OBSERVATION_METADATA.saturating_sub(512) {
        let Some(nodes) = snapshot["nodes"].as_array_mut() else {
            break;
        };
        let had_previous_node = nodes.len() > 1;
        let Some(node) = nodes.pop() else { break };
        let node_len = serde_json::to_vec(&node).map_or(0, |bytes| bytes.len());
        encoded_len = encoded_len.saturating_sub(node_len + usize::from(had_previous_node));
        removed = true;
    }
    if removed {
        snapshot["truncated"] = Value::Bool(true);
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct AccessibilityBoundsMapping {
    pub(crate) relative_process: Option<u32>,
    pub(crate) offset_x: i64,
    pub(crate) offset_y: i64,
    pub(crate) unmapped_process: Option<u32>,
    pub(crate) unmapped_reason: Option<&'static str>,
    pub(crate) unmapped_geometry: Option<[i64; 8]>,
}

pub(crate) fn accessibility_bounds_mapping(
    snapshot: &Value,
    active_window: Option<&Value>,
) -> AccessibilityBoundsMapping {
    let Some(active_window) = active_window else {
        return AccessibilityBoundsMapping::default();
    };
    let Some(process_id) = active_window["process_id"]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
    else {
        return AccessibilityBoundsMapping::default();
    };
    let Some(window_bounds) = active_window["bounds"].as_object() else {
        return AccessibilityBoundsMapping::default();
    };
    let (Some(window_x), Some(window_y), Some(window_width), Some(window_height)) = (
        window_bounds.get("x").and_then(Value::as_i64),
        window_bounds.get("y").and_then(Value::as_i64),
        window_bounds.get("width").and_then(Value::as_i64),
        window_bounds.get("height").and_then(Value::as_i64),
    ) else {
        return AccessibilityBoundsMapping::default();
    };
    let Some(nodes) = snapshot["nodes"].as_array() else {
        return AccessibilityBoundsMapping::default();
    };
    let Some(root) = nodes
        .iter()
        .filter(|node| {
            node["process_id"].as_u64() == Some(u64::from(process_id))
                && matches!(
                    node["role"]
                        .as_str()
                        .unwrap_or_default()
                        .to_ascii_lowercase()
                        .as_str(),
                    "frame" | "window" | "dialog" | "filler"
                )
                && node["bounds"].is_object()
        })
        .min_by_key(|node| {
            node["element_id"].as_str().map_or(usize::MAX, |path| {
                path.bytes().filter(|byte| *byte == b'.').count()
            })
        })
    else {
        return AccessibilityBoundsMapping::default();
    };
    let Some(bounds) = root["bounds"].as_object() else {
        return AccessibilityBoundsMapping {
            unmapped_process: Some(process_id),
            unmapped_reason: Some("AT-SPI omitted the active application's root bounds"),
            ..AccessibilityBoundsMapping::default()
        };
    };
    let (Some(accessible_x), Some(accessible_y), Some(accessible_width), Some(accessible_height)) = (
        bounds.get("x").and_then(Value::as_i64),
        bounds.get("y").and_then(Value::as_i64),
        bounds.get("width").and_then(Value::as_i64),
        bounds.get("height").and_then(Value::as_i64),
    ) else {
        return AccessibilityBoundsMapping {
            unmapped_process: Some(process_id),
            unmapped_reason: Some("AT-SPI returned incomplete root bounds"),
            ..AccessibilityBoundsMapping::default()
        };
    };
    if accessible_x.abs_diff(window_x) <= 24 && accessible_y.abs_diff(window_y) <= 24 {
        return AccessibilityBoundsMapping::default();
    }

    let dimensions_match = accessible_width.abs_diff(window_width) <= 24
        && accessible_height.abs_diff(window_height) <= 24;
    let root_id = root["element_id"].as_str().unwrap_or_default();
    let mut distinct_child_origins = std::collections::HashSet::new();
    if dimensions_match && accessible_x == 0 && accessible_y == 0 {
        for node in nodes.iter().filter(|node| {
            node["process_id"].as_u64() == Some(u64::from(process_id))
                && node["element_id"]
                    .as_str()
                    .is_some_and(|path| path.starts_with(root_id) && path != root_id)
        }) {
            let Some(child) = node["bounds"].as_object() else {
                continue;
            };
            let (Some(x), Some(y), Some(width), Some(height)) = (
                child.get("x").and_then(Value::as_i64),
                child.get("y").and_then(Value::as_i64),
                child.get("width").and_then(Value::as_i64),
                child.get("height").and_then(Value::as_i64),
            ) else {
                continue;
            };
            if x < 0
                || y < 0
                || width <= 0
                || height <= 0
                || x.saturating_add(width) > accessible_width.saturating_add(24)
                || y.saturating_add(height) > accessible_height.saturating_add(24)
            {
                continue;
            }
            distinct_child_origins.insert((x, y));
        }
    }
    if dimensions_match
        && accessible_x == 0
        && accessible_y == 0
        && distinct_child_origins.len() >= 2
    {
        return AccessibilityBoundsMapping {
            relative_process: Some(process_id),
            offset_x: window_x,
            offset_y: window_y,
            unmapped_process: None,
            unmapped_reason: None,
            unmapped_geometry: None,
        };
    }
    let reason = if accessible_x == 0 && accessible_y == 0 {
        "AT-SPI bounds appear window-relative, but their dimensions and child positions do not establish a safe mapping"
    } else {
        "Hyprland window geometry and AT-SPI root bounds use incompatible coordinate origins"
    };
    AccessibilityBoundsMapping {
        unmapped_process: Some(process_id),
        unmapped_reason: Some(reason),
        unmapped_geometry: Some([
            window_x,
            window_y,
            window_width,
            window_height,
            accessible_x,
            accessible_y,
            accessible_width,
            accessible_height,
        ]),
        ..AccessibilityBoundsMapping::default()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ImageGeometry {
    pub(crate) pixel_width: u32,
    pub(crate) pixel_height: u32,
    pub(crate) origin_x: i32,
    pub(crate) origin_y: i32,
    pub(crate) origin_known: bool,
    pub(crate) logical_width: u32,
    pub(crate) logical_height: u32,
}

pub(crate) fn add_image_bounds(
    snapshot: &mut Value,
    geometry: ImageGeometry,
    mapping: AccessibilityBoundsMapping,
) {
    let Some(nodes) = snapshot["nodes"].as_array_mut() else {
        return;
    };
    for node in nodes.iter_mut() {
        node["image_bounds"] = Value::Null;
        if mapping
            .unmapped_process
            .is_some_and(|pid| node["process_id"].as_u64() == Some(u64::from(pid)))
        {
            continue;
        }
        if !geometry.origin_known || geometry.logical_width == 0 || geometry.logical_height == 0 {
            continue;
        }
        let process_id = node["process_id"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok());
        let Some(bounds) = node["bounds"].as_object_mut() else {
            continue;
        };
        let mapped_process = process_id.is_some_and(|pid| mapping.relative_process == Some(pid));
        let coordinate_space = bounds.get("coordinate_space").and_then(Value::as_str);
        let relative = match coordinate_space {
            Some("screen") => false,
            Some("window") => mapped_process,
            _ => mapped_process,
        };
        if coordinate_space == Some("window") && !relative {
            continue;
        }
        let Some(x) = bounds.get("x").and_then(Value::as_i64) else {
            continue;
        };
        let Some(y) = bounds.get("y").and_then(Value::as_i64) else {
            continue;
        };
        let Some(element_width) = bounds.get("width").and_then(Value::as_i64) else {
            continue;
        };
        let Some(element_height) = bounds.get("height").and_then(Value::as_i64) else {
            continue;
        };
        if element_width <= 0 || element_height <= 0 {
            continue;
        }
        if relative {
            bounds.insert("coordinate_space".into(), json!("window"));
        }
        let offset_x = if relative { mapping.offset_x } else { 0 };
        let offset_y = if relative { mapping.offset_y } else { 0 };
        let Some(left) = x
            .checked_add(offset_x)
            .and_then(|value| value.checked_sub(i64::from(geometry.origin_x)))
        else {
            continue;
        };
        let Some(top) = y
            .checked_add(offset_y)
            .and_then(|value| value.checked_sub(i64::from(geometry.origin_y)))
        else {
            continue;
        };
        let Some(right) = x
            .checked_add(offset_x)
            .and_then(|value| value.checked_add(element_width))
            .and_then(|value| value.checked_sub(i64::from(geometry.origin_x)))
        else {
            continue;
        };
        let Some(bottom) = y
            .checked_add(offset_y)
            .and_then(|value| value.checked_add(element_height))
            .and_then(|value| value.checked_sub(i64::from(geometry.origin_y)))
        else {
            continue;
        };
        let sx = left as f64 * f64::from(geometry.pixel_width) / f64::from(geometry.logical_width);
        let sy = top as f64 * f64::from(geometry.pixel_height) / f64::from(geometry.logical_height);
        let ex = right as f64 * f64::from(geometry.pixel_width) / f64::from(geometry.logical_width);
        let ey =
            bottom as f64 * f64::from(geometry.pixel_height) / f64::from(geometry.logical_height);
        let left = (sx.floor() as i64).max(0);
        let top = (sy.floor() as i64).max(0);
        let right = (ex.ceil() as i64).min(i64::from(geometry.pixel_width));
        let bottom = (ey.ceil() as i64).min(i64::from(geometry.pixel_height));
        if right <= left || bottom <= top {
            continue;
        }
        node["image_bounds"] = json!({"x":left,"y":top,"width":right-left,"height":bottom-top,"coordinate_space":"captured output pixels"});
    }
    for node in nodes {
        if !node["image_bounds"].is_object() {
            node["name"] = Value::Null;
            node["description"] = Value::Null;
            node["text"] = Value::Null;
            node["text_truncated"] = Value::Bool(false);
            node["actions"] = json!([]);
            node["bounds"] = Value::Null;
        }
    }
}

pub(crate) fn active_window_from_snapshot(snapshot: &Value) -> Option<Value> {
    let nodes = snapshot["nodes"].as_array()?;
    let window = nodes.iter().find(|node| {
        node["states"]
            .as_array()
            .is_some_and(|states| states.iter().any(|state| state == "active"))
            && node["image_bounds"].is_object()
            && node["role"].as_str().is_some_and(|role| {
                matches!(
                    role.to_ascii_lowercase().as_str(),
                    "frame" | "window" | "dialog" | "filler"
                )
            })
    })?;
    let path = window["element_id"].as_str()?;
    let application = nodes
        .iter()
        .find(|node| {
            node["role"]
                .as_str()
                .is_some_and(|role| role.eq_ignore_ascii_case("application"))
                && node["element_id"]
                    .as_str()
                    .is_some_and(|app| path.starts_with(&format!("{app}.")))
        })
        .and_then(|node| node["name"].as_str())
        .unwrap_or_default();
    Some(
        json!({"application":application,"name":window["name"],"role":window["role"],"process_id":window["process_id"],"accessible_id":window["accessible_id"],"element_id":path}),
    )
}

pub(crate) fn focused_element_from_snapshot(snapshot: &Value) -> Option<Value> {
    let nodes = snapshot["nodes"].as_array()?;
    let active_window_path = nodes
        .iter()
        .filter(|node| {
            node["states"]
                .as_array()
                .is_some_and(|states| states.iter().any(|state| state == "active"))
                && node["role"].as_str().is_some_and(|role| {
                    matches!(
                        role.to_ascii_lowercase().as_str(),
                        "frame" | "window" | "dialog"
                    )
                })
        })
        .max_by_key(|node| {
            node["element_id"]
                .as_str()
                .map_or(0, |path| path.matches('.').count())
        })
        .and_then(|node| node["element_id"].as_str());
    let node = nodes
        .iter()
        .filter(|node| {
            let in_active_window = active_window_path.is_none_or(|active_path| {
                node["element_id"].as_str().is_some_and(|path| {
                    path == active_path
                        || path
                            .strip_prefix(active_path)
                            .is_some_and(|tail| tail.starts_with('.'))
                })
            });
            let states = node["states"].as_array();
            in_active_window
                && states.is_some_and(|states| states.iter().any(|state| state == "focused"))
                && states.is_some_and(|states| states.iter().any(|state| state == "visible"))
                && states.is_some_and(|states| states.iter().any(|state| state == "showing"))
                && node["bounds"].is_object()
        })
        .max_by_key(|node| {
            let depth = node["element_id"]
                .as_str()
                .map_or(0, |path| path.matches('.').count());
            let container = node["role"].as_str().is_some_and(|role| {
                matches!(
                    role.to_ascii_lowercase().as_str(),
                    "desktop" | "application" | "frame" | "window" | "dialog" | "filler"
                )
            });
            depth * 2 + usize::from(!container)
        })?;
    Some(json!({
        "element_id":node["element_id"],
        "name":node["name"],
        "role":node["role"],
        "process_id":node["process_id"],
        "accessible_id":node["accessible_id"],
        "bounds":node["bounds"]
    }))
}
