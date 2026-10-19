# AGENTS.md — guidance for LLMs using Manarouter

This file explains how an AI assistant should help a person configure or call a running Manarouter
instance. It is not a substitute for the product's current implementation status or provider docs.

## Explain the product simply

Manarouter is a self-hosted gateway between an app and one or more AI providers. The operator runs
an **upstream provider** configuration (and stores the provider's credential) and creates a separate
**client key** for apps that call the local gateway. Never confuse these two credentials. A *model*
is supplied by a provider; a *combo* is an operator-created route that can combine multiple models.

For a default local setup, the OpenAI-compatible API is `http://127.0.0.1:20900/v1` and the web
panel is `http://127.0.0.1:20901`. The relay and conductor are separate processes with separate
runtimes; `mr serve` starts both in the foreground, and the README documents launching them
individually. The panel is for local use and must not be exposed to an untrusted network without
deliberate authentication and network setup.

## Help a person get started

1. Ask what provider they want to use and whether they already have its API credential. Never ask
them to paste a secret into the conversation. Direct them to enter it in their own terminal.
2. Explain that the documented installer fetches source and compiles it; Rust (`rustup`) and Git are
required. The copy-paste command and steps are in `README.md`.
3. Help them add a provider, enter its secret using `mr account add <provider> --secret-stdin`, and
sync/list models. They paste the key in their terminal and send EOF (Ctrl-D). The encrypted vault
requires a local `vault.key` file, so remind them to protect their user account and backups. Do not recommend
`--secret <value>`: command-line arguments may be visible to other local processes.
4. Have them create a *client* key with `mr key create --label <label> --print`. It is printed once;
ask them to save it securely and not send it to the assistant.
5. Confirm the model name from the configured model list, then give a request example using the local
API and the client key. A request can incur provider charges.

Useful CLI commands (use `mr <command> --help` for exact options):

- `mr provider add/list/show/test ...`
- `mr account add/list ...`
- `mr models sync/list <provider>`
- `mr combo add/list/show ...`
- `mr key create/list/disable ...`

`mr serve` now runs both processes together for a foreground session, and is the quickest way to
start everything; the README still documents the two-process launch and the systemd units, which are
the supported way to run it unattended. `mr config` and `mr vault` are not implemented yet. Likewise,
consult `docs/23-implementation-status.md` before claiming a feature is implemented; command names may
exist before all behavior does.

## Calling the API

For OpenAI-compatible clients, use base URL `http://127.0.0.1:20900/v1` and send the client key as
`Authorization: Bearer <client-key>`. For example:

```bash
curl http://127.0.0.1:20900/v1/chat/completions \
  -H 'Authorization: Bearer YOUR_CLIENT_KEY' \
  -H 'Content-Type: application/json' \
  -d '{"model":"PROVIDER/MODEL","messages":[{"role":"user","content":"Hello"}]}'
```

Replace `PROVIDER/MODEL` with a model name that the user's configuration actually provides (or a
combo they created). Do not assume a specific provider, model, combo, account, or local port override.
The relay also has an Anthropic Messages surface at `http://127.0.0.1:20900` (no `/v1` in that base
URL); check provider and API documentation for request details. `GET /v1/models` is a useful first
check. If the response is 401, check that the *client* key was created and is being sent. If it is a
404, verify the path/base URL and model or route name. For upstream errors, inspect the provider,
account, model, and upstream credential without asking the user to reveal the credential.

## Secrets, safety, and consent

- Never request, echo, log, store in chat, or place in a generated file any provider API key, OAuth
token, client key, vault key, or panel password.
- Never ask a person to run a command that embeds a real secret in its shell history or process
arguments. Prefer the terminal's `--secret-stdin` flow for provider keys. Treat the one-time output
from `mr key create --print` as sensitive.
- Do not call provider test, model sync, or generation endpoints without the user's consent when they
could incur cost or consume quota. Explain the likely effect first when it is not obvious.
- Do not recommend exposing the panel or relay publicly as a convenience. Verify bind addresses,
password/authentication, TLS/reverse-proxy, and firewall assumptions before helping with remote use.
- Before changing or deleting a provider, account, combo, client key, config, or vault, explain the
impact and get confirmation. Prefer reversible steps and backups for configuration changes.
- If a credential was pasted into chat or a public log, advise the user to revoke/rotate it with its
provider. Do not repeat it.

## Keep advice honest

This project is actively developed. Read `docs/23-implementation-status.md` and the relevant
provider/API documentation before making claims about availability. Distinguish intended behavior
from implemented and verified behavior; do not claim a command worked unless its output or a test
confirms it. If a user is asking an LLM coding agent to modify this repository, follow the
repository-specific task map in `docs/LLM_INDEX.md`, inspect relevant source before editing, preserve
unrelated working-tree changes, and run the narrow tests plus broader checks appropriate to the edit.
