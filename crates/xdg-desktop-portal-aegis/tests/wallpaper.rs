//! Whole-interface delegation regression for Wallpaper.

const PORTAL_FILE: &str = include_str!("../../../contrib/xdg-desktop-portal/portals/aegis.portal");
const PORTALS_CONF: &str = include_str!("../../../contrib/xdg-desktop-portal/aegis-portals.conf");

#[test]
fn partial_aegis_backend_cannot_shadow_gtk() {
    let interface = "org.freedesktop.impl.portal.Wallpaper";
    let interfaces = PORTAL_FILE
        .lines()
        .find_map(|line| line.strip_prefix("Interfaces="))
        .expect("portal metadata must declare Interfaces");
    assert!(!interfaces.split(';').any(|entry| entry == interface));
    assert!(
        PORTALS_CONF
            .lines()
            .any(|line| line == format!("{interface}=gtk"))
    );
}
