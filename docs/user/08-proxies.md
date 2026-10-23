# Proxies

This guide explains how Manarouter routes upstream traffic through a proxy, and what happens when
the proxy does not work.

## Current status

**Working.** A provider or an account that names a proxy has its upstream traffic sent through that
proxy: the relay opens a tunnel to the proxy and sends the request inside it. The supported proxy
URL schemes are `http`, `https`, `socks4`, `socks5` and `socks5h`.

Two rules are worth knowing before you configure anything:

* **A proxy that is configured is used.** There is no "try direct if the proxy is down" mode. If the
  tunnel cannot be opened, the request fails with `proxy_error` and it is **not** rerouted to another
  provider, because the next provider might egress directly — and that would defeat the reason you
  configured a proxy.
* **A proxy in the pool is used even when nothing names it.** If the proxy list is not empty and a
  provider (and its account) do not name a proxy, the configured `proxy_strategy` picks one from the
  pool for every request. Configuring a proxy therefore changes where *all* your provider traffic
  goes, not only the traffic of a provider you name it on. There is no per-provider scoping: once
  any proxy is in the pool it is used for every provider that does not name its own, so keep the
  pool empty if the rest of your traffic must stay direct.

## Which proxy a request uses

The precedence is:

1. the **account's** proxy, if the account names one;
2. otherwise the **provider's** proxy, if the provider names one;
3. otherwise the **pool**, chosen by `proxy_strategy`;
4. otherwise a **direct** connection (the pool is empty).

`proxy_strategy` is one of `priority` (the lowest `priority` value first, ties in list order),
`least-used` (the proxy with the fewest requests in flight), `random` or `round-robin`.

A proxy that you have **disabled** is not dialed. If a provider or account names a disabled proxy,
its traffic goes out directly, because "disabled" means "do not use this one" — not "use it and hope
it works". A proxy id that does not exist at all is a configuration error: the relay refuses to
start and names the provider and the id.

## Configuration shape

The operator config schema includes a top-level `proxies` list and `proxy_strategy`. A proxy record
can carry an id, optional label, URL, username, vault secret reference, enabled flag and priority.
The supported config URL schemes are `http`, `https`, `socks4`, `socks5` and `socks5h`; include the
host and the port. Never put a real proxy password in a URL or in this document. Proxy credentials
belong in the encrypted vault, not as plaintext in configuration. See
[Configuration](../03-configuration.md) for the broader configuration context and
[Security and credentials](11-security.md) for secret-handling guidance.

The Settings view can add, edit, remove, enable or disable proxy records and choose `priority`,
`least-used`, `random` or `round-robin`; it saves through the raw configuration endpoint. It does not
manage provider/account assignments or proxy secrets; those require configuration and the vault.

A provider may name a proxy id and an account may override it; the configured precedence is account
over provider. The per-account override exists because a single credential may be region-locked:
moving just that account behind a proxy must not move the whole provider.

### The password

A proxy that needs a password stores it in the vault and names the vault entry in `secret_ref`. The
username is either the proxy's `username` field or the userinfo in the URL; the `username` field
wins when both are present. A configured username with no readable vault secret is an error, not an
unauthenticated dial — the relay refuses the request rather than connecting to the proxy without a
credential. SOCKS4 is the exception: its userid has no password field, so a username alone is
accepted.

The password is read from the vault at the moment the tunnel is opened, so rotating it takes effect
on the next request rather than at the next restart.

## What a failed tunnel looks like

The request fails with HTTP `502` and an error body whose code is `proxy_error`. The message names
what failed — a refused `CONNECT`, a proxy that required authentication, a timeout while waiting for
the proxy's reply. The upstream provider is never contacted directly, and the request is not
rerouted to another provider.

The relay's own view of the provider is unchanged: a proxy failure is an egress failure, not a
provider failure, so the provider is not put into a cooldown for it. If the proxy is down, every
request that uses it fails until the proxy is back.

## Limitations worth knowing

* **One pool per proxy.** Each proxy has its own connection pool, so a proxied request can never be
  served on a connection that was opened directly. That is deliberate: a pool shared between direct
  and proxied traffic could hand a request that was supposed to leave through a proxy a connection
  that leaves from this machine.
* **Hostname resolution follows the scheme.** With `socks5h` the *proxy* resolves the target name;
  with `socks5` the relay resolves it and sends the address.
* **Streaming responses are not switched mid-stream.** Once a response has started, it is the
  response the client receives. The relay does not abandon a half-sent answer for a faster one.
