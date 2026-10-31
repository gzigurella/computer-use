use serde_json::Value;
use std::fmt;

#[derive(Debug)]
pub enum PortalStartError {
    Failed(String),
}

impl PortalStartError {
    pub fn allows_compositor_fallback(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

impl fmt::Display for PortalStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed(message) => formatter.write_str(message),
        }
    }
}

pub struct PortalSession {
    pub logical_width: u32,
    pub logical_height: u32,
    pub origin_x: i32,
    pub origin_y: i32,
    pub position_available: bool,
    pub synthetic_layout: bool,
    pub source: Option<String>,
    pub remote_desktop_active: bool,
    pub remote_pointer: bool,
    pub remote_keyboard: bool,
    pub eis_error: Option<String>,
}

impl PortalSession {
    pub fn capture_with_cursor(
        &self,
        _: u8,
        _: u32,
    ) -> Result<(Vec<u8>, Option<(i32, i32)>), String> {
        Err(unavailable())
    }
    pub fn cursor_position_available(&self) -> bool {
        false
    }
    pub async fn pointer(&self, _: f64, _: f64) -> Result<(), String> {
        Err(unavailable())
    }
    pub async fn pointer_for_click(&self, _: f64, _: f64) -> Result<(), String> {
        Err(unavailable())
    }
    pub async fn pointer_for_scroll(&self, _: f64, _: f64) -> Result<(), String> {
        Err(unavailable())
    }
    pub async fn button(&self, _: u32, _: bool) -> Result<(), String> {
        Err(unavailable())
    }
    pub async fn key(&self, _: u32, _: bool) -> Result<(), String> {
        Err(unavailable())
    }
    pub async fn scroll(&self, _: &str, _: i32) -> Result<(), String> {
        Err(unavailable())
    }
    pub fn smooth_scroll_available(&self) -> bool {
        false
    }
    pub async fn scroll_smooth(&self, _: f64, _: f64) -> Result<(), String> {
        Err(unavailable())
    }
    pub async fn close(&self) -> Result<(), String> {
        Ok(())
    }
    pub fn pointer_available(&self) -> bool {
        false
    }
    pub fn keyboard_available(&self) -> bool {
        false
    }
    pub fn scroll_available(&self) -> bool {
        false
    }
    pub fn input_regions(&self, _: u32, _: u32) -> Vec<Value> {
        Vec::new()
    }
    pub fn coordinates_available(&self) -> bool {
        false
    }
    pub fn monitor_geometry_available(&self) -> bool {
        false
    }
    pub fn is_closed(&self) -> bool {
        true
    }
    pub fn unicode_text_available(&self) -> bool {
        false
    }
    pub fn type_text(&self, _: &str) -> Result<(), String> {
        Err(unavailable())
    }
}

pub async fn await_request_cleanups() {}
pub fn libei_available() -> bool {
    false
}

fn unavailable() -> String {
    "the portal backend was excluded from this build".into()
}
