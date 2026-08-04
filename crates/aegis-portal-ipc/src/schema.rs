//! Portal-owned projection of Aegis IPC protocol version 24.
//!
//! Only compositor-owned portal resources belong here. The wire types are
//! implemented independently from the compositor's Rust model so an internal
//! Aegis refactor cannot become a Portal build dependency.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 24;
pub const LOCAL_PORTAL_SCOPE: &str = "aegis-portal";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionCapabilities {
    pub query: bool,
    pub control: bool,
    #[serde(default)]
    pub input: bool,
    pub session: bool,
    #[serde(default)]
    pub interaction_domain: bool,
}

impl ConnectionCapabilities {
    pub const QUERY: Self = Self {
        query: true,
        control: false,
        input: false,
        session: false,
        interaction_domain: false,
    };

    pub fn privileged(self) -> bool {
        self.control || self.input || self.session || self.interaction_domain
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LeaseRequest {
    pub ttl_ms: u64,
}

impl Default for LeaseRequest {
    fn default() -> Self {
        Self { ttl_ms: 900_000 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseGrant {
    pub id: u64,
    pub ttl_ms: u64,
    pub renewable: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Rect {
    #[must_use]
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self {
            origin: Point { x, y },
            size: Size { w, h },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ColorScheme {
    #[default]
    System,
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Contrast {
    #[default]
    Normal,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccentColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl AccentColor {
    #[must_use]
    pub fn normalized(self) -> (f64, f64, f64) {
        (
            f64::from(self.red) / 255.0,
            f64::from(self.green) / 255.0,
            f64::from(self.blue) / 255.0,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesktopPreferences {
    pub color_scheme: ColorScheme,
    pub accent_color: Option<AccentColor>,
    pub contrast: Contrast,
    pub reduced_motion: bool,
    pub font_name: String,
    pub monospace_font_name: String,
    pub text_scale: f64,
    pub icon_theme: String,
    pub cursor_theme: String,
    pub cursor_size: u32,
}

impl Default for DesktopPreferences {
    fn default() -> Self {
        Self {
            color_scheme: ColorScheme::System,
            accent_color: None,
            contrast: Contrast::Normal,
            reduced_motion: false,
            font_name: "Sans 10".into(),
            monospace_font_name: "Monospace 10".into(),
            text_scale: 1.0,
            icon_theme: "hicolor".into(),
            cursor_theme: "default".into(),
            cursor_size: 24,
        }
    }
}

/// Partial settings snapshot. Serde intentionally ignores the compositor's
/// touchpad, display, and idle fields because the Settings portal exposes
/// only desktop preferences.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SettingsSnapshot {
    pub preferences: DesktopPreferences,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum StreamPixelFormat {
    Bgra8,
    Rgba8,
    Dmabuf { drm_format: u32, modifier: u64 },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum StreamTarget {
    #[default]
    Output,
    Window {
        window: WindowId,
    },
}

impl StreamTarget {
    pub(crate) fn is_output(&self) -> bool {
        matches!(self, Self::Output)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WindowId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PickKind {
    Region,
    Pixel,
    Window,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PickResult {
    Region { rect: Rect },
    Pixel { point: Point, rgb: [u8; 3] },
    Window { id: WindowId },
    Output,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ConfirmPickResult {
    Confirmed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    SettingsChanged {
        revision: u64,
    },
    StreamFrame {
        stream_id: u64,
        sequence: u64,
        width: u32,
        height: u32,
        stride: u32,
        format: StreamPixelFormat,
        damage: Vec<Rect>,
        dropped: u64,
        byte_len: u64,
    },
    StreamEnded {
        stream_id: u64,
        reason: String,
    },
    #[serde(skip)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum Request {
    Hello {
        version: u32,
        caps: ConnectionCapabilities,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lease: Option<LeaseRequest>,
    },
    GetSettings,
    Subscribe,
    RenewLease {
        ttl_ms: u64,
    },
    CaptureOutput {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<Rect>,
    },
    StreamOutputStart {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_fps: Option<u32>,
        #[serde(default, skip_serializing_if = "StreamTarget::is_output")]
        target: StreamTarget,
    },
    StreamOutputStop {
        stream_id: u64,
    },
    PickTarget {
        kind: PickKind,
    },
    PickConfirm {
        title: String,
        body: String,
        accept_label: Option<String>,
    },
}

/// Partial response projection. Unknown fields inside known responses are
/// intentionally ignored, while unknown response variants fail closed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum Response {
    Hello {
        version: u32,
        caps: ConnectionCapabilities,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lease: Option<LeaseGrant>,
    },
    Settings {
        snapshot: SettingsSnapshot,
    },
    CaptureOutput {
        width: u32,
        height: u32,
        png_bytes: u64,
    },
    StreamOutputStarted {
        stream_id: u64,
        width: u32,
        height: u32,
        format: StreamPixelFormat,
    },
    StreamOutputStopped {
        stream_id: u64,
    },
    Picked {
        result: PickResult,
    },
    ConfirmPicked {
        result: ConfirmPickResult,
    },
    LeaseRenewed {
        lease: LeaseGrant,
    },
    Subscribed,
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_matches_the_v24_wire_shape() {
        let request = Request::Hello {
            version: PROTOCOL_VERSION,
            caps: ConnectionCapabilities::QUERY,
            scope: None,
            lease: None,
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "type": "Hello",
                "version": 24,
                "caps": {
                    "query": true,
                    "control": false,
                    "input": false,
                    "session": false,
                    "interaction_domain": false
                },
                "scope": null
            })
        );
    }

    #[test]
    fn portal_operations_match_v24_wire_fixtures() {
        let fixtures = [
            (
                Request::CaptureOutput { region: None },
                serde_json::json!({ "type": "CaptureOutput" }),
            ),
            (
                Request::CaptureOutput {
                    region: Some(Rect::new(1, 2, 3, 4)),
                },
                serde_json::json!({
                    "type": "CaptureOutput",
                    "region": {
                        "origin": { "x": 1, "y": 2 },
                        "size": { "w": 3, "h": 4 }
                    }
                }),
            ),
            (
                Request::PickTarget {
                    kind: PickKind::Pixel,
                },
                serde_json::json!({
                    "type": "PickTarget",
                    "kind": { "type": "Pixel" }
                }),
            ),
            (
                Request::PickConfirm {
                    title: "Capture".into(),
                    body: "Allow capture?".into(),
                    accept_label: None,
                },
                serde_json::json!({
                    "type": "PickConfirm",
                    "title": "Capture",
                    "body": "Allow capture?",
                    "accept_label": null
                }),
            ),
            (
                Request::StreamOutputStart {
                    max_fps: Some(30),
                    target: StreamTarget::Output,
                },
                serde_json::json!({
                    "type": "StreamOutputStart",
                    "max_fps": 30
                }),
            ),
            (
                Request::StreamOutputStop { stream_id: 7 },
                serde_json::json!({
                    "type": "StreamOutputStop",
                    "stream_id": 7
                }),
            ),
        ];
        for (request, fixture) in fixtures {
            assert_eq!(serde_json::to_value(request).unwrap(), fixture);
        }
    }

    #[test]
    fn hello_response_ignores_non_portal_v24_fields() {
        let response: Response = serde_json::from_value(serde_json::json!({
            "type": "Hello",
            "version": 24,
            "caps": {
                "query": true,
                "control": true,
                "input": false,
                "session": false,
                "interaction_domain": false
            },
            "scope": { "name": "aegis-portal", "ops": ["CaptureOutput"] },
            "lease": { "id": 9, "ttl_ms": 900000, "renewable": true },
            "session": { "id": "ignored" },
            "agent": null
        }))
        .unwrap();
        let Response::Hello {
            version,
            caps,
            lease,
        } = response
        else {
            panic!("hello response");
        };
        assert_eq!(version, PROTOCOL_VERSION);
        assert!(caps.control);
        assert_eq!(lease.unwrap().id, 9);
    }

    #[test]
    fn real_settings_response_ignores_non_portal_fields() {
        let response: Response = serde_json::from_value(serde_json::json!({
            "type": "Settings",
            "snapshot": {
                "revision": 9,
                "touchpad": { "available": false, "config": {} },
                "display": { "configurable": false, "outputs": [], "error": null },
                "preferences": {
                    "color_scheme": "dark",
                    "accent_color": { "red": 1, "green": 2, "blue": 3 },
                    "contrast": "normal",
                    "reduced_motion": false,
                    "font_name": "Sans 10",
                    "monospace_font_name": "Monospace 10",
                    "text_scale": 1.0,
                    "icon_theme": "hicolor",
                    "cursor_theme": "default",
                    "cursor_size": 24
                },
                "idle": {}
            }
        }))
        .unwrap();
        let Response::Settings { snapshot } = response else {
            panic!("settings response");
        };
        assert_eq!(snapshot.preferences.color_scheme, ColorScheme::Dark);
    }
}
