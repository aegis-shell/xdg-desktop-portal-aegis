//! Routing regression: Inhibit is served by Aegis (logind-backed).

const PORTAL_FILE: &str = include_str!("../../../contrib/xdg-desktop-portal/portals/aegis.portal");
const PORTALS_CONF: &str = include_str!("../../../contrib/xdg-desktop-portal/aegis-portals.conf");

#[test]
fn inhibit_is_served_by_aegis() {
    let interface = "org.freedesktop.impl.portal.Inhibit";
    let interfaces = PORTAL_FILE
        .lines()
        .find_map(|line| line.strip_prefix("Interfaces="))
        .expect("portal metadata must declare Interfaces");
    assert!(
        interfaces.split(';').any(|entry| entry == interface),
        "the logind-backed Inhibit must be advertised"
    );
    assert!(
        PORTALS_CONF
            .lines()
            .any(|line| line == format!("{interface}=aegis"))
    );
}
