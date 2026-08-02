//! End-to-end FileChooser exercise using the real daemon on a private bus and
//! a pipe-compatible fake prompter. No compositor or display participates.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use aegis_portal_prompter::{
    BytePath, Choice, FileFilter, FilterRule, FilterRuleKind, PrompterRequest, PrompterResponse,
    SelectionMode, SelectionRequest, SelectionResponse,
};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

mod common;
use common::{KillOnDrop, daemon_command, private_bus, temp_dir, wait_for_name};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.aegis";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.impl.portal.FileChooser";

fn chooser(conn: &Connection) -> Proxy<'_> {
    Proxy::new(conn, PORTAL, DESKTOP_PATH, IFACE).expect("file chooser proxy")
}

fn call_chooser(
    proxy: &Proxy<'_>,
    method: &str,
    parent: &str,
    title: &str,
    options: HashMap<String, Value<'_>>,
    serial: u32,
) -> (u32, HashMap<String, OwnedValue>) {
    let handle = ObjectPath::try_from(format!(
        "/org/freedesktop/portal/desktop/request/1/fc{serial}"
    ))
    .expect("request handle path");
    proxy
        .call(method, &(handle, "dev.aegis.smoke", parent, title, options))
        .unwrap_or_else(|error| panic!("{method} must succeed at the bus level: {error}"))
}

fn result_uris(results: &HashMap<String, OwnedValue>) -> Vec<String> {
    let uris = results
        .get("uris")
        .unwrap_or_else(|| panic!("results must contain uris: {results:?}"));
    Vec::<String>::try_from(Value::from(uris.clone())).expect("uris is a string array")
}

fn write_response(directory: &Path, index: u32, response: &SelectionResponse) {
    std::fs::write(
        directory.join(format!("response-{index}.json")),
        serde_json::to_vec(&PrompterResponse::new(response.clone())).unwrap(),
    )
    .unwrap();
}

fn read_request(directory: &Path, index: u32) -> SelectionRequest {
    let request: PrompterRequest = serde_json::from_slice(
        &std::fs::read(directory.join(format!("request-{index}.json"))).unwrap(),
    )
    .unwrap();
    request.into_selection().unwrap()
}

fn fake_prompter(directory: &Path) -> std::path::PathBuf {
    let path = directory.join("fake-prompter");
    std::fs::write(
        &path,
        r#"#!/bin/sh
set -eu
fixture="${AEGIS_PROMPTER_FIXTURE:?}"
count_file="$fixture/count"
while ! mkdir "$fixture/count-lock" 2>/dev/null; do :; done
if test -f "$count_file"; then
    index=$(cat "$count_file")
else
    index=0
fi
index=$((index + 1))
printf '%s\n' "$index" > "$count_file"
rmdir "$fixture/count-lock"
cat > "$fixture/request-$index.json"
if test -f "$fixture/response-$index.json"; then
    cat "$fixture/response-$index.json"
else
    exec sleep 30
fi
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

#[test]
fn file_chooser_end_to_end() {
    let Some(bus) = private_bus() else {
        eprintln!("file_chooser_end_to_end: no dbus-daemon, skipping");
        return;
    };
    let conn = bus.connect();
    let data_dir = temp_dir("data");
    let runtime_dir = temp_dir("runtime");
    let fixture_dir = temp_dir("prompter");
    let prompter = fake_prompter(&fixture_dir);

    let image_filter = FileFilter {
        label: "Images".into(),
        rules: vec![
            FilterRule {
                kind: FilterRuleKind::Glob,
                value: "image/*".into(),
            },
            FilterRule {
                kind: FilterRuleKind::Mime,
                value: "image/png".into(),
            },
        ],
    };
    write_response(
        &fixture_dir,
        1,
        &SelectionResponse::Selected {
            paths: vec![BytePath::from_path("/tmp/fake-chosen.png")],
            current_filter: Some(image_filter.clone()),
            choices: vec![("encoding".into(), "utf8".into())],
        },
    );
    write_response(
        &fixture_dir,
        2,
        &SelectionResponse::Selected {
            paths: vec![BytePath::from_path("/tmp/fake-save.png")],
            current_filter: None,
            choices: Vec::new(),
        },
    );
    write_response(
        &fixture_dir,
        3,
        &SelectionResponse::Selected {
            // SaveFiles collision/name processing belongs to the prompter;
            // the backend receives final paths only.
            paths: vec![
                BytePath::from_path("/chosen/dir/one.txt"),
                BytePath::from_path("/chosen/dir/two.txt"),
            ],
            current_filter: None,
            choices: Vec::new(),
        },
    );
    write_response(&fixture_dir, 4, &SelectionResponse::Cancelled);
    write_response(
        &fixture_dir,
        6,
        &SelectionResponse::Selected {
            paths: vec![BytePath::from_path("/tmp/concurrent.txt")],
            current_filter: None,
            choices: Vec::new(),
        },
    );

    let mut command = daemon_command(&bus, &data_dir, &runtime_dir);
    command
        .env("AEGIS_PORTAL_PROMPTER", &prompter)
        .env("AEGIS_PROMPTER_FIXTURE", &fixture_dir);
    let _daemon = KillOnDrop(command.spawn().expect("spawn portal daemon"));

    wait_for_name(&conn, PORTAL);
    let chooser_proxy = chooser(&conn);
    let version: u32 = chooser_proxy
        .get_property("version")
        .expect("version property");
    assert_eq!(version, 3);

    // OpenFile exercises every v3 option whose old compositor picker lost.
    let filters = vec![(
        "Images".to_owned(),
        vec![(0u32, "image/*".to_owned()), (1u32, "image/png".to_owned())],
    )];
    let current_filter = filters[0].clone();
    let choices = vec![(
        "encoding".to_owned(),
        "Encoding".to_owned(),
        vec![("utf8".to_owned(), "UTF-8".to_owned())],
        "utf8".to_owned(),
    )];
    let mut options = HashMap::new();
    options.insert("modal".to_owned(), Value::from(false));
    options.insert("multiple".to_owned(), Value::from(true));
    options.insert("accept_label".to_owned(), Value::from("Import"));
    options.insert("filters".to_owned(), Value::from(filters));
    options.insert("current_filter".to_owned(), Value::from(current_filter));
    options.insert("choices".to_owned(), Value::from(choices));
    options.insert(
        "current_folder".to_owned(),
        Value::from(b"/tmp/images\0".to_vec()),
    );
    let (response, results) = call_chooser(
        &chooser_proxy,
        "OpenFile",
        "wayland:parent-token",
        "Import image",
        options,
        1,
    );
    assert_eq!(response, 0, "OpenFile must succeed: {results:?}");
    assert_eq!(result_uris(&results), ["file:///tmp/fake-chosen.png"]);
    assert!(results.contains_key("current_filter"));
    assert!(results.contains_key("choices"));
    let request = read_request(&fixture_dir, 1);
    assert_eq!(request.mode, SelectionMode::OpenFile);
    assert_eq!(
        request.parent_window.as_deref(),
        Some("wayland:parent-token")
    );
    assert!(!request.modal);
    assert!(request.multiple);
    assert_eq!(request.accept_label.as_deref(), Some("Import"));
    assert_eq!(request.current_filter, Some(image_filter));
    assert_eq!(
        request.choices,
        [Choice {
            id: "encoding".into(),
            label: "Encoding".into(),
            options: vec![("utf8".into(), "UTF-8".into())],
            selected: "utf8".into(),
        }]
    );

    // SaveFile preserves current_file as one semantic value instead of
    // reconstructing it from lossy UTF-8 folder/name pieces.
    let mut options = HashMap::new();
    options.insert(
        "current_file".to_owned(),
        Value::from(b"/tmp/existing.png\0".to_vec()),
    );
    let (response, results) = call_chooser(&chooser_proxy, "SaveFile", "", "Save it", options, 2);
    assert_eq!(response, 0, "SaveFile must succeed: {results:?}");
    assert_eq!(result_uris(&results), ["file:///tmp/fake-save.png"]);
    let request = read_request(&fixture_dir, 2);
    assert_eq!(request.mode, SelectionMode::SaveFile);
    assert_eq!(
        request.current_file.unwrap().to_path_buf(),
        Path::new("/tmp/existing.png")
    );

    let mut options = HashMap::new();
    options.insert(
        "files".to_owned(),
        Value::from(vec![b"one.txt\0".to_vec(), b"two.txt\0".to_vec()]),
    );
    let (response, results) =
        call_chooser(&chooser_proxy, "SaveFiles", "", "Save them", options, 3);
    assert_eq!(response, 0, "SaveFiles must succeed: {results:?}");
    assert_eq!(
        result_uris(&results),
        ["file:///chosen/dir/one.txt", "file:///chosen/dir/two.txt"]
    );
    let request = read_request(&fixture_dir, 3);
    assert_eq!(request.mode, SelectionMode::SaveFiles);
    assert_eq!(
        request
            .files
            .iter()
            .map(BytePath::to_path_buf)
            .collect::<Vec<_>>(),
        [Path::new("one.txt"), Path::new("two.txt")]
    );

    let (response, _) = call_chooser(
        &chooser_proxy,
        "OpenFile",
        "",
        "Cancel it",
        HashMap::new(),
        4,
    );
    assert_eq!(response, 1);

    // Request.Close is an active cancellation boundary, and a prompter that
    // stays alive must not serialize unrelated clients behind it.
    let bus_address = bus.address().to_owned();
    let started = std::time::Instant::now();
    let pending = std::thread::spawn(move || {
        let connection = zbus::blocking::connection::Builder::address(bus_address.as_str())
            .unwrap()
            .build()
            .unwrap();
        let chooser = chooser(&connection);
        call_chooser(&chooser, "OpenFile", "", "Close it", HashMap::new(), 5)
    });
    let request_file = fixture_dir.join("request-5.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !request_file.is_file() {
        assert!(
            std::time::Instant::now() < deadline,
            "prompter did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let bus_address = bus.address().to_owned();
    let (concurrent_tx, concurrent_rx) = std::sync::mpsc::channel();
    let concurrent = std::thread::spawn(move || {
        let connection = zbus::blocking::connection::Builder::address(bus_address.as_str())
            .unwrap()
            .build()
            .unwrap();
        let chooser = chooser(&connection);
        let result = call_chooser(&chooser, "OpenFile", "", "Concurrent", HashMap::new(), 6);
        concurrent_tx.send(result).unwrap();
    });
    let (response, results) = concurrent_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("a blocked chooser must not serialize an unrelated client");
    assert_eq!(response, 0);
    assert_eq!(result_uris(&results), ["file:///tmp/concurrent.txt"]);
    concurrent.join().unwrap();

    let request = Proxy::new(
        &conn,
        PORTAL,
        "/org/freedesktop/portal/desktop/request/1/fc5",
        "org.freedesktop.impl.portal.Request",
    )
    .unwrap();
    let _: () = request.call("Close", &()).unwrap();
    let (response, _) = pending.join().unwrap();
    assert_eq!(response, 1);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));

    let _ = std::fs::remove_dir_all(data_dir);
    let _ = std::fs::remove_dir_all(runtime_dir);
    let _ = std::fs::remove_dir_all(fixture_dir);
}
