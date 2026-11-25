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
