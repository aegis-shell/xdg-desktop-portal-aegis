//! Routing regression: DynamicLauncher is served by Aegis.

const PORTAL_FILE: &str = include_str!("../../../contrib/xdg-desktop-portal/portals/aegis.portal");
const PORTALS_CONF: &str = include_str!("../../../contrib/xdg-desktop-portal/aegis-portals.conf");

#[test]
fn dynamic_launcher_is_served_by_aegis() {
    let interface = "org.freedesktop.impl.portal.DynamicLauncher";
    let interfaces = PORTAL_FILE
        .lines()
        .find_map(|line| line.strip_prefix("Interfaces="))
        .expect("portal metadata must declare Interfaces");
    assert!(
        interfaces.split(';').any(|entry| entry == interface),
        "the Portal-owned DynamicLauncher must be advertised"
    );
    assert!(
        PORTALS_CONF
            .lines()
            .any(|line| line == format!("{interface}=aegis"))
    );
}
