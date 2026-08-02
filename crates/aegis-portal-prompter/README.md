# aegis-portal-prompter

`aegis-portal-prompter` is the one-shot, out-of-process user interface host
for interactive portal requests that own host resources. The portal backend
starts one independently supervised process for each file-selection request,
writes one explicitly versioned JSON request to standard input, and reads one
versioned JSON response from standard output.

The process owns GTK4 file browsing and never connects to the compositor IPC
socket. It imports a `wayland:` parent handle through xdg-foreign-v2, while
the compositor sees only that window relationship. Its process boundary
prevents a slow filesystem, toolkit fault, or dialog crash from blocking the
backend or compositor. The backend remains responsible for the D-Bus Request
lifecycle and terminates only that process when the caller closes its request.
