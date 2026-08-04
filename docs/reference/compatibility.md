# Compatibility Reference

Portal and Aegis use independent version sequences. A Portal release
supports the exact Aegis Git tag pinned by the `aegis-authority`,
`aegis-core`, `aegis-ipc`, and `aegis-logging` dependencies in the workspace
`Cargo.toml`. `aegis-authority` is used directly by the integration-test
compositor; the production backend receives the same types through
`aegis-ipc`.

| Portal release | Aegis release | IPC dependency source |
|----------------|---------------|-----------------------|
| `v0.0.3` | `v0.0.11` | `https://github.com/aegis-shell/aegis`, tag `v0.0.11` |
| `v0.0.2` | `v0.0.11` | `https://github.com/ming2k/aegis`, tag `v0.0.11` |
| `v0.0.1` | `v0.0.9` | `https://github.com/ming2k/aegis`, tag `v0.0.9` |

The committed `Cargo.lock` resolves that tagged source. Distribution
packages must express an exact dependency on the Aegis version in this
table. A local path patch is development state and does not change release
compatibility.

Portal `v0.0.2` and `v0.0.3` use Aegis `v0.0.11`, whose IPC protocol version
is 24. The FileChooser process boundary does not add a private Aegis
file-picking operation: the backend and its one-shot GTK process own that
resource flow.
