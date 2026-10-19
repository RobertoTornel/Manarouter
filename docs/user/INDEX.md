# The user and agent guide

This series is the operator-facing documentation: how to run Manarouter, how to put a provider
behind it, and how to call it. It is written for a person — or for an assistant acting for one —
who wants to **use** the gateway, not for someone changing the gateway's internals.

**The panel serves this series.** The *Documentation* section of the panel lists exactly these
files, and each one is readable at `GET /api/docs/<name>`, where `<name>` is the file stem
(`03-calling-the-api`, `12-cli-reference`, and so on).

## Reading order

| # | Guide | What it answers |
|---|---|---|
| 00 | [Getting started](00-getting-started.md) | From a fresh install to your first request through the gateway. |
| 01 | [Add a provider](01-add-a-provider.md) | Which upstream to use, how to store its credential safely, and how to confirm the gateway can reach it. |
| 02 | [Client keys](02-client-keys.md) | Creating the key your app sends, and keeping it separate from the provider credential. |
| 03 | [Calling the API](03-calling-the-api.md) | The two client surfaces, with request examples and how to read the first response. |
| 04 | [Models and reasoning levels](04-models-and-reasoning-levels.md) | Which model names the relay resolves, and how to ask for a reasoning level. |
| 05 | [Combos](05-combos.md) | Named routes over several providers, and how their ordered steps are tried. |
| 06 | [Quota and usage](06-quota.md) | Configured quota windows, transient cooldowns, and the usage data the panel does *not* yet show. |
| 07 | [Images, audio, and video](07-images-audio-video.md) | The media routes, and which providers actually accept them. |
| 08 | [Proxies](08-proxies.md) | Sending upstream traffic through a proxy. |
| 09 | [Pricing](09-pricing.md) | What a request costs upstream, and how the relay counts it. |
| 10 | [Troubleshooting](10-troubleshooting.md) | The failures you are most likely to hit, and what causes each. |
| 11 | [Security and credentials](11-security.md) | The vault, the panel's access controls, and what must not be exposed. |
| 12 | [CLI reference](12-cli-reference.md) | Every `mr` command, with its flags. |

## The other series

The **build-side** documentation — the architecture, the configuration schema, the compiled
snapshot, the provider dialects, the API reference — lives in `docs/` and is indexed by
[`docs/LLM_INDEX.md`](../LLM_INDEX.md). It is deliberately **not** part of this series: it is
written for changing the project, and the two audiences are different. Nothing in the build-side
series is required to operate the gateway, and nothing in this series is required to change it.

## See also

- [What Manarouter is](../00-overview.md) — the one-page overview.
- [Implementation status](../23-implementation-status.md) — what actually runs today, and the
  command that checks each claim. Read it before telling anyone a feature exists.
- [Troubleshooting](../20-troubleshooting.md) — the build-side troubleshooting guide, for faults
  in the gateway itself rather than in a client.
