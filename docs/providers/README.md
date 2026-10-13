# The provider catalogue

Manarouter ships with templates for the providers it knows about, in **`catalog/providers.toml`**.
Adding a provider is choosing a template and supplying a credential — not filling in a form from
documentation.

```bash
mr provider add anthropic --from-catalog anthropic
```

That prefills the dialect, the base URL, the auth kind, the capabilities and the fingerprint headers.
Without `--from-catalog` you would set each by hand, and the three or four that are easy to get subtly
wrong are exactly the ones the template exists to get right.

## Why a catalogue and not discovery

A template is knowledge that cannot be discovered at runtime:

- **Which dialect a provider speaks.** Two providers can share a base URL shape and differ in whether
  tools go in a `tools` array or a `functions` list. Guessing produces a request the provider rejects.
- **Which auth header.** `Authorization: Bearer` and `x-api-key` both exist, and a provider that wants
  the second will not say so — it returns 401.
- **Which fingerprint headers are mandatory.** GitHub Copilot returns a hard 400 without
  `Editor-Version`. No amount of model discovery reveals that.
- **Which capabilities are real.** A provider's `/models` endpoint lists models it will accept a
  request for, which is not the same as models that support tool calling.

So the catalogue is curated data, reviewed like code.

## Fields

| Field | Meaning |
|---|---|
| `id` | Stable identifier. Referenced by combos as `provider/model`. Renaming one breaks stored combos. |
| `label` | Display name for the panel. Free to change. |
| `dialect` | **One of the seven.** Determines request and response translation. |
| `base_url` | Where requests go. Some catalogues put `/v1` here, some do not — the entry records which. |
| `auth` | An `AuthKind`: `bearer`, `api-key-header`, `header`, `query-param`, `oauth`. |
| `capabilities` | Which behaviours are claimed. Drives what the router will even attempt. |
| `key_strategy` | Default account selection: `priority`, `round-robin`, `random`, `least-used`. |
| `note` | Free text, displayed in the panel. **Where uncertainty is recorded.** |
| `docs_url` | For the panel's link. |
| `headers` | Fingerprint headers sent on every request. |

OAuth entries add `oauth_platform`, `oauth_*_url`, `oauth_scopes`, `oauth_redirect_port`,
`oauth_pkce`, and for the Copilot family `copilot_*` fields. See
[OAuth overview](../10-oauth-overview.md).

### The seven dialects

`open-ai`, `anthropic`, `google`, `codex`, `copilot`, `antigravity`, `command-code`.

A dialect must be one of those. There is no "custom" dialect, and that is deliberate: a provider whose
requests cannot be described by one of these is a provider the router cannot route to, and pretending
otherwise would produce a configuration that looks valid and fails at request time. If a new shape is
genuinely needed, it is an adapter, and
[adding a platform](../14-oauth-adding-a-platform.md) describes the surrounding work.

### `note` is where uncertainty lives

Entries whose endpoint came from a shipped application rather than a public repository, or whose
headers are version-dependent, say so in `note` and appear marked in
[the catalogue](catalog.md#uncertainty-the-catalogue-records).

This is the file's most important convention. A provider entry that presents an unverified endpoint
as fact will work until it does not, and the operator will have no way to tell whether the router is
broken or the endpoint moved. Recording it in the data means the panel can show it.

## Adding a provider

1. **Check whether it is already there.** `grep -A5 'id = "thing"' catalog/providers.toml`. Many
   providers offer an OpenAI-compatible endpoint, which may already have an entry.

2. **Add the template** with the dialect, base URL, auth kind, capabilities and any fingerprint
   headers.

3. **Verify without spending money.** A wrong base URL or auth header is visible from the status code:
   a 401 means the auth, a 404 means the URL, a 400 usually means a missing header or a wrong dialect.

   ```bash
   mr provider add thing --from-catalog thing
   printf '%s' "$THING_KEY" | mr account add thing --secret-stdin
   mr provider test thing                 # one minimal real request
   mr models sync thing                   # only once test passes
   ```

   `mr provider test` prints the provider's **actual response body** on failure. That is the point of
   the command: a generic "request failed" tells you nothing about whether the key is wrong or the
   body shape is.

4. **If it is genuinely uncertain**, say so in `note` with `UNCERTAIN` and the reason. Do not omit the
   field, and do not guess silently.

## What the catalogue will never contain

**A provider that requires cookie or browser-session authentication.** Not `claude-web`, not
`gemini-web`, not `deepseek-web`, not any other. This is settled
([Security §6](../21-security.md#6-cookie-authentication-is-never-supported)) and the six known
web-session providers are deliberately absent from the file.

**A real credential.** Client ids and secrets that a vendor publishes in its own client are recorded,
because a native OAuth flow cannot work without them and OAuth 2.1 treats such clients as public. A
`GOCSPX-`-shaped value is assembled in `crates/mr-core/src/published.rs` rather than written out,
because GitHub's push protection refuses the literal — that module explains it. Nothing in the
catalogue is a secret.

## See also

- [The catalogue](catalog.md) — every provider that ships, generated from the TOML.
- [OpenAI-compatible](openai-compatible.md) — the largest family by far.
- [Anthropic-compatible](anthropic-compatible.md)
- [Command Code](commandcode.md)
- [Codex](codex.md) · [Copilot](copilot.md) · [Antigravity](antigravity.md)
- [Configuration](../03-configuration.md) — the provider schema in full.
