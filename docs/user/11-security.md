# Security and credentials

This guide explains how to protect Manarouter's separate provider credentials, relay client keys, panel access, and encrypted vault files.

## Keep credentials distinct

Manarouter uses different credentials for different trust boundaries:

- **Provider credential or OAuth grant:** lets Manarouter call an upstream AI provider on your behalf. Enter API credentials in your own terminal or through the provider's supported sign-in flow in the panel.
- **Client key:** lets an application call your Manarouter relay. Create one with `mr key create`; the plaintext is shown once. Store it securely because `mr key list` will not reveal it again. A client key is not an upstream provider credential.
- **Vault key (`vault.key`):** a local file Manarouter uses to decrypt the encrypted provider-secret store. Keep it private and maintain a protected backup with the corresponding encrypted vault data. Losing or replacing it can make stored provider credentials unrecoverable.

The panel password and browser session are separate again: they protect the control panel, not calls to upstream providers or the relay API.

## Enter and handle secrets safely

For API credentials, prefer the standard-input flow:

```bash
mr account add <provider> --secret-stdin
```

Paste the credential into the command's standard input, then send EOF (Ctrl-D on a typical Unix terminal). Do **not** use `--secret <value>`: command-line arguments can be visible to other local processes and may remain in shell history. `--secret-env <VAR>` is also supported, but environment variables may be exposed to processes or diagnostic dumps in some environments; use only where you understand those risks. Never put a real credential in this guide, a shell command, a screenshot, a ticket, or a conversation.

`mr key create --label <label> --print` prints only the newly created client key to standard output, once. Treat that output as a secret: do not paste it into chat, logs, source control, or a shared terminal. The non-`--print` form also reveals it once in its confirmation output. Save it in an appropriate password manager or secret store before closing the terminal. The same one-time-display rule applies to the replacement from `mr key rotate <id>`.

Do not send credentials to support staff. For diagnostics, share only the minimum error text after reviewing it for tokens, account identifiers, private URLs, and other sensitive details.

## Protect the panel and network

The panel binds to loopback by default. Keep it local unless remote access is a deliberate, reviewed requirement; do not expose it publicly as a convenience. A non-loopback panel bind without a password is rejected unless an explicit configuration opt-out allows remote access without one. Do not enable that opt-out.

If remote administration is necessary, first establish an appropriate authentication and network design. Prefer a private network or SSH tunnel. A reverse proxy does not replace application authentication: configure TLS correctly, preserve the request `Origin` header for state-changing panel operations, restrict allowed networks at the proxy and firewall, and avoid forwarding the panel to the public internet. Confirm the relay's bind and client-key requirements separately; protecting the panel does not automatically protect API traffic, and relay authentication does not protect panel administration.

TLS termination, proxy headers, trusted networks, and firewall rules are deployment-specific. Review them before enabling a non-loopback bind. Do not weaken authentication or firewall policy to troubleshoot a port-reachability issue.

## Protect vault files and backups

The encrypted provider-secret store and `vault.key` are both sensitive. File encryption helps if ciphertext alone is copied, but anyone who can read the key file and vault data as the Manarouter user can decrypt the credentials. Protect the host account, configuration directory, backups, and backup access controls. Keep the key backup separate from broad or shared backups where practical, while ensuring it remains recoverable by an authorized operator.

`mr doctor` can inspect whether vault files exist and whether the key file has expected metadata and restrictive permissions; it does not reveal or test secret values. `mr vault list` and `mr vault rekey` appear in help but are not implemented. Do not attempt a vault-key rotation by replacing `vault.key` manually.

## Rotation and revocation

If a provider credential may have leaked, revoke or rotate it at the provider first, then update or remove the affected Manarouter account using the provider's supported workflow. For an OAuth grant, revoke or disconnect the authorization at the provider if possible, then sign in again through the supported panel flow. Do not assume disabling an account revokes a credential at its provider.

If a client key may have leaked, use `mr key rotate <id>` to replace it; the old key is invalidated. Alternatively disable it with `mr key disable <id>` while investigating. Update every application using the old key, store the replacement securely, and remove a key only when it is no longer needed. Key rotation changes relay access and may break callers that have not been updated.

For a leaked panel password, change it in the panel's password settings. Changing the password revokes existing panel sessions; the browser used to make the change receives a fresh session. Do not share the password with support. If a secret was pasted into a public log or conversation, treat it as compromised even if the message is later deleted.

## See also

- [Troubleshooting](10-troubleshooting.md)
- [CLI reference](12-cli-reference.md)
