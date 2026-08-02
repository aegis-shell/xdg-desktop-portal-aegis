# aegis-portal-secret

Encrypted Secret implementation linked into the
`xdg-desktop-portal-aegis` process.

The crate owns the at-rest vault, the native
`org.freedesktop.impl.portal.Secret` backend, the transitional
`org.freedesktop.secrets` compatibility API, and their single-flight unlock
coordinator. It receives password prompting through the narrow
`SecretPrompter` capability and does not depend on Aegis IPC.
