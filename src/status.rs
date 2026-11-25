//! `computer_status`: probes the Login Session for available Targets, Backends and capabilities.

use super::*;

pub(crate) async fn desktop_status() -> Value {
    let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    let wayland = session_type.eq_ignore_ascii_case("wayland")
        || !std::env::var("WAYLAND_DISPLAY")
            .unwrap_or_default()
            .is_empty();
    let x11 = !std::env::var("DISPLAY").unwrap_or_default().is_empty();
    let grim_available = command_exists("grim");
    let hyprland_candidate = wayland
        && !std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .unwrap_or_default()
            .is_empty();
    let sway_candidate = cfg!(feature = "backend-sway")
        && wayland
        && !std::env::var("SWAYSOCK").unwrap_or_default().is_empty()
        && command_exists("swaymsg");
    let (remote_desktop, screencast, embedded_cursor, metadata_cursor, portal_error) =
        match timeout(Duration::from_secs(5), portal_interfaces()).await {
            Ok(Ok((remote_desktop, screencast, embedded_cursor, metadata_cursor))) => (
                remote_desktop,
                screencast,
                embedded_cursor,
                metadata_cursor,
                None,
            ),
            Ok(Err(error)) => (false, false, false, false, Some(error)),
            Err(_) => (
                false,
                false,
                false,
                false,
                Some("desktop portal discovery timed out".into()),
            ),
        };
    let x11_probe =
        tokio::task::spawn_blocking(move || !wayland && x11 && x11::x11_test_available());
    let x11_display_probe =
        tokio::task::spawn_blocking(move || !wayland && x11 && x11::x11_display_available());
    let hyprland_probe =
        tokio::task::spawn_blocking(move || hyprland_candidate && hyprland_layout().is_ok());
    let hyprland_cursor_probe = tokio::task::spawn_blocking(move || {
        hyprland_candidate && hyprland_cursor_position().is_some()
    });
    let pointer_overlay_probe =
        tokio::task::spawn_blocking(move || hyprland_candidate && hyprland_overlay::available());
    let hyprland_capture_probe =
        tokio::task::spawn_blocking(move || hyprland_candidate && hyprland_capture::available());
    let x11_pointer_probe = tokio::task::spawn_blocking(move || {
        !wayland && x11 && x11::x11_pointer_position().is_some()
    });
    let sway_probe =
        tokio::task::spawn_blocking(move || sway_candidate && sway::sway_layout().is_ok());
    let wlroots_layout_probe = tokio::task::spawn_blocking(move || {
        wayland && command_exists("wlr-randr") && wlroots_layout().is_ok()
    });
    let wayland_clipboard_probe =
        tokio::task::spawn_blocking(move || wayland && wayland_clipboard::available());
    let xkb_probe = tokio::task::spawn_blocking(xkb_available);
    let ffmpeg_x11_probe =
        tokio::task::spawn_blocking(move || !wayland && x11 && x11::ffmpeg_x11grab_available());
    let virtual_keyboard_probe = tokio::task::spawn_blocking(move || wayland && wtype_available());
    let hyprland_virtual_keyboard_probe =
        tokio::task::spawn_blocking(move || hyprland_candidate && virtual_keyboard::available());
    let virtual_pointer_probe = tokio::task::spawn_blocking(move || {
        if wayland {
            virtual_pointer::probe()
        } else {
            Err("not a Wayland session".into())
        }
    });
    let wayland_display_probe = tokio::task::spawn_blocking(move || {
        if wayland {
            virtual_pointer::display_probe()
        } else {
            Err("not a Wayland session".into())
        }
    });
    let gst_probe = tokio::task::spawn_blocking(gst_capture_available);
    let gst_compositor_probe = tokio::task::spawn_blocking(gst_compositor_available);
    let atspi_probe =
        tokio::task::spawn_blocking(|| probe_atspi().map(|result| result["available"] == true));
    let (
        x11_probe,
        x11_display_probe,
        hyprland_probe,
        hyprland_cursor_probe,
        pointer_overlay_probe,
        hyprland_capture_probe,
        x11_pointer_probe,
        sway_probe,
        wlroots_layout_probe,
        xkb_probe,
        wayland_clipboard_probe,
        ffmpeg_x11_probe,
        virtual_keyboard_probe,
        hyprland_virtual_keyboard_probe,
        virtual_pointer_probe,
        wayland_display_probe,
        gst_probe,
        gst_compositor_probe,
        atspi_probe,
    ) = tokio::join!(
        x11_probe,
        x11_display_probe,
        hyprland_probe,
        hyprland_cursor_probe,
        pointer_overlay_probe,
        hyprland_capture_probe,
        x11_pointer_probe,
        sway_probe,
        wlroots_layout_probe,
        xkb_probe,
        wayland_clipboard_probe,
        ffmpeg_x11_probe,
        virtual_keyboard_probe,
        hyprland_virtual_keyboard_probe,
        virtual_pointer_probe,
        wayland_display_probe,
        gst_probe,
        gst_compositor_probe,
        atspi_probe
    );
    let x11_input = x11_probe.unwrap_or(false);
    let x11_display = x11_display_probe.unwrap_or(false);
    let hyprland = hyprland_probe.unwrap_or(false);
    let hyprland_pointer_position = hyprland_cursor_probe.unwrap_or(false);
    let visual_pointer_overlay = pointer_overlay_probe.unwrap_or(false);
    let hyprland_native_capture = hyprland_capture_probe.unwrap_or(false);
    let x11_pointer_position = x11_pointer_probe.unwrap_or(false);
    let sway = sway_probe.unwrap_or(false);
    let wlroots_layout = wlroots_layout_probe.unwrap_or(false);
    let xkb = xkb_probe.unwrap_or(false);
    let ffmpeg_x11grab = !wayland && x11 && ffmpeg_x11_probe.unwrap_or(false);
    let libei = portal::libei_available();
    let virtual_keyboard = virtual_keyboard_probe.unwrap_or(false);
    let hyprland_virtual_keyboard = hyprland_virtual_keyboard_probe.unwrap_or(false);
    let virtual_pointer_error = virtual_pointer_probe
        .map(|result| result.err())
        .unwrap_or_else(|error| Some(format!("virtual-pointer probe failed: {error}")));
    let virtual_pointer = virtual_pointer_error.is_none();
    let wayland_display_error = wayland_display_probe
        .map(|result| result.err())
        .unwrap_or_else(|error| Some(format!("Wayland display probe failed: {error}")));
    let wayland_display = wayland_display_error.is_none();
    let pipewire_capture = gst_probe.unwrap_or(false);
    let gstreamer_compositor = gst_compositor_probe.unwrap_or(false);
    let (atspi, atspi_probe_error) = match atspi_probe {
        Ok(Ok(available)) => (available, None),
        Ok(Err(error)) => (false, Some(error)),
        Err(error) => (false, Some(format!("AT-SPI probe worker failed: {error}"))),
    };
    let grim_capture = wayland && wayland_display && grim_available;
    let grim_cursor_capture = grim_capture && grim_supports_cursor_capture();
    let wayland_capture = screencast && pipewire_capture;
    let wayland_sway = sway && grim_capture && (!wayland_capture || !remote_desktop);
    let wayland_grim = !wayland_capture && grim_capture && !wayland_sway;
    let xdotool_text_input = x11_input && command_exists("xdotool");
    let x11_layout_text_input = x11_input && x11::x11_layout_text_available();
    let x11_text_input = xdotool_text_input || x11_layout_text_input;
    let x11_clipboard =
        !wayland && x11_display && (command_exists("xclip") || command_exists("xsel"));
    let native_wayland_clipboard = wayland_clipboard_probe.unwrap_or(false);
    let wayland_clipboard = native_wayland_clipboard
        || wayland && wayland_display && command_exists("wl-copy") && command_exists("wl-paste");
    let backend = if wayland {
        if hyprland && hyprland_native_capture {
            "hyprland-native"
        } else if hyprland && wayland_capture {
            "hyprland"
        } else if wayland_sway {
            "sway-grim"
        } else if wayland_capture {
            "wayland-portal"
        } else if hyprland && grim_capture {
            "hyprland-grim"
        } else if wayland_grim {
            "wayland-grim"
        } else {
            "unsupported"
        }
    } else if x11 {
        "x11"
    } else {
        "unsupported"
    };
    let screenshot = if wayland {
        wayland_capture
            || hyprland && (hyprland_native_capture || grim_capture)
            || wayland_sway
            || wayland_grim
    } else if x11 {
        x11_display && (command_exists("import") || ffmpeg_x11grab)
    } else {
        false
    };
    let portal_backend = backend == "wayland-portal";
    let wtype_backend =
        backend == "sway-grim" || backend == "wayland-grim" || portal_backend && !remote_desktop;
    let portal_keyboard = portal_backend && remote_desktop;
    let direct_keyboard =
        backend == "hyprland" || backend == "hyprland-native" || backend == "hyprland-grim";
    let mut notes = capability_notes(CapabilityFacts {
        backend,
        screenshot,
        atspi,
        remote_desktop,
        wayland,
        x11,
        x11_input,
        hyprland_candidate,
        hyprland,
        sway_candidate,
        sway,
        xkb,
        virtual_pointer,
        embedded_cursor,
        grim_cursor_capture,
        wlroots_layout,
        screencast,
        pipewire_capture,
        virtual_pointer_error: virtual_pointer_error.as_deref(),
    });
    if hyprland && hyprland_virtual_keyboard && !xkb {
        notes.push("libxkbcommon is unavailable; Hyprland keypresses use the Rust XKB backend when standard evdev/pc105 XKB data is installed, otherwise the embedded US map.".into());
    }
    if hyprland && !visual_pointer_overlay {
        notes.push("The native Wayland layer-shell overlay could not connect; computer use remains available with the shared pointer.".into());
    }
    if wayland && pipewire_capture && !gstreamer_compositor {
        notes.push("Portal capture is available for one source; selecting multiple sources needs the GStreamer compositor plugin.".into());
    }
    if !wayland && x11 && screenshot && !ffmpeg_x11grab {
        notes.push(
            "The X11 screenshot omits the cursor; FFmpeg with x11grab can capture it.".into(),
        );
    }
    if !wayland && x11 && x11_input && !x11_text_input {
        notes.push(
            "X11 keypresses are available, but text typing needs xdotool or XKB keymap support."
                .into(),
        );
    }
    if wayland && !wayland_clipboard {
        notes.push("Plain-text clipboard actions need the ext-data-control protocol or wl-copy and wl-paste, plus access to the active Wayland session.".into());
    }
    if wayland
        && screenshot
        && (wtype_backend && virtual_keyboard || direct_keyboard && hyprland || portal_keyboard)
    {
        let unicode_text_input = hyprland && (hyprland_virtual_keyboard || virtual_keyboard)
            || wtype_backend && virtual_keyboard
            || portal_keyboard && libei;
        if !unicode_text_input {
            notes.push(if hyprland {
                "Hyprland key and ASCII text input work through compositor IPC, but Unicode typing needs the virtual-keyboard protocol or an available wtype fallback."
            } else {
                "Text input is available, but Unicode typing is not confirmed. Install wtype for a compositor with virtual-keyboard support, or use set_text when the target exposes AT-SPI editable text."
            }.into());
        }
    }
    if !wayland && x11 && !x11_clipboard {
        notes.push(
            "Plain-text clipboard actions need xclip or xsel and a reachable X11 display.".into(),
        );
    }
    if wayland && let Some(error) = portal_error {
        notes.push(format!(
            "Desktop portal interfaces could not be queried: {error}."
        ));
    }
    if wayland && hyprland && screencast && !remote_desktop {
        notes.push(hyprland_screencast_note(
            hyprland_native_capture,
            grim_capture,
        ));
    }
    if wayland && !wayland_display {
        let detail = if std::env::var("WAYLAND_DISPLAY")
            .unwrap_or_default()
            .is_empty()
        {
            "WAYLAND_DISPLAY is unset".to_owned()
        } else {
            wayland_display_error.unwrap_or_else(|| "connection probe failed".into())
        };
        notes.push(format!(
            "The Wayland display is unavailable to this process: {detail}."
        ));
    }
    if x11 && !wayland && !x11_display {
        notes.push("The X11 display is configured, but this process cannot connect to it.".into());
    }
    let (outputs, output_discovery_error) = if hyprland {
        match tokio::task::spawn_blocking(hyprland_overlay::monitor_choices).await {
            Ok(Ok(outputs)) => (outputs, None),
            Ok(Err(error)) => (Vec::new(), Some(error)),
            Err(error) => (
                Vec::new(),
                Some(format!("monitor discovery worker failed: {error}")),
            ),
        }
    } else {
        (Vec::new(), None)
    };
    let mut hyprland_missing_dependencies = Vec::new();
    if !grim_available {
        hyprland_missing_dependencies.push("grim");
    }
    let available_backends = vec![
        json!({"name":"hyprland-native","target":target_of_backend("hyprland-native"),"available":wayland && hyprland && hyprland_native_capture,"supported":true,"output_scope":"one Hyprland output; pass output","missing_dependencies":[]}),
        json!({"name":"hyprland-grim","target":target_of_backend("hyprland-grim"),"available":wayland && hyprland && grim_capture,"supported":true,"output_scope":"one Hyprland output; pass output","missing_dependencies":hyprland_missing_dependencies}),
        json!({"name":"wayland-portal","target":target_of_backend("wayland-portal"),"available":cfg!(feature = "backend-portal") && wayland && wayland_capture,"supported":false,"output_scope":"portal-selected source(s)"}),
        json!({"name":"sway-grim","target":target_of_backend("sway-grim"),"available":cfg!(feature = "backend-sway") && wayland && sway && grim_capture,"supported":false,"output_scope":"Sway compositor output layout"}),
        json!({"name":"wayland-grim","target":target_of_backend("wayland-grim"),"available":wayland && !hyprland && !sway && wlroots_layout && grim_capture,"supported":false,"output_scope":"wlroots output layout"}),
        json!({"name":"x11","target":target_of_backend("x11"),"available":cfg!(feature = "backend-x11") && !wayland && x11 && screenshot,"supported":false,"output_scope":"X11 display"}),
    ];
    let recommended_backend = available_backends
        .iter()
        .find(|backend| backend["available"] == true && backend["supported"] == true)
        .and_then(|backend| backend["name"].as_str());
    let targets = TARGETS
        .iter()
        .map(|target| {
            let backends = available_backends
                .iter()
                .filter(|backend| backend["target"] == *target);
            let available = backends.clone().any(|backend| backend["available"] == true);
            let supported = backends.clone().any(|backend| backend["supported"] == true);
            json!({"name":target,"available":available,"supported":supported})
        })
        .collect::<Vec<_>>();
    let recommended_target = recommended_backend.and_then(target_of_backend);
    let recommended_output = if outputs.len() == 1 && output_discovery_error.is_none() {
        outputs[0]["name"].as_str()
    } else {
        None
    };
    let mut status = json!({
        "session_type": if session_type.is_empty() { if wayland {"wayland"} else if x11 {"x11"} else {"unknown"} } else {&session_type},
        "desktop": std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
        "compositor": std::env::var("XDG_SESSION_DESKTOP").unwrap_or_default(),
        "backend": backend,
        "available_backends": available_backends,
        "outputs": outputs,
        "capabilities": {
            "helper_process_supervision": "in-process process-group deadlines",
            "screenshot": screenshot,
            "cursor_capture": screenshot && if backend == "x11" { ffmpeg_x11grab } else if backend == "hyprland-native" { hyprland_native_capture } else if backend == "hyprland-grim" || backend == "sway-grim" || backend == "wayland-grim" { grim_cursor_capture } else { wayland && embedded_cursor },
            "cursor_metadata_capture": wayland && metadata_cursor,
            "pointer_position": if backend == "x11" { x11_pointer_position } else if hyprland { hyprland_pointer_position } else { false },
            "pointer_input": if wayland { screenshot && match backend {
                "sway-grim" | "hyprland-grim" | "hyprland-native" => true,
                "hyprland" => hyprland || remote_desktop,
                "wayland-portal" => remote_desktop,
                "wayland-grim" => wlroots_layout && virtual_pointer,
                _ => false,
            } } else { x11_input },
            "keyboard_input": if wayland { screenshot && (wtype_backend && virtual_keyboard || direct_keyboard && hyprland || portal_keyboard) } else { x11_input },
            "text_input": if wayland { screenshot && (wtype_backend && virtual_keyboard || direct_keyboard && hyprland || portal_keyboard) } else { x11_text_input },
            "unicode_text_input": if wayland {
                screenshot && (hyprland && (hyprland_virtual_keyboard || virtual_keyboard) || wtype_backend && virtual_keyboard || portal_keyboard && libei)
            } else {
                screenshot && xdotool_text_input
            },
            "xdotool_text_input": xdotool_text_input,
            "x11_layout_text_input": x11_layout_text_input,
            "clipboard_read": x11_clipboard || wayland_clipboard,
            "clipboard_write": x11_clipboard || wayland_clipboard,
            "scroll_input": if wayland {
                screenshot && match backend {
                    "sway-grim" => true,
                    "hyprland-grim" | "hyprland-native" => virtual_pointer,
                    "hyprland" => remote_desktop || virtual_pointer,
                    "wayland-portal" => remote_desktop,
                    "wayland-grim" => wlroots_layout && virtual_pointer,
                    _ => false,
                }
            } else { x11_input },
            "smooth_scroll_input": if wayland {
                screenshot && match backend {
                    "sway-grim" | "hyprland-grim" | "hyprland-native" => virtual_pointer,
                    "wayland-grim" => wlroots_layout && virtual_pointer,
                    "hyprland" => remote_desktop || virtual_pointer,
                    "wayland-portal" => remote_desktop,
                    _ => false,
                }
            } else { false },
            "accessibility_tree": atspi && backend != "wayland-grim",
            "accessibility_actions": atspi && backend != "wayland-grim",
            "remote_desktop_portal": remote_desktop,
            "screencast_portal": screencast,
            "pipewire_capture_pipeline": pipewire_capture,
            "multi_stream_capture": wayland && pipewire_capture && gstreamer_compositor,
            "grim_capture": grim_capture,
            "grim_cursor_capture": grim_cursor_capture,
            "hyprland_native_screencopy": hyprland_native_capture,
            "display_connection": if wayland { wayland_display } else if x11 { x11_display } else { false },
            "libei": libei,
            "virtual_pointer_protocol": virtual_pointer,
            "xkbcommon": xkb,
            "virtual_keyboard_protocol": virtual_keyboard,
            "compositor_layout_capture": grim_capture,
            "wayland_output_layout": wlroots_layout,
            "hyprland_adapter": hyprland,
            "visual_pointer_overlay": visual_pointer_overlay,
            "sway_adapter": sway,
        },
        "notes": notes
    });
    status["capabilities"]["build_features"] = json!({"backend_x11": cfg!(feature = "backend-x11"), "backend_sway": cfg!(feature = "backend-sway"), "backend_portal": cfg!(feature = "backend-portal")});
    status["capabilities"]["virtual_keyboard_protocol"] =
        json!(virtual_keyboard || hyprland_virtual_keyboard);
    status["capabilities"]["hyprland_virtual_keyboard"] = json!(hyprland_virtual_keyboard);
    status["capabilities"]["wtype_available"] = json!(virtual_keyboard);
    status["capabilities"]["native_wayland_clipboard"] = json!(native_wayland_clipboard);
    status["capabilities"]["accessibility_probe_error"] = json!(atspi_probe_error);
    let remedy = accessibility_remedy(atspi_probe_error.as_deref());
    if let (Some(remedy), Some(notes)) = (&remedy, status["notes"].as_array_mut()) {
        notes.push(json!(remedy));
    }
    status["capabilities"]["accessibility_remedy"] = json!(remedy);
    status["recommended_backend"] = json!(recommended_backend);
    status["targets"] = json!(targets);
    status["recommended_target"] = json!(recommended_target);
    status["recommended_output"] = json!(recommended_output);
    status["output_selection_required"] =
        json!(hyprland && (outputs.len() != 1 || output_discovery_error.is_some()));
    status["output_discovery_error"] = json!(output_discovery_error);
    status
}

pub(crate) fn hyprland_screencast_note(native_capture: bool, grim_capture: bool) -> String {
    let route = if native_capture {
        "Auto and compositor routes use native Hyprland screencopy with Hyprland input and do not prompt for portal consent."
    } else if grim_capture {
        "Auto and compositor routes use Grim with Hyprland input and do not prompt for portal consent."
    } else {
        "No native or Grim compositor capture fallback is available, so auto and compositor routes cannot start."
    };
    format!(
        "This Hyprland session exposes ScreenCast but not the RemoteDesktop portal. {route} Explicit portal mode still asks for ScreenCast consent. The wlroots virtual-pointer protocol provides scrolling."
    )
}

/// What the status probe found, for choosing the notes that explain it.
pub(crate) struct CapabilityFacts<'a> {
    pub(crate) backend: &'a str,
    pub(crate) screenshot: bool,
    pub(crate) atspi: bool,
    pub(crate) remote_desktop: bool,
    pub(crate) wayland: bool,
    pub(crate) x11: bool,
    pub(crate) x11_input: bool,
    pub(crate) hyprland_candidate: bool,
    pub(crate) hyprland: bool,
    pub(crate) sway_candidate: bool,
    pub(crate) sway: bool,
    pub(crate) xkb: bool,
    pub(crate) virtual_pointer: bool,
    pub(crate) embedded_cursor: bool,
    pub(crate) grim_cursor_capture: bool,
    pub(crate) wlroots_layout: bool,
    pub(crate) screencast: bool,
    pub(crate) pipewire_capture: bool,
    pub(crate) virtual_pointer_error: Option<&'a str>,
}

pub(crate) fn capability_notes(facts: CapabilityFacts<'_>) -> Vec<String> {
    let CapabilityFacts {
        backend,
        screenshot,
        atspi,
        remote_desktop,
        wayland,
        x11,
        x11_input,
        hyprland_candidate,
        hyprland,
        sway_candidate,
        sway,
        xkb,
        virtual_pointer,
        embedded_cursor,
        grim_cursor_capture,
        wlroots_layout,
        screencast: _screencast,
        pipewire_capture: _pipewire_capture,
        virtual_pointer_error,
    } = facts;
    let mut notes = Vec::new();
    let grim_backend =
        backend == "hyprland-grim" || backend == "sway-grim" || backend == "wayland-grim";
    let native_backend = backend == "hyprland-native";
    if !wayland && !x11 {
        notes.push("No graphical session was detected: DISPLAY and WAYLAND_DISPLAY are unset. Run the server in the logged-in desktop session and pass its display environment.".into());
    }
    if hyprland_candidate && !hyprland {
        notes.push("Hyprland environment variables are set, but its IPC socket cannot provide the active monitor layout; check compositor socket access.".into());
    }
    if sway_candidate && !sway {
        notes.push("Sway environment variables are set, but swaymsg cannot read the active output layout; check compositor socket access.".into());
    }
    #[cfg(feature = "backend-portal")]
    {
        if wayland && !_screencast && !grim_backend && !native_backend {
            notes.push("No ScreenCast portal was detected; check the session D-Bus connection and desktop portal backend.".into());
        }
        if wayland && _screencast && !_pipewire_capture && !grim_backend && !native_backend {
            notes.push("The ScreenCast portal is present, but capture needs GStreamer with pipewiresrc, videoconvert, videoscale, and jpegenc plugins.".into());
        }
    }
    let cursor_capture = if native_backend {
        true
    } else if grim_backend {
        grim_cursor_capture
    } else {
        embedded_cursor
    };
    if wayland && screenshot && !cursor_capture {
        notes.push(
            "This screenshot backend cannot capture the pointer; screenshots may omit it.".into(),
        );
    }
    if wayland
        && !virtual_pointer
        && let Some(error) = virtual_pointer_error
    {
        notes.push(format!(
            "The wlroots virtual-pointer protocol is unavailable: {error}."
        ));
    }
    if backend == "wayland-portal" && remote_desktop && !xkb {
        notes.push("Common key names work through built-in X keysym values; extended key names need libxkbcommon.".into());
    }
    if backend == "wayland-grim" {
        notes.push(if wlroots_layout && virtual_pointer {
            "Grim screenshots are mapped using the compositor output layout from wlr-randr; the wlroots virtual-pointer protocol provides pointer and scroll input. Keyboard input is available only when wtype can connect to its virtual-keyboard protocol.".into()
        } else if virtual_pointer {
            "The compositor exposes virtual-pointer input, but its output layout is unavailable through wlr-randr; pointer and scroll input are disabled because screenshot coordinates cannot be mapped safely. Keyboard input is available only when wtype can connect to its virtual-keyboard protocol.".into()
        } else {
            "This compositor supports Grim screenshots; keyboard input is available only when wtype can connect to its virtual-keyboard protocol. Pointer and scroll input also require wlr-randr output geometry and the wlroots virtual-pointer protocol.".into()
        });
    }
    if backend == "sway-grim" {
        notes.push(if virtual_pointer {
            "The compositor advertises the wlroots virtual-pointer protocol; session startup will try it for Sway pointer and scroll input. Sway IPC cursor commands remain the fallback, and wtype can provide keyboard input when its virtual-keyboard protocol is available.".into()
        } else {
            "Sway IPC provides pointer movement, clicks, and scrolling through deprecated seat cursor commands; wtype provides keyboard input only when installed and the compositor supports the virtual-keyboard protocol.".into()
        });
    }
    if backend == "wayland-portal" && !remote_desktop {
        notes.push("This Wayland session can capture through ScreenCast, but has no RemoteDesktop input portal.".into());
    }
    if !wayland && x11 && !x11_input {
        notes.push("X11 input is unavailable; check that the X11 and Xtst runtime libraries are installed, the display is reachable, and the XTEST extension is enabled.".into());
    }
    if !screenshot && wayland {
        notes.push("No usable Wayland screenshot backend is available; check the session D-Bus, ScreenCast portal, and PipeWire/GStreamer setup.".into());
    } else if !screenshot && x11 {
        notes.push(
            "X11 screenshots require ImageMagick's import command and a reachable display.".into(),
        );
    }
    if !atspi {
        notes.push(format!("AT-SPI is unavailable. {}", ATSPI_BRIDGES));
    }
    notes
}

const ATSPI_BRIDGES: &str = "GTK and Qt 6 applications expose their accessibility tree once the bus is enabled; Chromium and Electron applications need --force-renderer-accessibility.";

/// Turns an AT-SPI probe error into the most likely cause and its fix.
pub(crate) fn accessibility_remedy(error: Option<&str>) -> Option<String> {
    let error = error?;
    let remedy = if error.contains("at-spi") && error.contains("Connection refused") {
        "The accessibility bus socket exists but its broker no longer accepts connections, which can happen after a D-Bus or at-spi2-core upgrade reloads the bus. Restart it with `systemctl --user restart at-spi-dbus-bus.service`, then restart the applications you want to control."
    } else if error.contains("ServiceUnknown")
        || error.contains("org.a11y.Bus")
        || error.contains("not provided by any .service files")
    {
        "No accessibility bus service is available. Install at-spi2-core (it provides org.a11y.Bus), then log in again."
    } else if error.contains("DBUS_SESSION_BUS_ADDRESS")
        || error.contains("session bus")
        || error.contains("No such file or directory")
    {
        "The server cannot reach the Login Session's D-Bus. Start it from the desktop session or through scripts/computer-use-mcp.sh, which imports DBUS_SESSION_BUS_ADDRESS from systemd."
    } else {
        "Check that at-spi2-core is installed and its bus is running (`systemctl --user status at-spi-dbus-bus.service`)."
    };
    Some(remedy.to_owned())
}

pub(crate) fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| {
            std::fs::metadata(directory.join(name)).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
    })
}

pub(crate) fn grim_supports_cursor_capture() -> bool {
    limited_command("2s", "grim")
        .arg("-h")
        .output()
        .is_ok_and(|output| {
            output.stdout.windows(2).any(|value| value == b"-c")
                || output.stderr.windows(2).any(|value| value == b"-c")
        })
}

pub(crate) fn hyprland_monitor_logical_size(monitor: &Value) -> Result<(u32, u32, f64), String> {
    let width = monitor["width"]
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or("Hyprland monitor omitted its width")?;
    let height = monitor["height"]
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or("Hyprland monitor omitted its height")?;
    let scale = monitor["scale"]
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or("Hyprland monitor omitted its scale")?;
    let transform = match monitor.get("transform") {
        None => 0,
        Some(value) => value
            .as_u64()
            .filter(|value| *value <= 7)
            .ok_or("Hyprland monitor has an invalid transform")?,
    };
    let (width, height) = if transform % 2 == 1 {
        (height, width)
    } else {
        (width, height)
    };
    let dimension = |value: f64| -> Result<u32, String> {
        let value = (value / scale).round();
        if value.is_finite() && (1.0..=f64::from(u32::MAX)).contains(&value) {
            Ok(value as u32)
        } else {
            Err("Hyprland monitor has invalid logical dimensions".into())
        }
    };
    Ok((dimension(width)?, dimension(height)?, scale))
}
