//! SetWallpaper (protocol 26) round-trip tests against the independent test
//! server: the image crosses as a sealed memfd behind the request header.
#![cfg(feature = "test-server")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aegis_portal_ipc::testing::{Handler, Server};
use aegis_portal_ipc::{Client, ConnectionCapabilities, LOCAL_PORTAL_SCOPE, WallpaperPlacement};

fn socket_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "aegis-ipc-{name}-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

/// Records every wallpaper application.
#[derive(Default)]
struct Recording {
    applied: Mutex<Vec<(WallpaperPlacement, Vec<u8>)>>,
}

impl Handler for Recording {
    fn set_wallpaper(
        &self,
        _connection: u64,
        placement: WallpaperPlacement,
        image: Vec<u8>,
    ) -> Result<(), String> {
        self.applied.lock().unwrap().push((placement, image));
        Ok(())
    }
}

#[test]
fn wallpaper_image_crosses_the_wire_sealed() {
    let handler = Arc::new(Recording::default());
    let server =
        Server::start(&socket_path("wallpaper"), handler.clone()).expect("bind test server");
    let mut client = Client::connect_scoped_with_timeout(
        server.path(),
        ConnectionCapabilities {
            query: true,
            control: true,
            input: false,
            session: false,
            interaction_domain: false,
        },
        LOCAL_PORTAL_SCOPE,
        Duration::from_secs(5),
    )
    .expect("handshake");
    assert_eq!(client.protocol_version(), 26);

    // A PNG header plus payload stands in for a real image.
    let image = b"\x89PNG\r\n\x1a\nfake-pixels".to_vec();
    client
        .set_wallpaper(&image, WallpaperPlacement::Both)
        .expect("set wallpaper");

    let applied = handler.applied.lock().unwrap();
    assert_eq!(applied.as_slice(), &[(WallpaperPlacement::Both, image)]);
}

#[test]
fn wallpaper_refusal_surfaces_as_an_error() {
    struct Refusing;
    impl Handler for Refusing {
        fn set_wallpaper(
            &self,
            _connection: u64,
            _placement: WallpaperPlacement,
            _image: Vec<u8>,
        ) -> Result<(), String> {
            Err("no outputs configured".into())
        }
    }
    let server = Server::start(&socket_path("wallpaper-refuse"), Arc::new(Refusing))
        .expect("bind test server");
    let mut client = Client::connect_with_timeout(
        server.path(),
        ConnectionCapabilities::QUERY,
        Duration::from_secs(5),
    )
    .expect("handshake");

    let error = client
        .set_wallpaper(b"image", WallpaperPlacement::Background)
        .expect_err("the refusal must surface");
    assert_eq!(error.to_string(), "no outputs configured");
}

#[test]
fn wallpaper_requires_protocol_26() {
    // A compositor from before the op negotiates the client down to 25;
    // the call then fails before anything is sent.
    let server = Server::start_legacy(
        &socket_path("wallpaper-v25"),
        Arc::new(Recording::default()),
        25,
    )
    .expect("bind legacy test server");
    let mut client = Client::connect_with_timeout(
        server.path(),
        ConnectionCapabilities::QUERY,
        Duration::from_secs(5),
    )
    .expect("handshake");
    assert_eq!(client.protocol_version(), 25);

    let error = client
        .set_wallpaper(b"image", WallpaperPlacement::Background)
        .expect_err("protocol 25 has no wallpaper op");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[test]
fn wallpaper_image_length_is_bounded() {
    let server = Server::start(
        &socket_path("wallpaper-cap"),
        Arc::new(Recording::default()),
    )
    .expect("bind test server");
    let mut client = Client::connect_with_timeout(
        server.path(),
        ConnectionCapabilities::QUERY,
        Duration::from_secs(5),
    )
    .expect("handshake");
    assert!(
        client
            .set_wallpaper(b"", WallpaperPlacement::Background)
            .is_err()
    );
    assert!(
        client
            .set_wallpaper(
                &vec![0u8; (aegis_portal_ipc::MAX_WALLPAPER_BYTES + 1) as usize],
                WallpaperPlacement::Background,
            )
            .is_err()
    );
}
