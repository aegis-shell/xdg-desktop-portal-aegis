# aegis-pam

Optional PAM module for password-protected Aegis secret vaults.

After another PAM module verifies a password, `pam_aegis.so` writes a
short-lived mode-0600 token into the user's runtime directory. The
`aegis-portal-secret` component consumes and deletes that token to unlock the
vault without showing a second password prompt. Module failures never decide
authentication and must be configured as `optional`.

Place `auth optional pam_aegis.so` after the module that establishes the
authentication token. The module resolves the authenticated account with
`getpwnam_r`, writes only to the kernel-owned `/run/user/<uid>` directory, and
refuses an unsafe owner or mode. It never trusts the authenticating process's
`XDG_RUNTIME_DIR`.
