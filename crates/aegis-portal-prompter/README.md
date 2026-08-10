# aegis-portal-prompter

`aegis-portal-prompter` is the one-shot, out-of-process user interface host
for interactive portal requests that do not require compositor-owned
resources. The portal backend starts one independently supervised process for
each FileChooser, Account confirmation, or Secret password request, writes one
explicitly versioned JSON request to standard input, and reads one versioned
JSON response from standard output.

The process owns optics (iris/lens) file browsing, yes/no confirmation, and
masked password input and never connects to the compositor IPC socket. Its
process boundary prevents a slow filesystem, toolkit fault, or dialog crash
from blocking the backend or compositor. The backend remains responsible for
the D-Bus Request lifecycle and terminates only that process when the caller
closes its request.

The dialogs follow the aegis design language (the palette is mirrored
locally — the portal build graph stays independent of the Aegis repository)
and honour the system light/dark preference. The optics Rust bindings come
from the tagged `ming2k/optics` release like any other dependency; joint
development against a sibling optics checkout mirrors the Aegis workflow:
`cp .cargo/optics-local.toml .cargo/config.toml`.

One known visual difference from other toolkits: iris cannot yet import an
exported `wayland:` parent handle through xdg-foreign-v2, so prompts map as
independent windows rather than transient-for-parent dialogs.
