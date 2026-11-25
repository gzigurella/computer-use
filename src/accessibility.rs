use atspi::{
    CoordType, ObjectRefOwned, State,
    proxy::{
        accessible::AccessibleProxy, action::ActionProxy, component::ComponentProxy,
        editable_text::EditableTextProxy, text::TextProxy,
    },
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{runtime::Builder, time::timeout};
use zbus::{Connection, fdo::DBusProxy};

struct AccessibilityConnection(Connection);

impl AccessibilityConnection {
    async fn new() -> Result<Self, String> {
        let session = Connection::session().await.map_err(display)?;
        let bus = atspi::proxy::bus::BusProxy::new(&session)
            .await
            .map_err(display)?;
        let address: zbus::Address = bus
            .get_address()
            .await
            .map_err(display)?
            .parse()
            .map_err(display)?;
        let connection = zbus::connection::Builder::address(address)
            .map_err(display)?
            .build()
            .await
            .map_err(display)?;
        Ok(Self(connection))
    }

    fn connection(&self) -> &Connection {
        &self.0
    }

    async fn root_accessible_on_registry(&self) -> Result<AccessibleProxy<'_>, String> {
        AccessibleProxy::builder(self.connection())
            .destination("org.a11y.atspi.Registry")
            .map_err(display)?
            .path("/org/a11y/atspi/accessible/root")
            .map_err(display)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .map_err(display)
    }
}

const MAX_NODES: usize = 400;
pub(crate) const MAX_DEPTH: usize = 16;

pub(crate) fn request(request: Value) -> Result<Value, String> {
    let budget = match request["operation"].as_str() {
        Some("tree") => Duration::from_secs(12),
        Some("action" | "set_text") => Duration::from_secs(6),
        Some("focused_element") => Duration::from_secs(2),
        _ => Duration::from_secs(3),
    };
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("starting AT-SPI runtime: {error}"))?;
    runtime
        .block_on(timeout(budget, request_async(request)))
        .map_err(|_| "AT-SPI request timed out".to_owned())?
}

async fn request_async(request: Value) -> Result<Value, String> {
    let operation = request["operation"]
        .as_str()
        .ok_or("AT-SPI operation is missing")?;
    if operation == "enable" {
        let _atspi = AccessibilityConnection::new().await.map_err(display)?;
        enable_accessibility().await?;
        return Ok(json!({"available":true,"enabled":true}));
    }
    let atspi = AccessibilityConnection::new().await.map_err(display)?;
    let activation_error =
        if operation == "tree" && std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
            enable_accessibility().await.err()
        } else {
            None
        };
    let root = atspi.root_accessible_on_registry().await?;
    let count = root.get_children().await.map_err(display)?.len();
    let desktop_name = root.name().await.unwrap_or_default();
    match operation {
        "probe" => Ok(json!({"available":true,"root_name":desktop_name,"children":count})),
        "active_window" => {
            let active = find_active(&atspi, &root, "0", String::new(), 0, &mut 256).await;
            Ok(json!({"available":true,"active_window":active}))
        }
        "focused_element" => {
            let active_path = find_deepest_active_path(&atspi, &root, "0", 0, &mut 256).await;
            let (scan_root, path, depth) = if let Some(path) = active_path {
                if let Some(reference) = resolve_path(&atspi, &root, &path).await {
                    if let Ok(scan_root) = proxy(&atspi, &reference).await {
                        (scan_root, path.clone(), path.matches('.').count())
                    } else {
                        (root, "0".into(), 0)
                    }
                } else {
                    (root, "0".into(), 0)
                }
            } else {
                (root, "0".into(), 0)
            };
            let focused = find_focused(&atspi, &scan_root, &path, depth, None, &mut 1200)
                .await
                .map(|(_, value)| value);
            Ok(json!({"available":true,"focused_element":focused}))
        }
        "tree" => {
            let active_path = find_deepest_active_path(&atspi, &root, "0", 0, &mut 800).await;
            let mut nodes = Vec::new();
            let mut budget = MAX_NODES;
            let mut truncated = false;
            collect_tree(
                &atspi,
                &root,
                "0",
                0,
                MAX_DEPTH,
                &mut budget,
                &mut nodes,
                &active_path,
                &mut truncated,
            )
            .await;
            Ok(
                json!({"available":true,"nodes":nodes,"truncated":truncated,"activation_error":activation_error}),
            )
        }
        "action" | "set_text" => {
            let path = request["element_id"]
                .as_str()
                .ok_or("accessible element_id is missing")?;
            let node = resolve_path(&atspi, &root, path)
                .await
                .ok_or("accessible element is stale")?;
            validate_target(&atspi, &node, &request).await?;
            let performed = if operation == "action" {
                let action = ActionProxy::builder(atspi.connection())
                    .destination(node.name.as_str())
                    .map_err(display)?
                    .path(node.path.as_str())
                    .map_err(display)?
                    .build()
                    .await
                    .map_err(display)?;
                let actions = action_names(&action, 64).await;
                let requested = request["action_name"]
                    .as_str()
                    .ok_or("action_name is missing")?;
                let index = if let Some(index) = request["action_index"].as_u64() {
                    let index = i32::try_from(index).map_err(display)?;
                    let Some(current) = usize::try_from(index)
                        .ok()
                        .and_then(|index| actions.get(index))
                    else {
                        return Err(
                            "accessible action changed since observation; observe again".into()
                        );
                    };
                    if current.as_deref() != Some(requested) {
                        return Err(
                            "accessible action changed since observation; observe again".into()
                        );
                    }
                    index
                } else {
                    actions
                        .iter()
                        .position(|name| name.as_deref() == Some(requested))
                        .and_then(|index| i32::try_from(index).ok())
                        .ok_or("accessible action changed since observation; observe again")?
                };
                action.do_action(index).await.map_err(display)?
            } else {
                let editable = EditableTextProxy::builder(atspi.connection())
                    .destination(node.name.as_str())
                    .map_err(display)?
                    .path(node.path.as_str())
                    .map_err(display)?
                    .build()
                    .await
                    .map_err(display)?;
                editable
                    .set_text_contents(request["text"].as_str().ok_or("text is missing")?)
                    .await
                    .map_err(display)?
            };
            Ok(json!({"performed":performed}))
        }
        _ => Err(format!("unknown AT-SPI operation: {operation}")),
    }
}

async fn enable_accessibility() -> Result<(), String> {
    let session = Connection::session().await.map_err(display)?;
    let status = atspi::proxy::bus::StatusProxy::new(&session)
        .await
        .map_err(display)?;
    status.set_is_enabled(true).await.map_err(display)?;
    let _ = status.set_screen_reader_enabled(true).await;
    Ok(())
}

#[derive(Clone)]
struct NodeRef {
    name: String,
    path: String,
}

async fn child_refs(proxy: &AccessibleProxy<'_>, parent: &str) -> Vec<(NodeRef, String)> {
    child_refs_with_truncation(proxy, parent).await.0
}

async fn child_refs_with_truncation(
    proxy: &AccessibleProxy<'_>,
    parent: &str,
) -> (Vec<(NodeRef, String)>, bool) {
    let reported_count = match proxy.inner().call::<_, _, i32>("GetChildCount", &()).await {
        Ok(count) => count,
        Err(_) => proxy.child_count().await.unwrap_or(0),
    };
    let count = reported_count.clamp(0, 100);
    let mut truncated = reported_count > 100;
    let mut children = Vec::new();
    for index in 0..count {
        let Ok(reference) = proxy.get_child_at_index(index).await else {
            truncated = true;
            break;
        };
        if reference.is_null() {
            truncated = true;
            break;
        }
        let Some(name) = reference.name_as_str() else {
            truncated = true;
            break;
        };
        children.push((
            NodeRef {
                name: name.to_owned(),
                path: reference.path_as_str().to_owned(),
            },
            format!("{parent}.{index}"),
        ));
    }
    if !children.is_empty() || count > 0 {
        return (children, truncated);
    }
    let Ok(references) = proxy.get_children().await else {
        return (Vec::new(), true);
    };
    truncated |= references.len() > 100;
    let mut children = Vec::new();
    for (index, reference) in references.into_iter().take(100).enumerate() {
        if reference.is_null() {
            truncated = true;
            continue;
        }
        let Some(name) = reference.name_as_str() else {
            truncated = true;
            continue;
        };
        children.push((
            NodeRef {
                name: name.to_owned(),
                path: reference.path_as_str().to_owned(),
            },
            format!("{parent}.{index}"),
        ));
    }
    (children, truncated)
}

async fn proxy<'a>(
    connection: &'a AccessibilityConnection,
    reference: &NodeRef,
) -> Result<AccessibleProxy<'a>, String> {
    let destination = zbus::names::BusName::try_from(reference.name.clone()).map_err(display)?;
    let path =
        zbus::zvariant::OwnedObjectPath::try_from(reference.path.clone()).map_err(display)?;
    AccessibleProxy::builder(connection.connection())
        .destination(destination)
        .map_err(display)?
        .path(path)
        .map_err(display)?
        .build()
        .await
        .map_err(display)
}

async fn resolve_path(
    connection: &AccessibilityConnection,
    _root: &AccessibleProxy<'_>,
    element_id: &str,
) -> Option<NodeRef> {
    let mut indexes = element_id.split('.');
    if indexes.next()? != "0" {
        return None;
    }
    let mut current = NodeRef {
        name: "org.a11y.atspi.Registry".into(),
        path: "/org/a11y/atspi/accessible/root".into(),
    };
    for part in indexes {
        let index: i32 = part.parse().ok()?;
        if !(0..100).contains(&index) {
            return None;
        }
        let current_proxy = proxy(connection, &current).await.ok()?;
        let child = current_proxy.get_child_at_index(index).await.ok()?;
        if child.is_null() {
            return None;
        }
        current = NodeRef {
            name: child.name_as_str()?.into(),
            path: child.path_as_str().into(),
        };
    }
    Some(current)
}

async fn state(proxy: &AccessibleProxy<'_>) -> Option<atspi::StateSet> {
    proxy.get_state().await.ok()
}
async fn text_property(proxy: &AccessibleProxy<'_>, property: &str) -> String {
    match property {
        "name" => proxy.name().await,
        "role" => proxy.get_role_name().await,
        _ => proxy.description().await,
    }
    .unwrap_or_default()
}

async fn process_id(connection: &AccessibilityConnection, name: &str) -> Option<u32> {
    let dbus = DBusProxy::new(connection.connection()).await.ok()?;
    let name = zbus::names::BusName::try_from(name).ok()?;
    dbus.get_connection_unix_process_id(name).await.ok()
}

async fn bounds(
    connection: &AccessibilityConnection,
    node: &NodeRef,
    coord: CoordType,
) -> Option<(i32, i32, i32, i32)> {
    ComponentProxy::builder(connection.connection())
        .destination(node.name.as_str())
        .ok()?
        .path(node.path.as_str())
        .ok()?
        .build()
        .await
        .ok()?
        .get_extents(coord)
        .await
        .ok()
}

async fn node_identity(
    connection: &AccessibilityConnection,
    node: &NodeRef,
    proxy: &AccessibleProxy<'_>,
) -> (Option<u32>, Option<String>) {
    let pid = process_id(connection, &node.name)
        .await
        .filter(|pid| *pid > 0);
    let accessible_id = proxy
        .accessible_id()
        .await
        .ok()
        .filter(|id| !id.is_empty() && id.len() <= 512);
    (pid, accessible_id)
}

async fn find_active(
    connection: &AccessibilityConnection,
    node: &AccessibleProxy<'_>,
    path: &str,
    mut app: String,
    depth: usize,
    budget: &mut usize,
) -> Option<Value> {
    if *budget == 0 {
        return None;
    }
    *budget -= 1;
    let role = text_property(node, "role").await;
    let name = text_property(node, "name").await;
    if role.eq_ignore_ascii_case("application") {
        app = name.clone()
    }
    let states = state(node).await?;
    if states.contains(State::Active)
        && ["frame", "window", "dialog"].contains(&role.to_lowercase().as_str())
        && states.contains(State::Visible)
        && states.contains(State::Showing)
    {
        let reference = ObjectRefOwned::try_from(node).ok()?;
        let node_ref = NodeRef {
            name: reference.name_as_str()?.into(),
            path: reference.path_as_str().into(),
        };
        let (pid, accessible_id) = node_identity(connection, &node_ref, node).await;
        return Some(
            json!({"application":app,"name":name,"role":role,"process_id":pid,"accessible_id":accessible_id,"element_id":path}),
        );
    }
    if depth >= 8 {
        return None;
    }
    let children = child_refs(node, path).await;
    let mut ranked = Vec::new();
    for (reference, child_path) in children {
        if let Ok(child) = proxy(connection, &reference).await {
            let active = state(&child)
                .await
                .is_some_and(|states| states.contains(State::Active));
            ranked.push((!active, reference, child_path));
        }
    }
    ranked.sort_by_key(|(inactive, _, _)| *inactive);
    for (_, reference, child_path) in ranked {
        if let Ok(child) = proxy(connection, &reference).await
            && let Some(found) = Box::pin(find_active(
                connection,
                &child,
                &child_path,
                app.clone(),
                depth + 1,
                budget,
            ))
            .await
        {
            return Some(found);
        }
    }
    None
}

async fn find_deepest_active_path(
    connection: &AccessibilityConnection,
    node: &AccessibleProxy<'_>,
    path: &str,
    depth: usize,
    budget: &mut usize,
) -> Option<String> {
    if *budget == 0 || depth > 8 {
        return None;
    }
    *budget -= 1;
    let role = text_property(node, "role").await.to_lowercase();
    let states = state(node).await?;
    let mut best = if ["frame", "window", "dialog"].contains(&role.as_str())
        && states.contains(State::Active)
        && states.contains(State::Visible)
        && states.contains(State::Showing)
    {
        Some(path.to_owned())
    } else {
        None
    };
    let children = child_refs(node, path).await;
    let mut ranked = Vec::new();
    for (reference, child_path) in children {
        if let Ok(child) = proxy(connection, &reference).await {
            let active = state(&child)
                .await
                .is_some_and(|s| s.contains(State::Active));
            ranked.push((!active, reference, child_path));
        }
    }
    ranked.sort_by_key(|(inactive, _, _)| *inactive);
    for (_, reference, child_path) in ranked {
        if let Ok(child) = proxy(connection, &reference).await
            && let Some(candidate) = Box::pin(find_deepest_active_path(
                connection,
                &child,
                &child_path,
                depth + 1,
                budget,
            ))
            .await
        {
            best = Some(candidate);
        }
    }
    best
}

async fn find_focused(
    connection: &AccessibilityConnection,
    node: &AccessibleProxy<'_>,
    path: &str,
    depth: usize,
    fallback_pid: Option<u32>,
    budget: &mut usize,
) -> Option<(usize, Value)> {
    if *budget == 0 || depth > MAX_DEPTH {
        return None;
    }
    *budget -= 1;
    let states = state(node).await?;
    let role = text_property(node, "role").await;
    let name = text_property(node, "name").await;
    let reference = ObjectRefOwned::try_from(node).ok()?;
    let node_ref = NodeRef {
        name: reference.name_as_str()?.into(),
        path: reference.path_as_str().into(),
    };
    let (pid, accessible_id) = node_identity(connection, &node_ref, node).await;
    let mut best = None;
    if states.contains(State::Focused)
        && states.contains(State::Visible)
        && states.contains(State::Showing)
    {
        let mut rect = bounds(connection, &node_ref, CoordType::Screen).await;
        let mut coordinate_space = "screen";
        if rect.is_some_and(|(x, y, _, _)| x == 0 && y == 0)
            && let Some(window_rect) = bounds(connection, &node_ref, CoordType::Window)
                .await
                .filter(|(x, y, _, _)| *x != 0 || *y != 0)
        {
            rect = Some(window_rect);
            coordinate_space = "window";
        }
        if let Some((x, y, width, height)) = rect.filter(|(_, _, w, h)| *w > 0 && *h > 0) {
            let container = ["desktop", "application", "frame", "window", "dialog"]
                .contains(&role.to_lowercase().as_str());
            best = Some((
                depth * 2 + usize::from(!container),
                json!({"element_id":path,"name":name.chars().take(512).collect::<String>(),"role":role.chars().take(128).collect::<String>(),"process_id":pid.or(fallback_pid),"accessible_id":accessible_id,"bounds":{"x":x,"y":y,"width":width,"height":height,"coordinate_space":coordinate_space}}),
            ));
        }
    }
    let children = child_refs(node, path).await;
    for (reference, child_path) in children {
        if let Ok(child) = proxy(connection, &reference).await
            && let Some(candidate) = Box::pin(find_focused(
                connection,
                &child,
                &child_path,
                depth + 1,
                fallback_pid.or(pid),
                budget,
            ))
            .await
            && best.as_ref().is_none_or(|current| candidate.0 > current.0)
        {
            best = Some(candidate);
        }
    }
    best
}

#[expect(
    clippy::too_many_arguments,
    reason = "the recursive walk threads its shared budget and output through each call"
)]
async fn collect_tree(
    connection: &AccessibilityConnection,
    node: &AccessibleProxy<'_>,
    path: &str,
    depth: usize,
    max_depth: usize,
    budget: &mut usize,
    nodes: &mut Vec<Value>,
    active_path: &Option<String>,
    truncated: &mut bool,
) {
    if *budget == 0 {
        *truncated = true;
        return;
    }
    if depth > max_depth {
        *truncated = true;
        return;
    }
    *budget -= 1;
    let name = text_property(node, "name").await;
    let role = text_property(node, "role").await;
    let description = text_property(node, "description").await;
    let states = state(node).await;
    let labels = [
        ("enabled", State::Enabled),
        ("sensitive", State::Sensitive),
        ("visible", State::Visible),
        ("showing", State::Showing),
        ("focused", State::Focused),
        ("focusable", State::Focusable),
        ("active", State::Active),
        ("checked", State::Checked),
        ("selected", State::Selected),
        ("expanded", State::Expanded),
        ("editable", State::Editable),
    ];
    let state_names = states
        .map(|states| {
            labels
                .into_iter()
                .filter_map(|(label, state)| states.contains(state).then_some(label))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let reference = ObjectRefOwned::try_from(node).ok();
    let node_ref = reference.as_ref().and_then(|reference| {
        Some(NodeRef {
            name: reference.name_as_str()?.into(),
            path: reference.path_as_str().into(),
        })
    });
    let (pid, accessible_id) = if let Some(node_ref) = node_ref.as_ref() {
        node_identity(connection, node_ref, node).await
    } else {
        (None, None)
    };
    let mut node_bounds = None;
    if state_names.contains(&"visible")
        && state_names.contains(&"showing")
        && let Some(node_ref) = node_ref.as_ref()
    {
        node_bounds=bounds(connection,node_ref,CoordType::Screen).await.map(|(x,y,width,height)|json!({"x":x,"y":y,"width":width,"height":height,"coordinate_space":"screen"}));
        if node_bounds
            .as_ref()
            .is_some_and(|rect| rect["x"] == 0 && rect["y"] == 0)
            && let Some((x, y, width, height)) = bounds(connection, node_ref, CoordType::Window)
                .await
                .filter(|(x, y, _, _)| *x != 0 || *y != 0)
        {
            node_bounds = Some(
                json!({"x":x,"y":y,"width":width,"height":height,"coordinate_space":"window"}),
            );
        }
    }
    let (mut children, children_truncated) = child_refs_with_truncation(node, path).await;
    let child_count = children.len();
    *truncated |= children_truncated;
    let mut actions = Vec::new();
    if let Some(node_ref) = node_ref.as_ref()
        && let Ok(builder) =
            ActionProxy::builder(connection.connection()).destination(node_ref.name.as_str())
        && let Ok(builder) = builder.path(node_ref.path.as_str())
        && let Ok(action) = builder.build().await
    {
        actions = action_names(&action, 16).await;
    }
    let mut text = None;
    let mut text_truncated = false;
    if node_bounds.is_some()
        && child_count == 0
        && !state_names.contains(&"editable")
        && !role.to_lowercase().contains("password")
        && !role.to_lowercase().contains("document")
        && let Some(node_ref) = node_ref.as_ref()
        && let Ok(builder) =
            TextProxy::builder(connection.connection()).destination(node_ref.name.as_str())
        && let Ok(builder) = builder.path(node_ref.path.as_str())
        && let Ok(text_proxy) = builder.build().await
        && let Ok(count) = text_proxy.character_count().await
        && count > 0
        && let Ok(value) = text_proxy.get_text(0, count.min(512)).await
    {
        text_truncated = count > 512;
        text = Some(value);
    }
    nodes.push(json!({"element_id":path,"name":name.chars().take(512).collect::<String>(),"role":role.chars().take(128).collect::<String>(),"process_id":pid,"accessible_id":accessible_id,"description":description.chars().take(512).collect::<String>(),"states":state_names,"bounds":node_bounds,"text":text,"text_truncated":text_truncated,"actions":actions}));
    if depth >= max_depth {
        if !children.is_empty() {
            *truncated = true;
        }
        return;
    }
    if *budget == 0 {
        if !children.is_empty() {
            *truncated = true;
        }
        return;
    }
    if let Some(active_path) = active_path {
        children.sort_by_key(|(_, child_path)| {
            !(active_path == child_path || active_path.starts_with(&format!("{child_path}.")))
        });
    }
    for (reference, child_path) in children {
        if *budget == 0 {
            *truncated = true;
            break;
        }
        if let Ok(child) = proxy(connection, &reference).await {
            Box::pin(collect_tree(
                connection,
                &child,
                &child_path,
                depth + 1,
                max_depth,
                budget,
                nodes,
                active_path,
                truncated,
            ))
            .await;
        }
    }
}

async fn action_names(action: &ActionProxy<'_>, limit: i32) -> Vec<Option<String>> {
    let count = action.n_actions().await.unwrap_or(0).clamp(0, limit);
    let mut names = Vec::with_capacity(count as usize);
    for index in 0..count {
        names.push(
            action
                .get_localized_name(index)
                .await
                .ok()
                .map(|name| name.chars().take(128).collect::<String>()),
        );
    }
    if count > 0 && names.iter().any(Option::is_some) {
        return names;
    }
    action
        .get_actions()
        .await
        .unwrap_or_default()
        .into_iter()
        .take(limit as usize)
        .map(|action| Some(action.name.chars().take(128).collect::<String>()))
        .collect()
}

async fn validate_target(
    connection: &AccessibilityConnection,
    node: &NodeRef,
    request: &Value,
) -> Result<(), String> {
    let accessible = proxy(connection, node).await?;
    let operation = request["operation"].as_str().unwrap_or_default();
    let expected_role = request["expected_role"].as_str().unwrap_or_default();
    if text_property(&accessible, "name").await
        != request["expected_name"].as_str().unwrap_or_default()
        || text_property(&accessible, "role").await != expected_role
    {
        return Err("accessible element changed since observation; observe again".into());
    }
    let (pid, id) = node_identity(connection, node, &accessible).await;
    if request["expected_process_id"]
        .as_u64()
        .is_some_and(|expected| Some(expected as u32) != pid)
        || request["expected_accessible_id"]
            .as_str()
            .is_some_and(|expected| Some(expected.to_owned()) != id)
    {
        return Err("accessible element identity changed since observation; observe again".into());
    }
    if let Some(expected) = request["expected_bounds"].as_object() {
        let coord = if expected.get("coordinate_space").and_then(Value::as_str) == Some("window") {
            CoordType::Window
        } else {
            CoordType::Screen
        };
        let actual = bounds(connection, node, coord).await;
        if actual
            != Some((
                expected
                    .get("x")
                    .and_then(Value::as_i64)
                    .unwrap_or(i64::MIN) as i32,
                expected
                    .get("y")
                    .and_then(Value::as_i64)
                    .unwrap_or(i64::MIN) as i32,
                expected
                    .get("width")
                    .and_then(Value::as_i64)
                    .unwrap_or(i64::MIN) as i32,
                expected
                    .get("height")
                    .and_then(Value::as_i64)
                    .unwrap_or(i64::MIN) as i32,
            ))
        {
            return Err("accessible element moved since observation; observe again".into());
        }
    }
    let states = state(&accessible)
        .await
        .ok_or("accessible element state is unavailable")?;
    if !states.contains(State::Visible) || !states.contains(State::Showing) {
        return Err("accessible element is no longer visible; observe again".into());
    }
    if !states.contains(State::Sensitive) {
        return Err("accessible element is no longer sensitive; observe again".into());
    }
    if operation == "set_text" && !states.contains(State::Editable) {
        return Err("accessible element is no longer visible and editable".into());
    }
    Ok(())
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}
