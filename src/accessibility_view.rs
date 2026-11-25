//! Accessibility tree views in Observations, and mapping AT-SPI bounds into Screenshot pixels.

use super::*;

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
