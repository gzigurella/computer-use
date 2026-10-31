pub(super) fn sway_layout() -> Result<(i32, i32, u32, u32), String> {
    Err(unavailable())
}
pub(super) fn sway_active_window() -> Result<Option<serde_json::Value>, String> {
    Ok(None)
}
pub(super) fn sway_move(_: &super::Observation, _: u32, _: u32) -> Result<(), String> {
    Err(unavailable())
}
pub(super) fn sway_pointer_button(_: u32, _: bool) -> Result<(), String> {
    Err(unavailable())
}
pub(super) fn sway_scroll(_: u32, _: usize) -> Result<(), String> {
    Err(unavailable())
}

fn unavailable() -> String {
    "Sway backend was excluded from this build".into()
}
