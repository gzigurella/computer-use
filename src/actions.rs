//! `computer_act`: validation and execution of Actions against an Observation.

use super::*;

/// Captures a Screenshot of the Selected Output without creating an Observation,
/// so the latest observation_id stays valid. The caller holds the action lock.
pub(crate) async fn screenshot_only(
    session_id: &str,
    state: SharedState,
    legacy_action: bool,
) -> Result<Value, String> {
    await_held_input_cleanups().await;
    let (backend, composite_pointer_overlay, observation_id, image_limits) = {
        let guard = state.lock().await;
        let session = guard.session.as_ref().ok_or("no desktop session is open")?;
        if session.id != session_id {
            return Err("unknown desktop session".into());
        }
        let observation_id = session
            .observation
            .as_ref()
            .filter(|(_, observation)| observation.captured_at.elapsed() <= OBSERVATION_TTL)
            .map(|(id, _)| id.clone());
        (
            session.backend.clone(),
            session.pointer_overlay.is_some(),
            observation_id,
            session.image_limits,
        )
    };
    let capture_backend = backend.clone();
    let frame = tokio::task::spawn_blocking(move || {
        capture(&capture_backend, composite_pointer_overlay, image_limits)
    })
    .await
    .map_err(|error| format!("capture worker failed: {error}"))??;
    if frame.image.len() > MAX_IMAGE {
        return Err("screenshot exceeds 3 MiB".into());
    }
    let action = json!({"performed":true,"action":"screenshot"});
    let mut metadata = if legacy_action {
        action
    } else {
        json!({"performed":true,"results":[action]})
    };
    metadata["session_id"] = json!(session_id);
    metadata["observation_id"] = json!(observation_id);
    metadata["observation_replaced"] = json!(false);
    metadata["display"] = json!({
        "width":frame.pixel_width,
        "height":frame.pixel_height,
        "logical_width":frame.logical_width,
        "logical_height":frame.logical_height,
        "coordinate_space":"captured output pixels",
        "origin_x":frame.origin_x,
        "origin_y":frame.origin_y,
        "origin_known":frame.origin_known,
        "source":frame.source,
        "pointer":frame.pointer.map(|(x, y)| json!({"x":x,"y":y,"coordinate_space":"captured output pixels"}))
    });
    if let Backend::Hyprland { monitor, .. } = &backend {
        metadata["selected_output"] = json!(monitor);
    }
    Ok(observation_result(metadata, frame.image))
}

pub(crate) async fn action_result_with_observation(
    session_id: &str,
    state: SharedState,
    action_result: Value,
) -> Value {
    await_held_input_cleanups().await;
    match observe(&json!({"session_id":session_id}), state).await {
        Ok(mut observation) => {
            let mut metadata = action_result.as_object().cloned().unwrap_or_default();
            metadata.insert(
                "observation".into(),
                observation["structuredContent"].clone(),
            );
            let metadata = Value::Object(metadata);
            observation["content"][0]["text"] = Value::String(metadata.to_string());
            observation["structuredContent"] = metadata;
            observation
        }
        Err(error) => {
            let mut metadata = action_result.as_object().cloned().unwrap_or_default();
            metadata.insert("observation_error".into(), json!(error));
            metadata.insert("observe_again".into(), Value::Bool(true));
            tool_result(Value::Object(metadata), Vec::new())
        }
    }
}

pub(crate) async fn act(
    args: &Value,
    state: SharedState,
    mut cancelled: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<Value, String> {
    let action_deadline = tokio::time::Instant::now() + ACTION_TIMEOUT;
    let session_id = required_string(args, "session_id")?;
    let legacy_action = args.get("action").is_some();
    let actions = match (args.get("action"), args.get("actions")) {
        (Some(action), None) => vec![action.clone()],
        (None, Some(Value::Array(actions))) if (1..=8).contains(&actions.len()) => actions.clone(),
        (None, Some(Value::Array(_))) => return Err("actions must contain 1 to 8 actions".into()),
        (None, Some(_)) => return Err("actions must be an array".into()),
        (Some(_), Some(_)) => return Err("provide action or actions, not both".into()),
        (None, None) => return Err("action or actions is required".into()),
    };
    let mut action_types = Vec::with_capacity(actions.len());
    let mut wait_total = 0u64;
    for action in &actions {
        let action_type = required_string(action, "type")?;
        if !matches!(
            action_type,
            "screenshot"
                | "click"
                | "double_click"
                | "move"
                | "drag"
                | "scroll"
                | "key"
                | "keypress"
                | "type"
                | "wait"
                | "accessibility"
                | "set_text"
                | "clipboard_read"
                | "clipboard_write"
        ) {
            return Err("unsupported action type".into());
        }
        if matches!(
            action_type,
            "click" | "double_click" | "move" | "drag" | "scroll"
        ) {
            mouse_modifiers(action)?;
        }
        if matches!(action_type, "key" | "keypress") {
            key_action(action)?;
        }
        if action_type == "scroll" {
            scroll_deltas(action)?;
        }
        if matches!(action_type, "type" | "clipboard_write" | "set_text") {
            let text = action
                .get("text")
                .and_then(Value::as_str)
                .ok_or("text is required")?;
            validate_action_text(action_type, text)?;
        }
        if action_type == "wait" {
            wait_total = wait_total.saturating_add(wait_duration(action)?);
            if wait_total > 10_000 {
                return Err("total batch wait cannot exceed 10000 milliseconds".into());
            }
        }
        action_types.push(action_type.to_owned());
    }
    if action_types.len() > 1
        && action_types
            .iter()
            .any(|action_type| action_type == "screenshot")
    {
        return Err("a screenshot action must be the only action in a call".into());
    }
    let observation_id = args.get("observation_id").and_then(Value::as_str);
    let action_lock = state.lock().await.action_lock.clone();
    let lock_wait = action_deadline.saturating_duration_since(tokio::time::Instant::now());
    let lock_result = if let Some(cancelled) = cancelled.as_mut() {
        tokio::select! {
            result = timeout(lock_wait, action_lock.lock()) => result.map_err(|_| "computer_act timed out waiting for the desktop action lock".to_owned()),
            _ = cancelled.changed() => return Err("computer_act was cancelled while waiting for the desktop action lock".into()),
        }
    } else {
        timeout(lock_wait, action_lock.lock())
            .await
            .map_err(|_| "computer_act timed out waiting for the desktop action lock".to_owned())
    }?;
    let _action_guard = lock_result;
    if action_types == ["screenshot"] {
        return screenshot_only(session_id, state, legacy_action).await;
    }
    let (backend, observation) = {
        let mut guard = state.lock().await;
        let session = guard.session.as_mut().ok_or("no desktop session is open")?;
        if session.id != session_id {
            return Err("unknown desktop session".into());
        }
        let needs_observation = action_types.iter().any(|action_type| {
            matches!(
                action_type.as_str(),
                "click"
                    | "double_click"
                    | "move"
                    | "drag"
                    | "scroll"
                    | "key"
                    | "keypress"
                    | "type"
                    | "accessibility"
                    | "set_text"
            )
        });
        let observation = if needs_observation {
            let oid = observation_id.ok_or("this action requires observation_id")?;
            Some(
                session
                    .observation
                    .as_ref()
                    .filter(|(key, value)| {
                        key == oid && value.captured_at.elapsed() <= OBSERVATION_TTL
                    })
                    .map(|(_, value)| value.clone())
                    .ok_or("observation is stale or expired; observe again")?,
            )
        } else {
            None
        };
        if let Some(observation) = &observation {
            for (action, action_type) in actions.iter().zip(&action_types) {
                validate_observed_action(action, action_type, observation)?;
            }
        }
        session.observation = None;
        (session.backend.clone(), observation)
    };
    let mut results = Vec::with_capacity(actions.len());
    let mut expected_focused_element = observation
        .as_ref()
        .and_then(|value| value.focused_element.clone());
    let mut expected_active_window = observation
        .as_ref()
        .and_then(|value| value.active_window.clone());
    let mut pointer_action_since_focus_check = false;
    for (action, action_type) in actions.iter().zip(&action_types) {
        let mut current_action_started = false;
        let remaining = action_deadline.saturating_duration_since(tokio::time::Instant::now());
        let was_cancelled = cancelled.as_ref().is_some_and(cancellation_requested);
        let result: Result<Value, String> = if was_cancelled {
            Err("computer_act was cancelled before this action started; remaining batch actions were skipped".into())
        } else if remaining.is_zero() {
            Err("computer_act deadline reached before this action started; the rest of the batch was not run".into())
        } else {
            async {
            if let Some(observation) = observation.as_ref() {
                let backend_for_layout = backend.clone();
                let observation_for_layout = observation.clone();
                run_blocking_result("display layout check", move || ensure_current_layout(&backend_for_layout, &observation_for_layout)).await?;
                if matches!(action_type.as_str(), "click" | "double_click" | "move" | "drag" | "scroll" | "key" | "keypress" | "type" | "accessibility" | "set_text") {
                    let backend_for_focus = backend.clone();
                    let expected_window = expected_active_window.clone();
                    run_blocking_result("window focus check", move || ensure_observation_focus(&backend_for_focus, &expected_window)).await?;
                }
                if matches!(action_type.as_str(), "key" | "keypress" | "type") {
                    let backend_for_monitor = backend.clone();
                    run_blocking_result("Hyprland monitor focus check", move || ensure_hyprland_focus_on_monitor(&backend_for_monitor)).await?;
                }
            }
            if matches!(action_type.as_str(), "key" | "keypress" | "type")
                && (expected_focused_element.is_some() || pointer_action_since_focus_check)
            {
                let actual = tokio::task::spawn_blocking(focused_element_now)
                    .await.map_err(|error| format!("focus check worker failed: {error}"))??;
                if pointer_action_since_focus_check {
                    if expected_focused_element.is_some() && actual.is_none() {
                        return Err("keyboard focus could not be verified after the pointer action; observe again".into());
                    }
                    expected_focused_element = actual;
                } else if !same_focused_element(&expected_focused_element, &actual) {
                    return Err("keyboard focus changed since observation; observe again".into());
                }
                pointer_action_since_focus_check = false;
            }
            let result = if action_type == "wait" {
                current_action_started = true;
                let duration = wait_duration(action)?;
                let wait = timeout(remaining, tokio::time::sleep(std::time::Duration::from_millis(duration)));
                let completed = if let Some(receiver) = cancelled.as_mut() {
                    tokio::select! {
                        biased;
                        _ = receiver.changed() => return Err("computer_act was cancelled during a wait; remaining batch actions were skipped".into()),
                        result = wait => result.is_ok(),
                    }
                } else {
                    wait.await.is_ok()
                };
                if !completed {
                    return Err("computer_act deadline reached during a wait; remaining batch actions were skipped".into());
                }
                json!({"performed":true,"action":"wait","duration_ms":duration})
            } else {
                if cancelled.as_ref().is_some_and(cancellation_requested) {
                    return Err("computer_act was cancelled before input began".into());
                }
                current_action_started = true;
                perform_action(&backend, action, observation.as_ref(), cancelled.as_mut()).await?
            };
            if matches!(action_type.as_str(), "click" | "double_click" | "move" | "drag") {
                pointer_action_since_focus_check = true;
                if expected_active_window.is_some() {
                    expected_active_window = tokio::task::spawn_blocking({
                        let backend = backend.clone();
                        move || active_window_now(&backend)
                    }).await.map_err(|error| format!("active-window check worker failed: {error}"))??;
                    if expected_active_window.is_none() {
                        return Err("active window could not be verified after the pointer action; observe again".into());
                    }
                }
            }
            if matches!(action_type.as_str(), "key" | "keypress" | "type")
                && expected_focused_element.is_some()
                && let Ok(Ok(Some(actual))) = tokio::task::spawn_blocking(focused_element_now).await {
                    expected_focused_element = Some(actual);
                }
            Ok(result)
            }.await
        };
        match result {
            Ok(result) => results.push(result),
            Err(error) => {
                if cancelled.as_ref().is_some_and(cancellation_requested) {
                    await_held_input_cleanups().await;
                    return Err(error);
                }
                let action_result = json!({
                    "performed":false,
                    "completed_actions":results.len(),
                    "current_action":action_type,
                    "current_action_started":current_action_started,
                    "results":results,
                    "error":error
                });
                drop(_action_guard);
                return Ok(action_result_with_observation(
                    session_id,
                    state.clone(),
                    action_result,
                )
                .await);
            }
        }
    }
    if cancelled.as_ref().is_some_and(cancellation_requested) {
        await_held_input_cleanups().await;
        return Err("computer_act was cancelled after the current action completed".into());
    }
    let action_result = if legacy_action {
        results.pop().expect("single action was validated")
    } else {
        json!({"performed":true,"results":results})
    };
    drop(_action_guard);
    Ok(action_result_with_observation(session_id, state, action_result).await)
}

pub(crate) fn cancellation_requested(receiver: &tokio::sync::watch::Receiver<bool>) -> bool {
    receiver.has_changed().is_err() || *receiver.borrow()
}

pub(crate) async fn perform_action(
    backend: &Backend,
    action: &Value,
    observation: Option<&Observation>,
    mut cancelled: Option<&mut tokio::sync::watch::Receiver<bool>>,
) -> Result<Value, String> {
    match action["type"].as_str().unwrap_or_default() {
        "screenshot" => Ok(json!({"performed":true,"action":"screenshot"})),
        "click" | "double_click" => {
            let observation = observation.ok_or("observation required")?;
            let x = coordinate(action, "x", observation.width)?;
            let y = coordinate(action, "y", observation.height)?;
            let button = pointer_button(action)?;
            let modifiers = mouse_modifiers(action)?;
            let mut held = HeldInput::new(backend);
            if let Err(error) = press_mouse_modifiers(backend, &modifiers, &mut held).await {
                return Err(match held.release_all().await {
                    Ok(()) => error,
                    Err(cleanup) => format!("{error}; modifier cleanup failed: {cleanup}"),
                });
            }
            let clicks = if action["type"] == "double_click" {
                2
            } else {
                1
            };
            let click_result = async {
                for click in 0..clicks {
                    if cancelled.as_deref().is_some_and(cancellation_requested) {
                        return Err("computer_act was cancelled before the next click".to_owned());
                    }
                    if click > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    if matches!(backend, Backend::X11 { .. }) {
                        let (x, y) = rounded_pointer_coordinates(observation, x, y);
                        run_blocking_result("X11 click", move || {
                            x11::x11_click_at(x as i32, y as i32, button)
                        })
                        .await?;
                        continue;
                    }
                    move_pointer(backend, observation, x, y, true).await?;
                    if cancelled.as_deref().is_some_and(cancellation_requested) {
                        return Err(
                            "computer_act was cancelled before the pointer button was pressed"
                                .to_owned(),
                        );
                    }
                    held.add_button(button);
                    let down = pointer_button_state(backend, button, true).await;
                    let up = release_pointer_button(backend, button).await;
                    if up.is_ok() {
                        held.remove_button(button);
                    }
                    down?;
                    up?;
                }
                Ok::<(), String>(())
            }
            .await;
            let cleanup_result = held.release_all().await;
            match (click_result, cleanup_result) {
                (Ok(()), Ok(())) => {}
                (Err(error), Ok(())) | (Ok(()), Err(error)) => return Err(error),
                (Err(error), Err(cleanup)) => {
                    return Err(format!("{error}; modifier cleanup failed: {cleanup}"));
                }
            }
            Ok(
                json!({"performed":true,"action":action["type"],"x":x,"y":y,"button":button_name(button),"keys":modifiers}),
            )
        }
        "move" => {
            let observation = observation.ok_or("observation required")?;
            let x = coordinate(action, "x", observation.width)?;
            let y = coordinate(action, "y", observation.height)?;
            let modifiers = mouse_modifiers(action)?;
            let mut held = HeldInput::new(backend);
            if let Err(error) = press_mouse_modifiers(backend, &modifiers, &mut held).await {
                return Err(match held.release_all().await {
                    Ok(()) => error,
                    Err(cleanup) => format!("{error}; modifier cleanup failed: {cleanup}"),
                });
            }
            let movement = move_pointer(backend, observation, x, y, false).await;
            let cleanup = held.release_all().await;
            match (movement, cleanup) {
                (Ok(()), Ok(())) => {
                    Ok(json!({"performed":true,"action":"move","x":x,"y":y,"keys":modifiers}))
                }
                (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
                (Err(error), Err(cleanup)) => {
                    Err(format!("{error}; modifier cleanup failed: {cleanup}"))
                }
            }
        }
        "drag" => {
            let observation = observation.ok_or("observation required")?;
            let path = drag_path(action, observation)?;
            let (x, y) = path[0];
            let (to_x, to_y) = *path.last().expect("validated drag path is non-empty");
            let button = pointer_button(action)?;
            let modifiers = mouse_modifiers(action)?;
            if matches!(backend, Backend::X11 { .. }) {
                let path = path
                    .into_iter()
                    .map(|(x, y)| rounded_pointer_coordinates(observation, x, y))
                    .collect();
                let worker_modifiers = modifiers.clone();
                let worker = tokio::task::spawn_blocking(move || {
                    x11::x11_drag_path(path, button, worker_modifiers)
                });
                let (sender, receiver) = tokio::sync::oneshot::channel();
                track_held_input_cleanup(tokio::spawn(async move {
                    let _ = sender.send(worker.await);
                }));
                receiver
                    .await
                    .map_err(|error| format!("X11 drag worker failed: {error}"))?
                    .map_err(|error| format!("X11 drag worker failed: {error}"))??;
                return Ok(
                    json!({"performed":true,"action":"drag","from":{"x":x,"y":y},"to":{"x":to_x,"y":to_y},"button":button_name(button),"keys":modifiers}),
                );
            }
            move_pointer(backend, observation, x, y, true).await?;
            if cancelled.as_deref().is_some_and(cancellation_requested) {
                return Err("computer_act was cancelled before the drag button was pressed".into());
            }
            let mut held = HeldInput::new(backend);
            if let Err(error) = press_mouse_modifiers(backend, &modifiers, &mut held).await {
                return Err(match held.release_all().await {
                    Ok(()) => error,
                    Err(cleanup) => format!("{error}; modifier cleanup failed: {cleanup}"),
                });
            }
            if cancelled.as_deref().is_some_and(cancellation_requested) {
                return Err("computer_act was cancelled before the drag button was pressed".into());
            }
            held.add_button(button);
            if let Err(error) = pointer_button_state(backend, button, true).await {
                return Err(match held.release_all().await {
                    Ok(()) => error,
                    Err(cleanup) => format!("{error}; button cleanup failed: {cleanup}"),
                });
            }
            let mut movement = Ok(());
            for (x, y) in path.into_iter().skip(1) {
                if cancelled.as_deref().is_some_and(cancellation_requested) {
                    movement = Err("computer_act was cancelled during drag".into());
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(8)).await;
                if cancelled.as_deref().is_some_and(cancellation_requested) {
                    movement = Err("computer_act was cancelled during drag".into());
                    break;
                }
                if let Err(error) = move_pointer(backend, observation, x, y, true).await {
                    movement = Err(error);
                    break;
                }
            }
            let released = release_pointer_button(backend, button).await;
            if released.is_ok() {
                held.remove_button(button);
            }
            let cleanup = held.release_all().await;
            match (movement, released, cleanup) {
                (Ok(()), Ok(()), Ok(())) => {}
                (Err(error), Ok(()), Ok(()))
                | (Ok(()), Err(error), Ok(()))
                | (Ok(()), Ok(()), Err(error)) => return Err(error),
                (Err(error), Err(release), Ok(())) => return Err(format!("{error}; {release}")),
                (Err(error), _, Err(cleanup)) | (_, Err(error), Err(cleanup)) => {
                    return Err(format!("{error}; drag cleanup failed: {cleanup}"));
                }
            }
            Ok(
                json!({"performed":true,"action":"drag","from":{"x":x,"y":y},"to":{"x":to_x,"y":to_y},"button":button_name(button),"keys":modifiers}),
            )
        }
        "scroll" => {
            let observation = observation.ok_or("observation required")?;
            let x = coordinate(action, "x", observation.width)?;
            let y = coordinate(action, "y", observation.height)?;
            let (scroll_x, scroll_y) = scroll_deltas(action)?;
            let smooth_request =
                action.get("scroll_x").is_some() || action.get("scroll_y").is_some();
            let smooth_available = smooth_scroll_available(backend);
            let smooth_applied = smooth_request && smooth_available;
            let (effective_x, effective_y) = if smooth_applied {
                (scroll_x, scroll_y)
            } else if smooth_request {
                (
                    pixel_distance_to_steps(scroll_x),
                    pixel_distance_to_steps(scroll_y),
                )
            } else {
                (scroll_x, scroll_y)
            };
            let mut axes = Vec::with_capacity(2);
            if effective_x < 0 {
                axes.push(("left", effective_x.unsigned_abs()));
            }
            if effective_x > 0 {
                axes.push(("right", effective_x as u32));
            }
            if effective_y < 0 {
                axes.push(("up", effective_y.unsigned_abs()));
            }
            if effective_y > 0 {
                axes.push(("down", effective_y as u32));
            }
            let modifiers = mouse_modifiers(action)?;
            let mut held = HeldInput::new(backend);
            if let Err(error) = press_mouse_modifiers(backend, &modifiers, &mut held).await {
                return Err(match held.release_all().await {
                    Ok(()) => error,
                    Err(cleanup) => format!("{error}; modifier cleanup failed: {cleanup}"),
                });
            }
            let scrolling = async {
                if smooth_applied {
                    if let Some(pointer) = direct_scroll_pointer(backend) {
                        if matches!(
                            backend,
                            Backend::Hyprland { .. } | Backend::Wayland { hyprland: true, .. }
                        ) {
                            move_pointer(backend, observation, x, y, false).await?;
                        } else {
                            let (px, py) = portal_coordinates(observation, x, y);
                            let mut pointer = pointer
                                .lock()
                                .map_err(|_| "virtual-pointer lock is unavailable")?;
                            pointer.pointer(
                                px,
                                py,
                                observation.logical_width,
                                observation.logical_height,
                            )?;
                        }
                        return pointer
                            .lock()
                            .map_err(|_| "virtual-pointer lock is unavailable")?
                            .scroll_smooth(scroll_x, scroll_y);
                    }
                    if let Backend::Wayland { portal, .. } = backend {
                        let (px, py) = portal_coordinates(observation, x, y);
                        portal.pointer_for_scroll(px, py).await?;
                        return portal
                            .scroll_smooth(f64::from(scroll_x), f64::from(scroll_y))
                            .await;
                    }
                    return Err("smooth scrolling is unavailable through this backend".into());
                }
                for (direction, amount) in axes {
                    if let Some(pointer) = direct_scroll_pointer(backend) {
                        if matches!(
                            backend,
                            Backend::Hyprland { .. } | Backend::Wayland { hyprland: true, .. }
                        ) {
                            move_pointer(backend, observation, x, y, false).await?;
                        } else {
                            let (px, py) = portal_coordinates(observation, x, y);
                            pointer
                                .lock()
                                .map_err(|_| "virtual-pointer lock is unavailable")?
                                .pointer(
                                    px,
                                    py,
                                    observation.logical_width,
                                    observation.logical_height,
                                )?;
                        }
                        pointer
                            .lock()
                            .map_err(|_| "virtual-pointer lock is unavailable")?
                            .scroll(direction, amount as i32)?;
                    } else {
                        match backend {
                            Backend::X11 { .. } => {
                                let button = scroll_button(direction)?;
                                let (x, y) = rounded_pointer_coordinates(observation, x, y);
                                run_blocking_result("X11 scroll", move || {
                                    x11::x11_scroll_at(x as i32, y as i32, button, amount as usize)
                                })
                                .await?;
                            }
                            Backend::Sway { .. } => {
                                let observation = observation.clone();
                                let button = scroll_button(direction)?;
                                run_blocking_result("Sway scroll", move || {
                                    sway::sway_move(&observation, x, y)?;
                                    sway::sway_scroll(button, amount as usize)
                                })
                                .await?;
                            }
                            Backend::Hyprland { .. } => return Err(
                                "scroll injection is not available through the Hyprland adapter"
                                    .into(),
                            ),
                            Backend::WaylandGrim { .. } => {
                                return Err(
                                    "Wayland input is unavailable in the Grim capture-only session"
                                        .into(),
                                );
                            }
                            Backend::Wayland {
                                portal,
                                hyprland: true,
                                ..
                            } if !portal.scroll_available() => return Err(
                                "scroll injection is not available through this Hyprland adapter"
                                    .into(),
                            ),
                            Backend::Wayland { portal, .. } => {
                                let (px, py) = portal_coordinates(observation, x, y);
                                portal.pointer_for_scroll(px, py).await?;
                                portal.scroll(direction, amount as i32).await?;
                            }
                            Backend::Unsupported => {
                                return Err("scroll input is unsupported".into());
                            }
                        }
                    }
                }
                Ok::<(), String>(())
            }
            .await;
            let cleanup = held.release_all().await;
            match (scrolling, cleanup) {
                (Ok(()), Ok(())) => {
                    let mode = if smooth_applied {
                        "smooth"
                    } else if smooth_request {
                        "discrete_fallback"
                    } else {
                        "discrete"
                    };
                    let mut result = json!({"performed":true,"action":"scroll","scroll_x":scroll_x,"scroll_y":scroll_y,"scroll_mode":mode,"x":x,"y":y,"keys":modifiers});
                    if smooth_request && !smooth_applied {
                        result["scroll_steps_x"] = json!(effective_x);
                        result["scroll_steps_y"] = json!(effective_y);
                    }
                    if let Some(direction) = action.get("direction") {
                        result["direction"] = direction.clone();
                    }
                    if let Some(amount) = action.get("amount") {
                        result["amount"] = amount.clone();
                    }
                    Ok(result)
                }
                (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
                (Err(error), Err(cleanup)) => {
                    Err(format!("{error}; modifier cleanup failed: {cleanup}"))
                }
            }
        }
        "key" | "keypress" => {
            let (key, modifiers) = key_action(action)?;
            let (keyboard_layout, automatic_modifiers) =
                send_key(backend, &key, &modifiers).await?;
            let mut result =
                json!({"performed":true,"action":action["type"],"key":key,"modifiers":modifiers});
            if let Some((layout, source)) = keyboard_layout {
                result["keyboard_layout"] = json!({"layout":layout,"source":source});
            }
            if !automatic_modifiers.is_empty() {
                result["automatic_modifiers"] = json!(automatic_modifiers);
            }
            Ok(result)
        }
        "wait" => {
            let duration = wait_duration(action)?;
            tokio::time::sleep(std::time::Duration::from_millis(duration)).await;
            Ok(json!({"performed":true,"action":"wait","duration_ms":duration}))
        }
        "clipboard_read" => {
            let backend = backend.clone();
            let text = tokio::task::spawn_blocking(move || clipboard_read(&backend))
                .await
                .map_err(|error| format!("clipboard read worker failed: {error}"))??;
            Ok(json!({"performed":true,"action":"clipboard_read","text":text}))
        }
        "clipboard_write" => {
            let text = action
                .get("text")
                .and_then(Value::as_str)
                .ok_or("text is required")?;
            let bytes = text.len();
            let backend = backend.clone();
            let text = text.to_owned();
            tokio::task::spawn_blocking(move || clipboard_write(&backend, &text))
                .await
                .map_err(|error| format!("clipboard write worker failed: {error}"))??;
            Ok(json!({"performed":true,"action":"clipboard_write","bytes":bytes}))
        }
        "type" => {
            let text = required_string(action, "text")?;
            let portal_text =
                matches!(backend, Backend::Wayland { portal, .. } if portal.keyboard_available());
            if portal_text {
                let Backend::Wayland {
                    portal,
                    hyprland_virtual_keyboard,
                    virtual_keyboard,
                    ..
                } = backend
                else {
                    unreachable!("portal text input requires Wayland")
                };
                if portal.unicode_text_available() {
                    portal.type_text(text)?;
                } else if *hyprland_virtual_keyboard {
                    let text = text.to_owned();
                    type_text_native(text, cancelled.as_deref_mut()).await?;
                } else if *virtual_keyboard {
                    let text = text.to_owned();
                    run_blocking_result("wtype text", move || wtype_text(&text)).await?;
                } else {
                    let keysyms = text.chars().map(|character| match character {
                        '\t' => Ok(0xff09),
                        '\n' | '\r' => Ok(0xff0d),
                        value if value.is_control() => Err("text input does not support control characters other than tab and newline".to_owned()),
                        value if (value as u32) <= 0xff => Ok(value as u32),
                        value => Ok(0x0100_0000 | value as u32),
                    }).collect::<Result<Vec<_>, String>>()?;
                    let mut held = HeldInput::new(backend);
                    for keysym in keysyms {
                        if cancelled.as_deref().is_some_and(cancellation_requested) {
                            return Err(
                                "text input cancelled after releasing the previous key".into()
                            );
                        }
                        held.add_portal_key(keysym);
                        let down = portal_key_state(backend, keysym, true).await;
                        let up = release_portal_key(portal, keysym).await;
                        if up.is_ok() {
                            held.remove_portal_key(keysym);
                        }
                        if let Err(error) = portal_key_cycle(down, up) {
                            return Err(match held.release_all().await {
                                Ok(()) => error,
                                Err(cleanup) => {
                                    format!("{error}; text key cleanup failed: {cleanup}")
                                }
                            });
                        }
                    }
                }
            } else {
                if matches!(
                    backend,
                    Backend::Hyprland {
                        native_keyboard: true,
                        ..
                    } | Backend::Wayland {
                        hyprland: true,
                        hyprland_virtual_keyboard: true,
                        ..
                    }
                ) {
                    let text = text.to_owned();
                    type_text_native(text, cancelled).await?;
                } else if matches!(
                    backend,
                    Backend::Hyprland {
                        wtype_keyboard: true,
                        ..
                    } | Backend::Sway {
                        virtual_keyboard: true,
                        ..
                    } | Backend::WaylandGrim {
                        virtual_keyboard: true,
                        ..
                    } | Backend::Wayland {
                        virtual_keyboard: true,
                        ..
                    }
                ) {
                    let text = text.to_owned();
                    run_blocking_result("wtype text", move || wtype_text(&text)).await?;
                } else {
                    let keys = text
                        .chars()
                        .map(character_key)
                        .collect::<Result<Vec<_>, _>>()?;
                    if uses_hyprland_keyboard(backend) {
                        let commands = keys
                            .iter()
                            .map(|(key, shift)| {
                                let modifier = if *shift { "SHIFT" } else { "" };
                                format!("dispatch sendshortcut {modifier},{key},activewindow")
                            })
                            .collect::<Vec<_>>();
                        run_blocking_result("Hyprland text", move || hypr_batch(&commands)).await?;
                    } else if matches!(backend, Backend::X11 { .. }) {
                        if command_exists("xdotool") {
                            let text = text.to_owned();
                            run_blocking_result("xdotool text", move || x11::xdotool_type(&text))
                                .await?;
                        } else {
                            let text = text.to_owned();
                            run_blocking_result("X11 text", move || x11::x11_type_ascii(&text))
                                .await?;
                        }
                    } else {
                        return Err("keyboard input is unsupported".into());
                    }
                }
            }
            Ok(json!({"performed":true,"action":"type","characters":text.chars().count()}))
        }
        "accessibility" => {
            let observation = observation.ok_or("observation required")?;
            let element_id = required_string(action, "element_id")?;
            let Some(element) = observation.elements.get(element_id) else {
                return Err("invalid element_id".into());
            };
            if !element.on_screen {
                return Err("element is not visible in the observed output".into());
            }
            let (action_name, action_index) = select_accessibility_action(element, action)?;
            let response_element_id = element_id.to_owned();
            let element_id = response_element_id.clone();
            let name = element.name.clone();
            let role = element.role.clone();
            let process_id = element.process_id;
            let accessible_id = element.accessible_id.clone();
            let bounds = element.bounds.clone();
            let response_action_name = action_name.clone();
            let action_name = response_action_name.clone();
            run_blocking_result("AT-SPI action", move || {
                atspi_action(
                    &element_id,
                    &name,
                    &role,
                    process_id,
                    accessible_id.as_deref(),
                    &bounds,
                    &action_name,
                    action_index,
                )
            })
            .await?;
            Ok(
                json!({"performed":true,"action":"accessibility","element_id":response_element_id,"action_name":response_action_name,"action_index":action_index}),
            )
        }
        "set_text" => {
            let observation = observation.ok_or("observation required")?;
            let element_id = required_string(action, "element_id")?;
            let element = observation
                .elements
                .get(element_id)
                .ok_or("invalid element_id")?;
            if !element.editable || !element.on_screen {
                return Err("element is not an observed, visible editable text control".into());
            }
            let text = action
                .get("text")
                .and_then(Value::as_str)
                .ok_or("text is required")?;
            let response_element_id = element_id.to_owned();
            let element_id = response_element_id.clone();
            let name = element.name.clone();
            let role = element.role.clone();
            let process_id = element.process_id;
            let accessible_id = element.accessible_id.clone();
            let bounds = element.bounds.clone();
            let characters = text.chars().count();
            let text = text.to_owned();
            run_blocking_result("AT-SPI text", move || {
                atspi_set_text(
                    &element_id,
                    &name,
                    &role,
                    process_id,
                    accessible_id.as_deref(),
                    &bounds,
                    &text,
                )
            })
            .await?;
            Ok(
                json!({"performed":true,"action":"set_text","element_id":response_element_id,"characters":characters}),
            )
        }
        _ => Err("unsupported action type".into()),
    }
}

pub(crate) async fn run_blocking_result<T, F>(operation: &'static str, task: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|error| format!("{operation} worker failed: {error}"))?
}

pub(crate) async fn type_text_native(
    text: String,
    cancelled: Option<&mut tokio::sync::watch::Receiver<bool>>,
) -> Result<(), String> {
    let cancellation = Arc::new(AtomicBool::new(false));
    let worker_cancellation = cancellation.clone();
    let mut worker = tokio::task::spawn_blocking(move || {
        virtual_keyboard::type_text_cancellable(&text, &worker_cancellation)
    });
    if let Some(cancelled) = cancelled {
        tokio::select! {
            biased;
            result = &mut worker => result.map_err(|error| format!("native Wayland text worker failed: {error}"))?,
            _ = cancelled.changed() => {
                cancellation.store(true, Ordering::Release);
                let result = worker.await.map_err(|error| format!("native Wayland text worker failed during cleanup: {error}"))?;
                match result {
                    Ok(()) => Err("native Wayland text input cancelled after the current key was released".into()),
                    Err(error) => Err(error),
                }
            }
        }
    } else {
        worker
            .await
            .map_err(|error| format!("native Wayland text worker failed: {error}"))?
    }
}

pub(crate) fn ensure_current_layout(
    backend: &Backend,
    observation: &Observation,
) -> Result<(), String> {
    if matches!(backend, Backend::X11 { .. }) && observation.layout.is_none() {
        return Ok(());
    }
    let current = match backend {
        Backend::Hyprland { monitor, .. } => Some(hyprland_monitor_layout(monitor)?),
        Backend::Sway { .. } => Some(sway::sway_layout()?),
        Backend::WaylandGrim {
            layout_capture: true,
            ..
        } => Some(wlroots_layout()?),
        Backend::X11 { .. } => {
            let (width, height) =
                x11::x11_screen_size().ok_or("X11 screen geometry unavailable; observe again")?;
            Some((0, 0, width, height))
        }
        _ => None,
    };
    if current.is_some_and(|layout| observation.layout != Some(layout)) {
        return Err("display layout changed since observation; observe again".into());
    }
    Ok(())
}

pub(crate) fn ensure_observation_focus(
    backend: &Backend,
    expected: &Option<Value>,
) -> Result<(), String> {
    let Some(expected) = expected else {
        return Ok(());
    };
    if !same_active_window(&Some(expected.clone()), &active_window_now(backend)?) {
        return Err("active window changed since observation; observe again".into());
    }
    Ok(())
}

pub(crate) fn ensure_hyprland_focus_on_monitor(backend: &Backend) -> Result<(), String> {
    let Backend::Hyprland { monitor, .. } = backend else {
        return Ok(());
    };
    let active = hyprland_active_window()?
        .ok_or("keyboard input requires an active window on the selected Hyprland output")?;
    let bounds = &active["bounds"];
    let (Some(x), Some(y), Some(width), Some(height)) = (
        bounds["x"].as_i64(),
        bounds["y"].as_i64(),
        bounds["width"].as_u64(),
        bounds["height"].as_u64(),
    ) else {
        return Err("keyboard input requires verifiable active-window bounds on the selected Hyprland output".into());
    };
    let (mx, my, mw, mh) = hyprland_monitor_layout(monitor)?;
    let (right, bottom) = (
        x.saturating_add(width as i64),
        y.saturating_add(height as i64),
    );
    if right <= i64::from(mx)
        || bottom <= i64::from(my)
        || x >= i64::from(mx) + i64::from(mw)
        || y >= i64::from(my) + i64::from(mh)
    {
        return Err(
            "active window is outside the selected Hyprland output; keyboard input was not sent"
                .into(),
        );
    }
    Ok(())
}

pub(crate) fn same_active_window(expected: &Option<Value>, actual: &Option<Value>) -> bool {
    match (expected, actual) {
        (None, None) => true,
        (Some(expected), Some(actual)) => {
            if expected["window_id"].is_string() && actual["window_id"].is_string() {
                expected["window_id"] == actual["window_id"]
                    && expected["application"] == actual["application"]
                    // A stable window identity is not enough for screenshot-grounded
                    // coordinates: reject observations spanning a move or resize.
                    && expected["bounds"] == actual["bounds"]
            } else {
                expected == actual
            }
        }
        _ => false,
    }
}

pub(crate) fn same_focused_element(expected: &Option<Value>, actual: &Option<Value>) -> bool {
    match (expected, actual) {
        (None, None) => true,
        (Some(expected), Some(actual)) => {
            expected["element_id"] == actual["element_id"]
                && expected["name"] == actual["name"]
                && expected["role"] == actual["role"]
                && expected["process_id"] == actual["process_id"]
                && expected["bounds"] == actual["bounds"]
                && (!expected["accessible_id"].is_string()
                    || !actual["accessible_id"].is_string()
                    || expected["accessible_id"] == actual["accessible_id"])
        }
        _ => false,
    }
}

pub(crate) fn coordinate(action: &Value, name: &str, extent: u32) -> Result<u32, String> {
    let value = action
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{name} must be a non-negative integer"))?;
    let value = u32::try_from(value).map_err(|_| format!("{name} is too large"))?;
    if value >= extent {
        return Err(format!("{name} is outside the observed display"));
    }
    Ok(value)
}

pub(crate) fn scroll_deltas(action: &Value) -> Result<(i32, i32), String> {
    if action.get("scroll_x").is_some() || action.get("scroll_y").is_some() {
        if action.get("direction").is_some() || action.get("amount").is_some() {
            return Err("use scroll_x/scroll_y or direction/amount, not both".into());
        }
        let delta = |name| -> Result<i32, String> {
            let value = action
                .get(name)
                .map_or(Some(0), Value::as_i64)
                .ok_or_else(|| format!("{name} must be an integer from -1000 to 1000"))?;
            i32::try_from(value)
                .ok()
                .filter(|value| (-1000..=1000).contains(value))
                .ok_or_else(|| format!("{name} must be an integer from -1000 to 1000"))
        };
        let (x, y) = (delta("scroll_x")?, delta("scroll_y")?);
        if x == 0 && y == 0 {
            return Err("scroll_x and scroll_y cannot both be zero".into());
        }
        return Ok((x, y));
    }
    let direction = required_string(action, "direction")?;
    let amount = action
        .get("amount")
        .map(|value| {
            value
                .as_u64()
                .ok_or("scroll amount must be an integer from 1 to 100")
        })
        .transpose()?
        .unwrap_or(1);
    if !(1..=100).contains(&amount) {
        return Err("scroll amount must be an integer from 1 to 100".into());
    }
    let amount = amount as i32;
    match direction {
        "up" => Ok((0, -amount)),
        "down" => Ok((0, amount)),
        "left" => Ok((-amount, 0)),
        "right" => Ok((amount, 0)),
        _ => Err("direction must be up, down, left, or right".into()),
    }
}

pub(crate) fn validate_action_text(action: &str, text: &str) -> Result<(), String> {
    match action {
        "type" => {
            if text.is_empty() {
                return Err("text is required".into());
            }
            if text.len() > 4096 {
                return Err("text exceeds 4096 bytes".into());
            }
            if text
                .chars()
                .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
            {
                return Err(
                    "text input does not support control characters other than tab and newline"
                        .into(),
                );
            }
        }
        "clipboard_write" if text.len() > 64 * 1024 => {
            return Err("clipboard text exceeds 64 KiB".into());
        }
        "set_text" if text.chars().count() > 4096 => {
            return Err("text exceeds 4096 characters".into());
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn wait_duration(action: &Value) -> Result<u64, String> {
    match action.get("duration_ms") {
        None => Ok(2_000),
        Some(value) => value
            .as_u64()
            .filter(|ms| *ms <= 10_000)
            .ok_or_else(|| "duration_ms must be an integer from 0 to 10000".into()),
    }
}

pub(crate) fn drag_path(
    action: &Value,
    observation: &Observation,
) -> Result<Vec<(u32, u32)>, String> {
    if let Some(points) = action.get("path") {
        let points = points.as_array().ok_or("path must be an array")?;
        if !(2..=64).contains(&points.len()) {
            return Err("drag path must contain 2 to 64 points".into());
        }
        points
            .iter()
            .map(|point| {
                Ok((
                    coordinate(point, "x", observation.width)?,
                    coordinate(point, "y", observation.height)?,
                ))
            })
            .collect()
    } else {
        Ok(vec![
            (
                coordinate(action, "x", observation.width)?,
                coordinate(action, "y", observation.height)?,
            ),
            (
                coordinate(action, "to_x", observation.width)?,
                coordinate(action, "to_y", observation.height)?,
            ),
        ])
    }
}

pub(crate) fn key_action(action: &Value) -> Result<(String, Vec<String>), String> {
    if action["type"] == "key" && action.get("keys").is_some() {
        return Err("keys is only supported for keypress; use modifiers with key".into());
    }
    if action["type"] == "keypress" && action.get("keys").is_some() {
        if action.get("key").is_some() || action.get("modifiers").is_some() {
            return Err("use either keys or key with modifiers for keypress, not both".into());
        }
        let keys = action["keys"].as_array().ok_or("keys must be an array")?;
        if !(1..=8).contains(&keys.len()) {
            return Err("keypress keys must contain 1 to 8 entries".into());
        }
        let mut key = None;
        let mut modifiers = Vec::new();
        for value in keys {
            let value = value.as_str().ok_or("each key must be a string")?;
            if value.chars().count() > 32 {
                return Err("key names must be at most 32 characters".into());
            }
            let upper = value.to_ascii_uppercase();
            let modifier = match upper.as_str() {
                "CTRL" | "CONTROL" => Some("CTRL"),
                "SHIFT" => Some("SHIFT"),
                "ALT" => Some("ALT"),
                "SUPER" | "META" | "WINDOWS" => Some("SUPER"),
                _ => None,
            };
            if let Some(modifier) = modifier {
                if !modifiers.iter().any(|existing| existing == modifier) {
                    modifiers.push(modifier.to_owned());
                }
            } else if key.is_none() {
                key = Some(value.to_owned());
            } else {
                return Err("keypress accepts one key and any modifiers".into());
            }
        }
        Ok((
            key.ok_or("keypress keys must include a non-modifier key")?,
            modifiers,
        ))
    } else {
        let key = required_string(action, "key")?;
        if key.chars().count() > 32 {
            return Err("key names must be at most 32 characters".into());
        }
        Ok((key.to_owned(), modifiers(action)?))
    }
}

pub(crate) fn select_accessibility_action(
    element: &AccessibleElement,
    action: &Value,
) -> Result<(String, Option<u64>), String> {
    let requested_name = action
        .get("action_name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty());
    let action_index = action
        .get("action_index")
        .map(|value| {
            value
                .as_u64()
                .ok_or("action_index must be a non-negative integer")
        })
        .transpose()?;
    if let Some(index) = action_index {
        let position = usize::try_from(index).map_err(|_| "action_index is too large")?;
        let advertised = element
            .actions
            .get(position)
            .ok_or("action_index is outside the element's advertised actions")?;
        let name = advertised
            .as_deref()
            .ok_or("AT-SPI did not provide an action name at action_index")?;
        if requested_name.is_some_and(|requested| requested != name) {
            return Err("action_name does not match the action at action_index".into());
        }
        Ok((name.to_owned(), Some(index)))
    } else {
        let name = requested_name.ok_or("action_name or action_index is required")?;
        if !element
            .actions
            .iter()
            .flatten()
            .any(|advertised| advertised == name)
        {
            return Err("action_name was not advertised for this element".into());
        }
        Ok((name.to_owned(), None))
    }
}

pub(crate) fn validate_observed_action(
    action: &Value,
    action_type: &str,
    observation: &Observation,
) -> Result<(), String> {
    match action_type {
        "click" | "double_click" => {
            coordinate(action, "x", observation.width)?;
            coordinate(action, "y", observation.height)?;
            pointer_button(action)?;
        }
        "move" => {
            coordinate(action, "x", observation.width)?;
            coordinate(action, "y", observation.height)?;
        }
        "drag" => {
            drag_path(action, observation)?;
            pointer_button(action)?;
        }
        "scroll" => {
            coordinate(action, "x", observation.width)?;
            coordinate(action, "y", observation.height)?;
        }
        "key" | "keypress" => {
            let (key, _) = key_action(action)?;
            validate_key(&normalize_key(&key))?;
        }
        "accessibility" => {
            let element_id = required_string(action, "element_id")?;
            let element = observation
                .elements
                .get(element_id)
                .ok_or("invalid element_id")?;
            if !element.on_screen {
                return Err("element is not visible in the observed output".into());
            }
            select_accessibility_action(element, action)?;
        }
        "set_text" => {
            let element_id = required_string(action, "element_id")?;
            let element = observation
                .elements
                .get(element_id)
                .ok_or("invalid element_id")?;
            if !element.editable || !element.on_screen {
                return Err("element is not an observed, visible editable text control".into());
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn scroll_button(direction: &str) -> Result<u32, String> {
    match direction {
        "up" => Ok(4),
        "down" => Ok(5),
        "left" => Ok(6),
        "right" => Ok(7),
        _ => Err("invalid scroll direction".into()),
    }
}

pub(crate) fn smooth_scroll_available(backend: &Backend) -> bool {
    if direct_scroll_pointer(backend).is_some() {
        return true;
    }
    matches!(backend, Backend::Wayland { portal, .. } if portal.smooth_scroll_available())
}

pub(crate) fn hyprland_portal_scroll_available(
    hyprland: bool,
    portal_scroll: bool,
    positioned_monitor: bool,
    virtual_pointer: bool,
) -> bool {
    portal_scroll || (hyprland && positioned_monitor && virtual_pointer)
}

pub(crate) fn pixel_distance_to_steps(distance: i32) -> i32 {
    if distance == 0 {
        return 0;
    }
    let steps = distance.unsigned_abs().div_ceil(120).max(1) as i32;
    if distance < 0 { -steps } else { steps }
}

pub(crate) async fn move_pointer(
    backend: &Backend,
    observation: &Observation,
    x: u32,
    y: u32,
    click_target: bool,
) -> Result<(), String> {
    if let Backend::Hyprland {
        direct_pointer: Some(pointer),
        ..
    } = backend
    {
        let (x, y) = rounded_pointer_coordinates(observation, x, y);
        let layout = hyprland_layout()?;
        let (x, y, width, height) = pointer_coordinates_in_layout(
            x,
            y,
            (observation.origin_x, observation.origin_y),
            layout,
        )?;
        return pointer
            .lock()
            .map_err(|_| "virtual-pointer lock is unavailable")?
            .pointer(f64::from(x), f64::from(y), width, height);
    }
    if let Some(pointer) = direct_pointer(backend) {
        let (x, y) = portal_coordinates(observation, x, y);
        return pointer
            .lock()
            .map_err(|_| "virtual-pointer lock is unavailable")?
            .pointer(x, y, observation.logical_width, observation.logical_height);
    }
    match backend {
        Backend::Wayland {
            portal,
            hyprland: true,
            ..
        } if !portal.pointer_available() => {
            if !portal.monitor_geometry_available() || !observation.origin_known {
                return Err("Hyprland pointer fallback needs positioned monitor streams; use portal input or select a positioned monitor".into());
            }
            let (x, y) = rounded_pointer_coordinates(observation, x, y);
            let coordinates = format!(
                "{} {}",
                x as i32 + observation.origin_x,
                y as i32 + observation.origin_y
            );
            run_blocking_result("Hyprland pointer", move || {
                hypr("dispatch", &["movecursor", &coordinates])
            })
            .await
        }
        Backend::Wayland { portal, .. } => {
            let (x, y) = portal_coordinates(observation, x, y);
            if click_target {
                portal.pointer_for_click(x, y).await
            } else {
                portal.pointer(x, y).await
            }
        }
        Backend::Hyprland { .. } => {
            let (x, y) = rounded_pointer_coordinates(observation, x, y);
            let coordinates = format!(
                "{} {}",
                x as i32 + observation.origin_x,
                y as i32 + observation.origin_y
            );
            run_blocking_result("Hyprland pointer", move || {
                hypr("dispatch", &["movecursor", &coordinates])
            })
            .await
        }
        Backend::Sway { .. } => {
            let observation = observation.clone();
            run_blocking_result("Sway pointer", move || sway::sway_move(&observation, x, y)).await
        }
        Backend::WaylandGrim { .. } => {
            Err("Wayland input is unavailable in the Grim capture-only session".into())
        }
        Backend::X11 { .. } => {
            let (x, y) = rounded_pointer_coordinates(observation, x, y);
            run_blocking_result("X11 pointer", move || x11::x11_move(x as i32, y as i32)).await
        }
        Backend::Unsupported => Err("pointer input is unsupported".into()),
    }
}

pub(crate) fn pointer_coordinates_in_layout(
    x: u32,
    y: u32,
    monitor_origin: (i32, i32),
    layout: (i32, i32, u32, u32),
) -> Result<(u32, u32, u32, u32), String> {
    let x = i64::from(monitor_origin.0) + i64::from(x) - i64::from(layout.0);
    let y = i64::from(monitor_origin.1) + i64::from(y) - i64::from(layout.1);
    let width = layout.2;
    let height = layout.3;
    if x < 0 || y < 0 || x >= i64::from(width) || y >= i64::from(height) {
        return Err(
            "selected Hyprland output is outside the virtual-pointer desktop layout".into(),
        );
    }
    Ok((x as u32, y as u32, width, height))
}

pub(crate) async fn pointer_button_state(
    backend: &Backend,
    button: u32,
    pressed: bool,
) -> Result<(), String> {
    if let Some(pointer) = direct_pointer(backend) {
        return pointer
            .lock()
            .map_err(|_| "virtual-pointer lock is unavailable")?
            .button(evdev_button_code(button), pressed);
    }
    match backend {
        Backend::Wayland {
            portal,
            hyprland: true,
            ..
        } if !portal.pointer_available() => {
            if !portal.monitor_geometry_available() {
                return Err("Hyprland pointer fallback needs positioned monitor streams; use portal input or select a positioned monitor".into());
            }
            let state = if pressed { "down" } else { "up" };
            let key = format!(",{},{},activewindow", hypr_mouse_button(button), state);
            run_blocking_result("Hyprland pointer button", move || {
                hypr("dispatch", &["sendkeystate", &key])
            })
            .await
        }
        Backend::Wayland { portal, .. } => portal.button(button, pressed).await,
        Backend::Hyprland { .. } => {
            let state = if pressed { "down" } else { "up" };
            let key = format!(",{},{},activewindow", hypr_mouse_button(button), state);
            run_blocking_result("Hyprland pointer button", move || {
                hypr("dispatch", &["sendkeystate", &key])
            })
            .await
        }
        Backend::Sway { .. } => {
            run_blocking_result("Sway pointer button", move || {
                sway::sway_pointer_button(button, pressed)
            })
            .await
        }
        Backend::WaylandGrim { .. } => {
            Err("Wayland input is unavailable in the Grim capture-only session".into())
        }
        Backend::X11 { .. } => x11::x11_button_state(button, pressed),
        Backend::Unsupported => Err("pointer input is unsupported".into()),
    }
}

pub(crate) async fn release_pointer_button(backend: &Backend, button: u32) -> Result<(), String> {
    match pointer_button_state(backend, button, false).await {
        Ok(()) => Ok(()),
        Err(first) => pointer_button_state(backend, button, false)
            .await
            .map_err(|second| format!("button release failed ({first}); retry failed ({second})")),
    }
}

pub(crate) fn direct_pointer(
    backend: &Backend,
) -> Option<&Arc<std::sync::Mutex<virtual_pointer::VirtualPointer>>> {
    match backend {
        Backend::Hyprland { direct_pointer, .. }
        | Backend::Sway { direct_pointer, .. }
        | Backend::WaylandGrim { direct_pointer, .. } => direct_pointer.as_ref(),
        _ => None,
    }
}

pub(crate) fn direct_scroll_pointer(
    backend: &Backend,
) -> Option<&Arc<std::sync::Mutex<virtual_pointer::VirtualPointer>>> {
    match backend {
        Backend::Wayland { direct_pointer, .. } => direct_pointer.as_ref(),
        _ => direct_pointer(backend),
    }
}

pub(crate) fn direct_keyboard(
    backend: &Backend,
) -> Option<&Arc<std::sync::Mutex<virtual_keyboard::VirtualKeyboard>>> {
    match backend {
        Backend::Wayland {
            direct_keyboard, ..
        }
        | Backend::Hyprland {
            direct_keyboard, ..
        } => direct_keyboard.as_ref(),
        _ => None,
    }
}

pub(crate) fn wtype_keyboard_available(backend: &Backend) -> bool {
    matches!(
        backend,
        Backend::Hyprland {
            wtype_keyboard: true,
            ..
        }
    ) || matches!(
        backend,
        Backend::Wayland {
            virtual_keyboard: true,
            ..
        }
    )
}

pub(crate) fn uses_hyprland_keyboard(backend: &Backend) -> bool {
    matches!(backend, Backend::Hyprland { .. })
        || matches!(backend, Backend::Wayland { portal, hyprland: true, .. } if !portal.keyboard_available())
}

pub(crate) fn hypr_mouse_button(button: u32) -> &'static str {
    match button {
        1 => "mouse:272",
        2 => "mouse:274",
        3 => "mouse:273",
        8 => "mouse:275",
        9 => "mouse:276",
        _ => "mouse:273",
    }
}

pub(crate) fn evdev_button_code(button: u32) -> u32 {
    match button {
        1 => 272,
        2 => 274,
        3 => 273,
        8 => 275,
        9 => 276,
        _ => 273,
    }
}

pub(crate) fn pointer_button(action: &Value) -> Result<u32, String> {
    match action["button"].as_str().unwrap_or("left") {
        "left" => Ok(1),
        "middle" | "wheel" => Ok(2),
        "right" => Ok(3),
        "back" => Ok(8),
        "forward" => Ok(9),
        _ => Err("invalid button".into()),
    }
}

pub(crate) fn button_name(button: u32) -> &'static str {
    match button {
        1 => "left",
        2 => "middle",
        3 => "right",
        8 => "back",
        9 => "forward",
        _ => "unknown",
    }
}

pub(crate) fn modifiers(action: &Value) -> Result<Vec<String>, String> {
    let Some(values) = action.get("modifiers") else {
        return Ok(Vec::new());
    };
    let items = values.as_array().ok_or("modifiers must be an array")?;
    if items.len() > 4 {
        return Err("at most four modifier keys are accepted".into());
    }
    let mut result = Vec::new();
    for item in items {
        let value = item.as_str().ok_or("modifier must be a string")?;
        if value.chars().count() > 32 {
            return Err("modifier names must be at most 32 characters".into());
        }
        let value = value.to_ascii_uppercase();
        let modifier = match value.as_str() {
            "CTRL" | "CONTROL" => "CTRL",
            "SHIFT" => "SHIFT",
            "ALT" => "ALT",
            "SUPER" | "META" | "WINDOWS" => "SUPER",
            _ => return Err("unsupported modifier".into()),
        };
        if !result.iter().any(|existing| existing == modifier) {
            result.push(modifier.to_owned());
        }
    }
    Ok(result)
}

pub(crate) fn mouse_modifiers(action: &Value) -> Result<Vec<String>, String> {
    let Some(values) = action.get("keys") else {
        return Ok(Vec::new());
    };
    let values = values.as_array().ok_or("mouse keys must be an array")?;
    if values.len() > 4 {
        return Err("mouse actions accept at most four modifier keys".into());
    }
    let mut result = Vec::new();
    for item in values {
        let value = item.as_str().ok_or("mouse modifier must be a string")?;
        if value.chars().count() > 32 {
            return Err("mouse modifier names must be at most 32 characters".into());
        }
        let value = value.to_ascii_uppercase();
        let modifier = match value.as_str() {
            "CTRL" | "CONTROL" => "CTRL",
            "SHIFT" => "SHIFT",
            "ALT" => "ALT",
            "SUPER" | "META" | "WINDOWS" => "SUPER",
            _ => return Err("mouse keys may contain only CTRL, SHIFT, ALT, or SUPER".into()),
        };
        if !result.iter().any(|existing| existing == modifier) {
            result.push(modifier.to_owned());
        }
    }
    Ok(result)
}

pub(crate) async fn modifier_state(
    backend: &Backend,
    modifier: &str,
    pressed: bool,
) -> Result<(), String> {
    key_state(backend, modifier_key(modifier)?, pressed).await
}

pub(crate) async fn press_mouse_modifiers(
    backend: &Backend,
    modifiers: &[String],
    held: &mut HeldInput,
) -> Result<(), String> {
    for modifier in modifiers {
        let key = modifier_key(modifier)?;
        held.add_key(key);
        modifier_state(backend, modifier, true).await?;
    }
    Ok(())
}

pub(crate) fn modifier_key(modifier: &str) -> Result<&'static str, String> {
    match modifier {
        "CTRL" => Ok("Control_L"),
        "SHIFT" => Ok("Shift_L"),
        "ALT" => Ok("Alt_L"),
        "SUPER" => Ok("Super_L"),
        "ALTGR" => Ok("ALTGR"),
        _ => Err("unsupported mouse modifier".into()),
    }
}

pub(crate) async fn key_state(backend: &Backend, key: &str, pressed: bool) -> Result<(), String> {
    validate_key(key)?;
    if uses_hyprland_keyboard(backend) {
        let keyboard = direct_keyboard(backend)
            .cloned()
            .ok_or("held modifiers require Hyprland virtual-keyboard support")?;
        let key = key.to_owned();
        return run_blocking_result("Hyprland modifier", move || {
            keyboard
                .lock()
                .map_err(|_| "virtual keyboard lock was poisoned".to_owned())?
                .key_state(&key, pressed)
        })
        .await;
    }
    match backend {
        Backend::Wayland { portal, .. } if portal.keyboard_available() => {
            portal.key(portal_keysym(key)?, pressed).await
        }
        Backend::X11 { .. } => x11::x11_key_state(key, pressed),
        _ => Err("held key input is not supported by the active input backend".into()),
    }
}

pub(crate) async fn release_key_state(backend: &Backend, key: &str) -> Result<(), String> {
    match key_state(backend, key, false).await {
        Ok(()) => Ok(()),
        Err(first) => key_state(backend, key, false)
            .await
            .map_err(|second| format!("key release failed ({first}); retry failed ({second})")),
    }
}

pub(crate) async fn portal_key_state(
    backend: &Backend,
    keysym: u32,
    pressed: bool,
) -> Result<(), String> {
    match backend {
        Backend::Wayland { portal, .. } => portal.key(keysym, pressed).await,
        _ => Err("portal keysym input is unavailable on the active backend".into()),
    }
}

pub(crate) async fn release_portal_keysym(backend: &Backend, keysym: u32) -> Result<(), String> {
    match portal_key_state(backend, keysym, false).await {
        Ok(()) => Ok(()),
        Err(first) => portal_key_state(backend, keysym, false)
            .await
            .map_err(|second| format!("key release failed ({first}); retry failed ({second})")),
    }
}

pub(crate) async fn send_key(
    backend: &Backend,
    key: &str,
    modifiers: &[String],
) -> Result<(Option<(String, &'static str)>, Vec<String>), String> {
    let key = normalize_key(key);
    validate_key(&key)?;
    if uses_hyprland_keyboard(backend) {
        let Some(keyboard) = direct_keyboard(backend).cloned() else {
            if wtype_keyboard_available(backend) {
                let key = key.clone();
                let modifiers = modifiers.to_vec();
                run_blocking_result("wtype key", move || wtype_key(&key, &modifiers)).await?;
                return Ok((None, Vec::new()));
            }
            return Err(
                "keyboard input is unavailable; the compositor has no active virtual keyboard"
                    .into(),
            );
        };
        let refreshed_layout = run_blocking_result("Hyprland XKB layout refresh", move || {
            keyboard
                .lock()
                .map_err(|_| "virtual keyboard lock was poisoned".to_owned())?
                .refresh_keymap()
        })
        .await?;
        let keyboard = direct_keyboard(backend)
            .cloned()
            .ok_or("Hyprland virtual keyboard stopped during key input")?;
        let key_name = key.clone();
        let automatic_modifiers = run_blocking_result("Hyprland XKB key resolution", move || {
            keyboard
                .lock()
                .map_err(|_| "virtual keyboard lock was poisoned".to_owned())?
                .required_modifiers(&key_name)
        })
        .await?;
        let mut applied_modifiers = modifiers.to_vec();
        for modifier in &automatic_modifiers {
            if !applied_modifiers.iter().any(|applied| applied == modifier) {
                applied_modifiers.push((*modifier).to_owned());
            }
        }
        let mut held = HeldInput::new(backend);
        for modifier in &applied_modifiers {
            let name = modifier_key(modifier)?;
            held.add_key(name);
            if let Err(error) = key_state(backend, name, true).await {
                return Err(match held.release_all().await {
                    Ok(()) => error,
                    Err(cleanup) => format!("{error}; modifier cleanup failed: {cleanup}"),
                });
            }
        }
        held.add_key(&key);
        let key_down = key_state(backend, &key, true).await;
        let key_up = release_key_state(backend, &key).await;
        if key_up.is_ok() {
            held.remove_key(&key);
        }
        let key_result = portal_key_cycle(key_down, key_up);
        let modifier_result = held.release_all().await;
        return match (key_result, modifier_result) {
            (Ok(()), Ok(())) => Ok((
                refreshed_layout,
                automatic_modifiers.into_iter().map(str::to_owned).collect(),
            )),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(error), Err(cleanup)) => {
                Err(format!("{error}; modifier cleanup failed: {cleanup}"))
            }
        };
    }
    match backend {
        Backend::Wayland { virtual_keyboard: true, portal, xkb, .. } if !portal.keyboard_available() || !xkb => {
            let key = key.clone();
            let modifiers = modifiers.to_vec();
            run_blocking_result("wtype key", move || wtype_key(&key, &modifiers)).await?;
            Ok((None, Vec::new()))
        }
        Backend::Wayland { .. } => {
            let mut held = HeldInput::new(backend);
            for modifier in modifiers {
                let name = modifier_key(modifier)?;
                held.add_key(name);
                if let Err(error) = key_state(backend, name, true).await {
                    return Err(match held.release_all().await {
                        Ok(()) => error,
                        Err(cleanup) => format!("{error}; modifier cleanup failed: {cleanup}"),
                    });
                }
            }
            held.add_key(&key);
            let key_down = key_state(backend, &key, true).await;
            let key_up = release_key_state(backend, &key).await;
            if key_up.is_ok() { held.remove_key(&key); }
            let key_result = portal_key_cycle(key_down, key_up);
            let modifier_result = held.release_all().await;
            match (key_result, modifier_result) {
                (Ok(()), Ok(())) => Ok((None, Vec::new())),
                (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
                (Err(error), Err(cleanup)) => Err(format!("{error}; modifier cleanup failed: {cleanup}")),
            }
        }
        Backend::Hyprland { .. } => unreachable!("Hyprland key action returned before direct routing"),
        Backend::Sway { virtual_keyboard: true, .. } | Backend::WaylandGrim { virtual_keyboard: true, .. } => {
            let key = key.clone();
            let modifiers = modifiers.to_vec();
            run_blocking_result("wtype key", move || wtype_key(&key, &modifiers)).await?;
            Ok((None, Vec::new()))
        }
        Backend::Sway { .. } => Err("keyboard input is unavailable; install wtype and use a compositor with virtual-keyboard support".into()),
        Backend::WaylandGrim { .. } => Err("Wayland input is unavailable in the Grim capture-only session".into()),
        Backend::X11 { .. } => {
            let key = key.clone();
            let modifiers = modifiers.to_vec();
            run_blocking_result("X11 key", move || x11::x11_key(&key, &modifiers)).await?;
            Ok((None, Vec::new()))
        }
        Backend::Unsupported => Err("keyboard input is unsupported".into()),
    }
}

pub(crate) async fn release_portal_key(
    portal: &portal::PortalSession,
    keysym: u32,
) -> Result<(), String> {
    match portal.key(keysym, false).await {
        Ok(()) => Ok(()),
        Err(first) => portal
            .key(keysym, false)
            .await
            .map_err(|second| format!("key release failed ({first}); retry failed ({second})")),
    }
}

pub(crate) fn portal_key_cycle(
    down: Result<(), String>,
    up: Result<(), String>,
) -> Result<(), String> {
    match (down, up) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(release)) => Err(format!("{error}; key release failed: {release}")),
    }
}

pub(crate) fn normalize_key(key: &str) -> String {
    match key.to_ascii_uppercase().as_str() {
        "ENTER" | "RETURN" => "Return",
        "ESC" | "ESCAPE" => "Escape",
        "BACKSPACE" | "BACK_SPACE" => "BackSpace",
        "SPACE" => "space",
        "DEL" | "DELETE" => "Delete",
        "INS" | "INSERT" => "Insert",
        "HOME" => "Home",
        "END" => "End",
        "PAGEUP" | "PAGE_UP" | "PRIOR" => "Page_Up",
        "PAGEDOWN" | "PAGE_DOWN" | "NEXT" => "Page_Down",
        "UP" | "ARROWUP" | "ARROW_UP" => "Up",
        "DOWN" | "ARROWDOWN" | "ARROW_DOWN" => "Down",
        "LEFT" | "ARROWLEFT" | "ARROW_LEFT" => "Left",
        "RIGHT" | "ARROWRIGHT" | "ARROW_RIGHT" => "Right",
        "CTRL" | "CONTROL" => "Control_L",
        "SHIFT" => "Shift_L",
        "ALT" => "Alt_L",
        "SUPER" | "META" | "WINDOWS" => "Super_L",
        "CAPSLOCK" | "CAPS_LOCK" => "Caps_Lock",
        "NUMLOCK" | "NUM_LOCK" => "Num_Lock",
        "SCROLLLOCK" | "SCROLL_LOCK" => "Scroll_Lock",
        "PRINTSCREEN" | "PRINT_SCREEN" => "Print",
        "PAUSE" => "Pause",
        "BREAK" => "Break",
        _ => return key.to_owned(),
    }
    .to_owned()
}

pub(crate) fn character_key(character: char) -> Result<(String, bool), String> {
    let result = match character {
        'a'..='z' => (character.to_string(), false),
        'A'..='Z' => (character.to_ascii_lowercase().to_string(), true),
        '0'..='9' => (character.to_string(), false), ' ' => ("space".into(), false),
        '!' => ("1".into(), true), '@'=>("2".into(),true), '#'=>("3".into(),true), '$'=>("4".into(),true), '%'=>("5".into(),true), '^'=>("6".into(),true), '&'=>("7".into(),true), '*'=>("8".into(),true), '('=>("9".into(),true), ')'=>("0".into(),true),
        '-'|'_' => ("minus".into(), character=='_'), '='|'+' => ("equal".into(), character=='+'), '['|'{' => ("bracketleft".into(), character=='{'), ']'|'}' => ("bracketright".into(), character=='}'), '\\'|'|' => ("backslash".into(), character=='|'), ';'|':' => ("semicolon".into(), character==':'), '\''|'"' => ("apostrophe".into(), character=='"'), ','|'<' => ("comma".into(), character=='<'), '.'|'>' => ("period".into(), character=='>'), '/'|'?' => ("slash".into(), character=='?'), '`'|'~' => ("grave".into(), character=='~'),
        '\n'|'\r' => ("Return".into(),false), '\t' => ("Tab".into(),false),
        _ => return Err("text input supports ASCII characters only; use an accessibility action or paste in the target application".into()),
    };
    Ok(result)
}

pub(crate) fn portal_coordinates(observation: &Observation, x: u32, y: u32) -> (f64, f64) {
    (
        x as f64 * observation.logical_width as f64 / observation.width as f64,
        y as f64 * observation.logical_height as f64 / observation.height as f64,
    )
}

pub(crate) fn rounded_pointer_coordinates(observation: &Observation, x: u32, y: u32) -> (u32, u32) {
    let (x, y) = portal_coordinates(observation, x, y);
    (
        x.round()
            .clamp(0.0, observation.logical_width.saturating_sub(1) as f64) as u32,
        y.round()
            .clamp(0.0, observation.logical_height.saturating_sub(1) as f64) as u32,
    )
}

pub(crate) fn portal_keysym(key: &str) -> Result<u32, String> {
    if let Some(keysym) = builtin_portal_keysym(key) {
        return Ok(keysym);
    }
    let library = DynamicLibrary::open(c"libxkbcommon.so.0")?;
    let keysym_from_name: unsafe extern "C" fn(*const c_char, c_int) -> u32 =
        library.symbol(c"xkb_keysym_from_name")?;
    let key = CString::new(key).map_err(|_| "invalid key name")?;
    let value = unsafe { keysym_from_name(key.as_ptr(), 1) };
    if value == 0 {
        Err("unknown XKB keysym".into())
    } else {
        Ok(value)
    }
}

pub(crate) fn builtin_portal_keysym(key: &str) -> Option<u32> {
    let keysym = match key {
        "BackSpace" => 0xff08,
        "Tab" => 0xff09,
        "Return" => 0xff0d,
        "Pause" => 0xff13,
        "Scroll_Lock" => 0xff14,
        "Escape" => 0xff1b,
        "Home" => 0xff50,
        "Left" => 0xff51,
        "Up" => 0xff52,
        "Right" => 0xff53,
        "Down" => 0xff54,
        "Page_Up" => 0xff55,
        "Page_Down" => 0xff56,
        "End" => 0xff57,
        "Print" => 0xff61,
        "Insert" => 0xff63,
        "Break" => 0xff6b,
        "Num_Lock" => 0xff7f,
        "KP_Space" => 0xff80,
        "KP_Enter" => 0xff8d,
        "KP_Home" => 0xff95,
        "KP_Left" => 0xff96,
        "KP_Up" => 0xff97,
        "KP_Right" => 0xff98,
        "KP_Down" => 0xff99,
        "KP_Page_Up" => 0xff9a,
        "KP_Page_Down" => 0xff9b,
        "KP_End" => 0xff9c,
        "KP_Begin" => 0xff9d,
        "KP_Insert" => 0xff9e,
        "KP_Delete" => 0xff9f,
        "KP_Multiply" => 0xffaa,
        "KP_Add" => 0xffab,
        "KP_Separator" => 0xffac,
        "KP_Subtract" => 0xffad,
        "KP_Decimal" => 0xffae,
        "KP_Divide" => 0xffaf,
        "KP_Equal" => 0xffbd,
        "minus" => 0x2d,
        "equal" => 0x3d,
        "bracketleft" => 0x5b,
        "bracketright" => 0x5d,
        "backslash" => 0x5c,
        "semicolon" => 0x3b,
        "apostrophe" => 0x27,
        "comma" => 0x2c,
        "period" => 0x2e,
        "slash" => 0x2f,
        "grave" => 0x60,
        "Shift_L" => 0xffe1,
        "Control_L" => 0xffe3,
        "Caps_Lock" => 0xffe5,
        "Alt_L" => 0xffe9,
        "Super_L" => 0xffeb,
        "Delete" => 0xffff,
        _ => {
            if let Some(number) = key
                .strip_prefix("KP_")
                .and_then(|number| number.parse::<u32>().ok())
                .filter(|number| *number <= 9)
            {
                return Some(0xffb0 + number);
            }
            if let Some(number) = key
                .strip_prefix('F')
                .and_then(|number| number.parse::<u32>().ok())
                .filter(|number| (1..=35).contains(number))
            {
                return Some(0xffbd + number);
            }
            if (key.len() == 1 && key.as_bytes()[0].is_ascii_graphic()) || key == "space" {
                return Some(if key == "space" {
                    0x20
                } else {
                    u32::from(key.as_bytes()[0])
                });
            }
            return None;
        }
    };
    Some(keysym)
}

pub(crate) fn xkb_available() -> bool {
    let Ok(library) = DynamicLibrary::open(c"libxkbcommon.so.0") else {
        return false;
    };
    let symbol: Result<unsafe extern "C" fn(*const c_char, c_int) -> u32, _> =
        library.symbol(c"xkb_keysym_from_name");
    symbol.is_ok()
        && library
            .symbol::<unsafe extern "C" fn(u32) -> u32>(c"xkb_utf32_to_keysym")
            .is_ok()
        && library
            .symbol::<unsafe extern "C" fn(u32, *mut c_char, usize) -> c_int>(
                c"xkb_keysym_get_name",
            )
            .is_ok()
}

pub(crate) fn wtype_available() -> bool {
    if !command_exists("wtype") {
        return false;
    }
    let Ok(mut child) = limited_command("2s", "wtype")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    drop(child.stdin.take());
    child.wait().is_ok_and(|status| status.success())
}

pub(crate) fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty()
        || key.len() > 32
        || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err("key must be a short alphanumeric keysym name".into());
    }
    Ok(())
}
