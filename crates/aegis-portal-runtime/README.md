# aegis-portal-runtime

Shared backend primitives for Aegis portal components.

The crate owns the `org.freedesktop.impl.portal.Request` lifecycle, including
exact-path registration, `Close` cancellation tracking, and cleanup. It has
no Aegis IPC dependency and is linked into the private
`xdg-desktop-portal-aegis` process.
