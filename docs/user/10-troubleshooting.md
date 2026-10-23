# Troubleshooting

Use this guide to diagnose common Manarouter startup, panel, routing, account, and configuration problems without exposing credentials.

## Start with safe checks

```bash
mr doctor
mr version
mr models list
```

`mr doctor` reads the configuration and checks its validation, vault-file metadata, routing snapshot, client-key setup, and configured listener addresses. It does not open the vault, contact providers, bind or probe ports, or repair files. `mr version` shows the configuration path. `mr models list` prints the stored model catalogue from earlier syncs without fetching anything. These checks do not change Manarouter state.

Do not use `mr models sync` as a read-only check: it contacts the provider and saves the returned model list. `mr provider test <provider>` makes a real authenticated upstream request — by default a model-list probe; `--model` adds a one-token completion that may consume quota. Use either only when you intend that network request.

## The relay or panel will not start

The default local endpoints are separate:

- **Relay (API):** `http://127.0.0.1:20900`, with the OpenAI-compatible API under `/v1`.
- **Conductor (web panel):** `http://127.0.0.1:20901`.

A panel page opened on the relay port, or an API request sent to the panel port, uses the wrong listener. Check `mr doctor` for configured addresses and `mr version` for the configuration location. `mr serve --help` shows the temporary `--relay-port` and `--control-port` options; these override the ports for that run, they do not edit the configuration.

If a listener fails to bind, another process may already be using that address and port. Check the service's startup output and listener ownership using your operating system's normal tools. Do not stop or restart a process until you know which service it is. A configuration error may also prevent startup; use `mr doctor` to see configuration validation findings.

The panel is loopback-only by default. A non-loopback panel bind without a configured password is refused at startup, unless an explicit insecure remote-without-password opt-out has been configured. Do not use that opt-out to make the panel easier to reach; see [Security](11-security.md).

## HTTP errors

### `401 Unauthorized`

First identify which listener returned the response:

- **Relay API:** the client key is missing, invalid, disabled, or from another Manarouter instance. Send an enabled *client key* with the request (for example, as `Authorization: Bearer …`) and check `mr key list`. A provider API credential is not a client key. With no client keys configured, loopback relay requests are unauthenticated; `mr doctor` warns about this setup.
- **Panel:** the panel login session may have expired; sign in again. A panel password/session is separate from both relay client keys and provider credentials.
- **Upstream response:** the provider may have rejected its credential. Check the account's state and renew or replace that provider credential through its provider. Never paste it into chat or a report.

### `404 Not Found`

Confirm that the request uses the relay's correct path and port, then check the exact `model` value. Useful read-only commands are:

```bash
mr models list
mr combo list
mr alias list
```

Use a qualified `provider/model` id if a bare model name is ambiguous. Confirm that the provider, model, combo, or alias is enabled and exists. A relay response saying `model_not_found` means the requested model/route could not be resolved; it is different from a web server returning 404 for a wrong URL path.

### `429 Too Many Requests`

A 429 can be the provider's own rate/quota response, or Manarouter's `no_usable_accounts` response when every account/route step is unavailable or cooling down. Read the response body and any `Retry-After` header. Check `mr account list <provider>` and `mr combo show <name>`; re-enable an account only if it was deliberately disabled. For provider throttling, wait for the provider's reset or consult its status and quota page. Do not repeatedly retry an authentication failure.

### Other upstream or gateway errors

A provider response can be passed through to the caller. Check the provider's service status, endpoint/base URL, model availability, account state, and network access from the Manarouter host. A timeout or connection failure is not proof that a credential is wrong. If you choose to run `mr provider test <provider>`, remember the default probe is an authenticated model-list call; adding `--model` sends a one-token completion that may use quota. Share only redacted status/error text when seeking help.

## OAuth account no longer works

OAuth connections are available only for supported provider entries, and the sign-in method depends on the provider. Reconnect the account through the panel when its state indicates an authentication failure or its grant has expired. Codex has browser and device-code flows; GitHub Copilot uses device code; Antigravity uses a browser authorization flow, which is experimental in this version. If that integration fails, treat it as an integration limitation rather than repeatedly retrying. A headless server or a browser on another machine may need a device-code flow where the provider supports one. The CLI has no OAuth sign-in command.

Some providers use single-use refresh tokens. If a grant has been consumed or revoked, retrying the same request will not repair it; complete a fresh sign-in. If the OAuth platform or login flow is not offered for that provider, do not try to substitute browser cookies or session tokens.

## Configuration or vault validation problem

Run `mr doctor` to check configuration parsing and validation, vault-file metadata, and the routing snapshot. It is a diagnostic, not a repair command. `mr config validate`, `mr config show`, `mr config edit`, and `mr config path` are listed in CLI help, but **the `mr config` command is not implemented yet**; do not rely on them. `mr vault` commands are also unimplemented. `mr version` reports the configuration path. If correcting configuration manually, make a protected backup first and change only the intended setting; invalid configuration changes can prevent services from starting.

If `mr doctor` reports a missing vault key while encrypted vault data exists, restore the matching `vault.key` from a protected backup. Do not delete or replace the key: encrypted provider credentials cannot be decrypted without the matching file. Keep the key and encrypted vault private.

## Related guides

- [Security and credentials](11-security.md)
- [CLI reference](12-cli-reference.md)
