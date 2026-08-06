//! Daemon-level media portal tests. A fake compositor serves the real scoped
//! Aegis IPC protocol; requests cross D-Bus and capture bytes cross the
//! sealed-memfd transport before the backend persists/returns them.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aegis_portal_ipc::testing::{
    CaptureOutputPayload, Handler, Server, StreamFramePayload, StreamInfo,
};
use aegis_portal_ipc::{ConfirmPickResult, PickKind, PickResult, StreamPixelFormat, StreamTarget};
use zbus::blocking::Proxy;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

mod common;
use common::{KillOnDrop, daemon_command, private_bus, temp_dir, wait_for_name};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.aegis";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";

// Valid 1x1 transparent PNG. The backend must transport it byte-for-byte;
// PNG encoding itself remains compositor-owned.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[derive(Default)]
struct FakeCompositor {
    captures: Mutex<Vec<Option<aegis_portal_ipc::Rect>>>,
    picks: Mutex<Vec<PickKind>>,
    confirms: Mutex<Vec<(String, String)>>,
    stream_starts: Mutex<Vec<(Option<u32>, StreamTarget)>>,
    stream_stops: Mutex<Vec<u64>>,
    stream_disconnects: Mutex<Vec<u64>>,
}

impl Handler for FakeCompositor {
    fn capture_output(
        &self,
        region: Option<aegis_portal_ipc::Rect>,
    ) -> Result<CaptureOutputPayload, String> {
        self.captures.lock().unwrap().push(region);
        Ok(CaptureOutputPayload {
            width: 1,
            height: 1,
            png: PNG.to_vec(),
        })
    }

    fn pick_target(&self, _conn_id: u64, kind: PickKind) -> Result<PickResult, String> {
        self.picks.lock().unwrap().push(kind);
        Ok(match kind {
            PickKind::Region => PickResult::Region {
                rect: aegis_portal_ipc::Rect::new(10, 20, 30, 40),
            },
            PickKind::Pixel => PickResult::Pixel {
                point: aegis_portal_ipc::Point { x: 4, y: 8 },
                rgb: [255, 128, 0],
            },
            PickKind::Window => PickResult::Cancelled,
        })
    }

    fn pick_confirm(
        &self,
        _conn_id: u64,
        title: String,
        body: String,
        _accept_label: Option<String>,
    ) -> Result<ConfirmPickResult, String> {
        self.confirms.lock().unwrap().push((title, body));
        Ok(ConfirmPickResult::Confirmed)
    }

    fn stream_output_start(
        &self,
        _conn_id: u64,
        max_fps: Option<u32>,
        target: StreamTarget,
    ) -> Result<StreamInfo, String> {
        self.stream_starts.lock().unwrap().push((max_fps, target));
        Ok(StreamInfo {
            stream_id: 1,
            width: 2,
            height: 2,
            format: StreamPixelFormat::Bgra8,
        })
    }

    fn stream_output_stop(&self, stream_id: u64) {
        self.stream_stops.lock().unwrap().push(stream_id);
    }

    fn streams_disconnected(&self, conn_id: u64) {
        self.stream_disconnects.lock().unwrap().push(conn_id);
    }
}

fn handle(path: &str) -> ObjectPath<'_> {
    ObjectPath::try_from(path).expect("valid request path")
}

#[test]
fn screenshot_and_color_cross_real_daemon_and_scoped_ipc() {
    let Some(bus) = private_bus() else {
        eprintln!("media: no dbus-daemon, skipping");
        return;
    };
    let conn = bus.connect();
    let data_dir = temp_dir("media-data");
    let runtime_dir = temp_dir("media-runtime");
    let fake = Arc::new(FakeCompositor::default());
    let _server = Server::start(&runtime_dir.join("aegis.sock"), Arc::clone(&fake))
        .expect("bind fake compositor IPC");
    let backend_log = runtime_dir.join("backend.log");
    let mut backend = daemon_command(&bus, &data_dir, &runtime_dir);
    backend.env("RUST_LOG", "debug").stderr(Stdio::from(
        std::fs::File::create(&backend_log).expect("backend log"),
    ));
    let _daemon = KillOnDrop(backend.spawn().expect("spawn portal daemon"));
    wait_for_name(&conn, PORTAL);

    let screenshot = Proxy::new(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.Screenshot",
    )
    .expect("Screenshot proxy");
    let mut options = HashMap::new();
    options.insert("interactive".to_owned(), Value::from(true));
    options.insert("target".to_owned(), Value::from(4_u32));
    let (code, results): (u32, HashMap<String, OwnedValue>) = screenshot
        .call(
            "Screenshot",
            &(
                handle("/org/freedesktop/portal/desktop/request/1/shot1"),
                "dev.aegis.MediaTest",
                "",
                options,
            ),
        )
        .expect("Screenshot call");
    assert_eq!(code, 0, "interactive screenshot: {results:?}");
    let uri = String::try_from(results["uri"].clone()).expect("uri string");
    let path = uri.strip_prefix("file://").expect("local file URI");
    assert_eq!(std::fs::read(path).expect("read capture"), PNG);
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fake.captures.lock().unwrap().as_slice(),
        &[Some(aegis_portal_ipc::Rect::new(10, 20, 30, 40))]
    );

    let (code, results): (u32, HashMap<String, OwnedValue>) = screenshot
        .call(
            "PickColor",
            &(
                handle("/org/freedesktop/portal/desktop/request/1/color1"),
                "dev.aegis.MediaTest",
                "",
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("PickColor call");
    assert_eq!(code, 0, "PickColor: {results:?}");
    let color = Value::from(results["color"].clone());
    assert_eq!(color.value_signature().to_string(), "(ddd)");
    let Value::Structure(color) = color else {
        panic!("color must be a structure");
    };
    let channels: Vec<f64> = color
        .fields()
        .iter()
        .map(|channel| f64::try_from(channel).expect("double channel"))
        .collect();
    assert_eq!(channels[0], 1.0);
    assert!((channels[1] - 128.0 / 255.0).abs() < f64::EPSILON);
    assert_eq!(channels[2], 0.0);
    assert_eq!(
        fake.picks.lock().unwrap().as_slice(),
        &[PickKind::Region, PickKind::Pixel]
    );

    // Legacy, noninteractive capture is fail-closed unless the frontend says
    // PermissionStore already approved it: an explicit compositor consent
    // must occur before any pixels are requested.
    let (code, _): (u32, HashMap<String, OwnedValue>) = screenshot
        .call(
            "Screenshot",
            &(
                handle("/org/freedesktop/portal/desktop/request/1/shot2"),
                "dev.aegis.MediaTest",
                "",
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("legacy Screenshot call");
    assert_eq!(code, 0);
    assert_eq!(fake.confirms.lock().unwrap().len(), 1);
    assert_eq!(fake.captures.lock().unwrap().len(), 2);

    std::fs::remove_file(path).ok();
    std::fs::remove_dir_all(data_dir).ok();
    std::fs::remove_dir_all(runtime_dir).ok();
}

fn pipewire_e2e_required() -> bool {
    std::env::var_os("AEGIS_PORTAL_REQUIRE_PIPEWIRE_E2E").is_some() || common::e2e_required()
}

fn media_tool_available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn require_or_skip(condition: bool, message: &str) -> bool {
    if condition {
        return true;
    }
    assert!(!pipewire_e2e_required(), "{message}");
    eprintln!("screencast PipeWire E2E: {message}; skipping");
    false
}

fn stream_details(results: &HashMap<String, OwnedValue>) -> (u32, u64) {
    let streams = Value::from(results["streams"].clone());
    let Value::Array(streams) = streams else {
        panic!("streams result must be an array");
    };
    let Value::Structure(stream) = streams.get(0).expect("read stream").expect("one stream") else {
        panic!("stream entry must be a structure");
    };
    let node_id = u32::try_from(&stream.fields()[0]).expect("PipeWire node id");
    let Value::Dict(properties) = &stream.fields()[1] else {
        panic!("stream properties must be a dict");
    };
    let serial = properties
        .iter()
        .find_map(|(key, value)| {
            let Value::Str(key) = key else {
                return None;
            };
            if key.as_str() != "pipewire-serial" {
                return None;
            }
            let Value::Value(value) = value else {
                return None;
            };
            u64::try_from(value.as_ref()).ok()
        })
        .expect("v6 stream must include pipewire-serial");
    (node_id, serial)
}

#[test]
fn screencast_republishes_compositor_frames_through_real_pipewire() {
    if !require_or_skip(
        media_tool_available("pipewire"),
        "pipewire executable unavailable",
    ) || !require_or_skip(
        media_tool_available("gst-launch-1.0"),
        "GStreamer PipeWire consumer unavailable",
    ) || !require_or_skip(
        media_tool_available("wireplumber"),
        "WirePlumber session manager unavailable",
    ) {
        return;
    }
    let Some(bus) = private_bus() else {
        if pipewire_e2e_required() {
            panic!("dbus-daemon unavailable");
        }
        eprintln!("screencast PipeWire E2E: no dbus-daemon, skipping");
        return;
    };
    let conn = bus.connect();
    let data_dir = temp_dir("cast-data");
    let runtime_dir = temp_dir("cast-runtime");
    std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))
        .expect("secure PipeWire runtime directory");

    let pipewire_log = runtime_dir.join("pipewire.log");
    let mut pipewire = Command::new("pipewire");
    pipewire
        .env("DBUS_SESSION_BUS_ADDRESS", bus.address())
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("PIPEWIRE_RUNTIME_DIR", &runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            std::fs::File::create(&pipewire_log).expect("PipeWire log"),
        ));
    let mut pipewire = pipewire.spawn().expect("pipewire was probed above");
    let socket = runtime_dir.join("pipewire-0");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if socket.exists() {
            break;
        }
        if let Some(status) = pipewire.try_wait().expect("poll PipeWire") {
            let log = std::fs::read_to_string(&pipewire_log).unwrap_or_default();
            if !require_or_skip(
                false,
                &format!("isolated PipeWire exited as {status}: {log}"),
            ) {
                std::fs::remove_dir_all(data_dir).ok();
                std::fs::remove_dir_all(runtime_dir).ok();
                return;
            }
        }
        if Instant::now() >= deadline {
            let _ = pipewire.kill();
            let _ = pipewire.wait();
            let log = std::fs::read_to_string(&pipewire_log).unwrap_or_default();
            if !require_or_skip(false, &format!("isolated PipeWire did not start: {log}")) {
                std::fs::remove_dir_all(data_dir).ok();
                std::fs::remove_dir_all(runtime_dir).ok();
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _pipewire = KillOnDrop(pipewire);

    // Target-node linking is session-manager policy in PipeWire. Running the
    // same WirePlumber component production desktops use makes this a real
    // producer/consumer test rather than only a registry-object check.
    let wireplumber_log = runtime_dir.join("wireplumber.log");
    let mut wireplumber = Command::new("wireplumber");
    wireplumber
        .env("DBUS_SESSION_BUS_ADDRESS", bus.address())
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("PIPEWIRE_RUNTIME_DIR", &runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            std::fs::File::create(&wireplumber_log).expect("WirePlumber log"),
        ));
    let mut wireplumber = wireplumber.spawn().expect("WirePlumber was probed above");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = wireplumber.try_wait().expect("poll WirePlumber") {
            let log = std::fs::read_to_string(&wireplumber_log).unwrap_or_default();
            panic!("WirePlumber exited as {status}: {log}");
        }
        let registry = Command::new("pw-dump")
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("PIPEWIRE_RUNTIME_DIR", &runtime_dir)
            .output();
        if registry
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("WirePlumber"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "WirePlumber did not register: {}",
            std::fs::read_to_string(&wireplumber_log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let _wireplumber = KillOnDrop(wireplumber);

    let fake = Arc::new(FakeCompositor::default());
    let server = Server::start(&runtime_dir.join("aegis.sock"), Arc::clone(&fake))
        .expect("bind fake compositor IPC");
    let backend_log = runtime_dir.join("backend.log");
    let mut backend = daemon_command(&bus, &data_dir, &runtime_dir);
    backend.env("RUST_LOG", "debug").stderr(Stdio::from(
        std::fs::File::create(&backend_log).expect("backend log"),
    ));
    let _daemon = KillOnDrop(backend.spawn().expect("spawn portal daemon"));
    wait_for_name(&conn, PORTAL);

    let screencast = Proxy::new(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.ScreenCast",
    )
    .expect("ScreenCast proxy");
    let session_path = "/org/freedesktop/portal/desktop/session/1/cast1";
    let (code, _): (u32, HashMap<String, OwnedValue>) = screencast
        .call(
            "CreateSession",
            &(
                handle("/org/freedesktop/portal/desktop/request/1/create1"),
                handle(session_path),
                "",
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("CreateSession");
    assert_eq!(code, 0, "host applications have an empty backend app_id");

    let (code, _): (u32, HashMap<String, OwnedValue>) = screencast
        .call(
            "SelectSources",
            &(
                handle("/org/freedesktop/portal/desktop/request/1/select1"),
                handle(session_path),
                "",
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("SelectSources");
    assert_eq!(code, 0);

    // Regression: OBS's unified "Screen Capture (PipeWire)" source offers
    // monitor|window (0b11); the backend must accept the mask and serve its
    // monitor subset instead of rejecting the mixed offer.
    let mix_session_path = "/org/freedesktop/portal/desktop/session/1/cast_mix";
    let (code, _): (u32, HashMap<String, OwnedValue>) = screencast
        .call(
            "CreateSession",
            &(
                handle("/org/freedesktop/portal/desktop/request/1/create_mix"),
                handle(mix_session_path),
                "",
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("CreateSession (mixed types)");
    assert_eq!(code, 0);
    let (code, _): (u32, HashMap<String, OwnedValue>) = screencast
        .call(
            "SelectSources",
            &(
                handle("/org/freedesktop/portal/desktop/request/1/select_mix"),
                handle(mix_session_path),
                "",
                HashMap::from([("types".to_string(), Value::from(0b11_u32))]),
            ),
        )
        .expect("SelectSources (mixed types)");
    assert_eq!(code, 0, "monitor|window offer must be served as monitor");

    let (code, results): (u32, HashMap<String, OwnedValue>) = screencast
        .call(
            "Start",
            &(
                handle("/org/freedesktop/portal/desktop/request/1/start1"),
                handle(session_path),
                "",
                "",
                HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("Start");
    assert_eq!(code, 0, "ScreenCast Start: {results:?}");
    let (node_id, serial) = stream_details(&results);
    assert_ne!(node_id, u32::MAX, "PipeWire node id must be valid");
    assert_ne!(serial, 0, "v6 requires a stable PipeWire serial");
    assert_eq!(
        fake.stream_starts.lock().unwrap().as_slice(),
        &[(Some(30), StreamTarget::Output)]
    );

    // Consume one raw frame from the exact node through an independent
    // PipeWire client. Keeping the latest compositor frame lets the producer
    // satisfy the first process callback even when linking finishes later.
    let captured = runtime_dir.join("captured.bgrx");
    let consumer_log = runtime_dir.join("consumer.log");
    let mut consumer = Command::new("gst-launch-1.0");
    consumer
        .args([
            "-q",
            "pipewiresrc",
            &format!("target-object={serial}"),
            "num-buffers=1",
            "!",
            "video/x-raw,format=BGRx,width=2,height=2",
            "!",
            "filesink",
            &format!("location={}", captured.display()),
        ])
        .env("DBUS_SESSION_BUS_ADDRESS", bus.address())
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("PIPEWIRE_RUNTIME_DIR", &runtime_dir)
        .env("PIPEWIRE_REMOTE", "pipewire-0-manager")
        .env("GST_DEBUG", "pipewiresrc:6")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            std::fs::File::create(&consumer_log).expect("consumer log"),
        ));
    let mut consumer = consumer.spawn().expect("GStreamer was probed above");
    let pixels: Arc<[u8]> = Arc::from(&[7_u8; 16][..]);
    assert!(server.push_stream_frame(StreamFramePayload {
        stream_id: 1,
        sequence: 1,
        width: 2,
        height: 2,
        stride: 8,
        format: StreamPixelFormat::Bgra8,
        damage: vec![aegis_portal_ipc::Rect::new(0, 0, 2, 2)],
        dropped: 0,
        pixels,
    }));
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = consumer.try_wait().expect("poll consumer") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = consumer.kill();
            let _ = consumer.wait();
            let log = std::fs::read_to_string(&consumer_log).unwrap_or_default();
            let registry = Command::new("pw-dump")
                .env("XDG_RUNTIME_DIR", &runtime_dir)
                .env("PIPEWIRE_RUNTIME_DIR", &runtime_dir)
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                .unwrap_or_default();
            let backend = std::fs::read_to_string(&backend_log).unwrap_or_default();
            panic!(
                "PipeWire consumer timed out: {log}\nbackend:\n{backend}\nregistry:\n{registry}"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let log = std::fs::read_to_string(&consumer_log).unwrap_or_default();
    assert!(
        status.success(),
        "PipeWire consumer failed: {log}\nbackend:\n{}",
        std::fs::read_to_string(&backend_log).unwrap_or_default()
    );
    assert_eq!(
        std::fs::read(&captured).expect("captured raw frame"),
        [7; 16]
    );

    let session = Proxy::new(
        &conn,
        PORTAL,
        session_path,
        "org.freedesktop.impl.portal.Session",
    )
    .expect("Session proxy");
    let _: () = session.call("Close", &()).expect("Session.Close");
    let deadline = Instant::now() + Duration::from_secs(5);
    while fake.stream_disconnects.lock().unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "closing the portal session must disconnect the compositor stream"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    std::fs::remove_dir_all(data_dir).ok();
    std::fs::remove_dir_all(runtime_dir).ok();
}
