pub(super) fn x11_test_available() -> bool {
    false
}
pub(super) fn x11_display_available() -> bool {
    false
}
pub(super) fn x11_pointer_position() -> Option<(i32, i32)> {
    None
}
pub(super) fn x11_screen_size() -> Option<(u32, u32)> {
    None
}
pub(super) fn x11_layout_text_available() -> bool {
    false
}
pub(super) fn x11_active_window() -> Result<Option<serde_json::Value>, String> {
    Ok(None)
}

fn unavailable() -> String {
    "X11 backend was excluded from this build".into()
}

pub(super) fn x11_move(_: i32, _: i32) -> Result<(), String> {
    Err(unavailable())
}
pub(super) fn x11_click_at(_: i32, _: i32, _: u32) -> Result<(), String> {
    Err(unavailable())
}
pub(super) fn x11_scroll_at(_: i32, _: i32, _: u32, _: usize) -> Result<(), String> {
    Err(unavailable())
}
pub(super) fn x11_drag_path(_: Vec<(u32, u32)>, _: u32, _: Vec<String>) -> Result<(), String> {
    Err(unavailable())
}
pub(super) fn x11_button_state(_: u32, _: bool) -> Result<(), String> {
    Err(unavailable())
}
pub(super) fn x11_key_state(_: &str, _: bool) -> Result<(), String> {
    Err(unavailable())
}
pub(super) fn x11_key(_: &str, _: &[String]) -> Result<(), String> {
    Err(unavailable())
}
pub(super) fn x11_type_ascii(_: &str) -> Result<(), String> {
    Err(unavailable())
}

pub(super) fn xdotool_type(_: &str) -> Result<(), String> {
    Err(unavailable())
}

pub(super) fn ffmpeg_x11grab_available() -> bool {
    false
}
pub(super) fn x11_image(_: u8, _: bool, _: Option<(u32, u32)>, _: u32) -> Result<Vec<u8>, String> {
    Err(unavailable())
}
