//! Routing regression for interfaces that require GTK's complete backend.

const PORTAL_FILE: &str = include_str!("../../../contrib/xdg-desktop-portal/portals/aegis.portal");
const PORTALS_CONF: &str = include_str!("../../../contrib/xdg-desktop-portal/aegis-portals.conf");

#[test]
fn app_chooser_is_delegated_as_a_whole_interface() {
    let interfaces = PORTAL_FILE
        .lines()
        .find_map(|line| line.strip_prefix("Interfaces="))
        .expect("portal metadata must declare Interfaces");
    assert!(
        !interfaces
            .split(';')
            .any(|interface| interface == "org.freedesktop.impl.portal.AppChooser"),
        "a partial AppChooser must not win interface-level backend selection"
    );
    assert!(
        PORTALS_CONF
            .lines()
            .any(|line| line == "org.freedesktop.impl.portal.AppChooser=gtk")
    );
}

#[test]
fn inhibit_is_delegated_as_a_whole_interface() {
    let interfaces = PORTAL_FILE
        .lines()
        .find_map(|line| line.strip_prefix("Interfaces="))
        .expect("portal metadata must declare Interfaces");
    assert!(
        !interfaces
            .split(';')
            .any(|interface| interface == "org.freedesktop.impl.portal.Inhibit"),
        "an idle-only Inhibit backend must not shadow GTK's complete interface"
    );
    assert!(
        PORTALS_CONF
            .lines()
            .any(|line| line == "org.freedesktop.impl.portal.Inhibit=gtk")
    );
}
