# aegis-pam

Optional PAM module for password-protected Aegis secret vaults.

After another PAM module verifies a password, `pam_aegis.so` writes a
short-lived mode-0600 token into the user's runtime directory. The
`aegis-portal-secret` component consumes and deletes that token to unlock the
vault without showing a second password prompt. Module failures never decide
authentication and must be configured as `optional`.
