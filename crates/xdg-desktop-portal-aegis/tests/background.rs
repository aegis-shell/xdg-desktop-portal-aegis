//! Routing regression: Background is served by Aegis.

const PORTAL_FILE: &str = include_str!("../../../contrib/xdg-desktop-portal/portals/aegis.portal");
const PORTALS_CONF: &str = include_str!("../../../contrib/xdg-desktop-portal/aegis-portals.conf");

#[test]
fn background_is_served_by_aegis() {
    let interfaces = PORTAL_FILE
        .lines()
        .find_map(|line| line.strip_prefix("Interfaces="))
        .expect("portal metadata must declare Interfaces");
    assert!(
        interfaces
            .split(';')
            .any(|interface| interface == "org.freedesktop.impl.portal.Background"),
        "the Portal-owned Background must be advertised"
    );
    assert!(
        PORTALS_CONF
            .lines()
            .any(|line| line == "org.freedesktop.impl.portal.Background=aegis")
    );
}
