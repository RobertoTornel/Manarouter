# Proxies

This guide explains the proxy options represented in Manarouter's configuration and an important current limitation: outgoing relay requests are not yet wired through the proxy connector.

## Current status

**Gap — proxying is not active for normal relay traffic.** The Settings view can save proxy endpoint records and a selection strategy, and raw configuration can associate a proxy with a provider or account. But outgoing requests currently connect directly to the upstream provider instead of using those settings. Adding a proxy therefore does not route your provider traffic through it. Do not rely on it for egress policy, IP restrictions, or privacy.

## Configuration shape (reference only)

The operator config schema includes a top-level `proxies` list and `proxy_strategy`. A proxy record can carry an id, optional label, URL, username, vault secret reference, enabled flag, and priority. The supported config URL schemes are `http`, `https`, `socks4`, `socks5`, and `socks5h`; include the host and port. Never put a real proxy password in a URL or this document. Proxy credentials belong in the encrypted vault, not as plaintext in configuration. See [Configuration](../03-configuration.md) for the broader configuration context and [Security and credentials](11-security.md) for secret-handling guidance.

The Settings view can add, edit, remove, enable or disable proxy records and choose `priority`, `least-used`, `random`, or `round-robin`; it saves through the raw configuration endpoint. It does not manage provider/account assignments or proxy secrets; those require configuration and the vault. A provider may name a proxy id and an account may override it; the configured precedence is account over provider. These fields describe implemented configuration and selector behavior, not working relay proxying.

## What to do instead

If your deployment requires outbound proxying, configure it at an appropriate network layer outside Manarouter until proxy support is connected and verified in a release. Check your firewall and egress configuration independently; do not assume Manarouter honors proxy fields. For general safe deployment advice, see [Security and credentials](11-security.md).
