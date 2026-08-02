# Compatibility Reference

Portal and Aegis use independent version sequences. A Portal release
supports the exact Aegis Git tag pinned by the `aegis-core`, `aegis-ipc`,
and `aegis-logging` dependencies in the workspace `Cargo.toml`.

| Portal release | Aegis release | IPC dependency source |
|----------------|---------------|-----------------------|
| `v0.0.1` | `v0.0.9` | `https://github.com/ming2k/aegis`, tag `v0.0.9` |

The committed `Cargo.lock` resolves that tagged source. Distribution
packages must express an exact dependency on the Aegis version in this
table. A local path patch is development state and does not change release
compatibility.
