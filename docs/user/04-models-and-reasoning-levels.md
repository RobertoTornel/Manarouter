# Models and reasoning levels

Use this guide to choose a model name the relay can resolve and, for models that support it, request a reasoning level explicitly.

## Find a name the relay accepts

The most reliable source is the relay's `GET /v1/models` listing. With the default relay port:

```bash
read -rsp 'Manarouter client key: ' MR_CLIENT_KEY
printf '\n'
curl --config - <<CURL
url = "http://127.0.0.1:20900/v1/models"
header = "Authorization: Bearer $MR_CLIENT_KEY"
CURL
unset MR_CLIENT_KEY
```

When Manarouter has no client keys configured, its loopback development setup can allow requests without a key; once keys are configured, send a valid Manarouter client key. The model listing is built from the current routing snapshot and does not contact providers. It includes visible provider models, their reasoning metadata, and combo names. Read `manarouter.reasoning_levels` and `manarouter.default_reasoning_level` when choosing a level-qualified name.

Use a fully qualified provider/model name for a provider model:

```json
{
  "model": "PROVIDER/MODEL",
  "messages": [{"role": "user", "content": "Hello"}]
}
```

Replace the uppercase placeholders with IDs from your own listing; they are placeholders, not real providers or credentials. For example, the structure is `provider-id/model-id`. Use a combo name or an alias you created when you want an operator-defined route. Matching is case-insensitive for ASCII model/provider names.

A bare model ID such as `MODEL` is **not** a provider-model lookup. Without the provider prefix it must match a combo or alias, otherwise the relay returns `404 model_not_found` with an explanation. This avoids silently choosing one provider when several offer the same model ID. Model IDs can contain `/` themselves, so preserve the provider prefix and the entire remaining model ID: `provider/team/model-name` is a qualified model name, not two routes.

Exact model IDs take precedence over interpreting a trailing `-<level>` as a suffix. If a real model's ID is `MODEL-low`, use that exact qualified ID to select that model; the router does not also read it as model `MODEL` at level `low`. The `/v1/models` endpoint suppresses suffix variants for model IDs that already end in a recognized level word.

## Request a reasoning level

Add a level suffix to a qualified model name, or to a combo name, to request a level:

```json
{
  "model": "PROVIDER/MODEL-high",
  "messages": [{"role": "user", "content": "Explain the result briefly."}]
}
```

The current canonical ladder, from lowest to highest, is:

```text
none < minimal < low < medium < high < xhigh < ultra < max
```

`minimal` is also accepted and listed when the selected model advertises it. `xhigh` is the canonical suffix used in model names and in the API listing. The level parser also accepts human-friendly spellings such as `x-high`, `extra-high`, `extra_high`, and `extrahigh` in contexts that use that parser, but use canonical `xhigh` in model names. Availability depends on the selected model. Manarouter uses a non-empty model-level `reasoning_levels` list first; if that is empty, it can derive levels from its bundled model metadata; otherwise a model/provider declared with reasoning capability gets a generic set, and models without level metadata or a reasoning capability have no reasoning-level suffixes. `GET /v1/models` lists only levels the model actually offers. A combo lists suffixes that are offered by at least one of its steps; each step then runs at that level if available, or its own highest available level if not.

If a model supports the requested level, the relay selects it. If the model does not offer the requested level, the request does **not** fail solely for that reason: it falls back to that model's highest available level. A model with no reasoning levels uses `none` and gets no default reasoning fragment. These substituted levels are not listed as supported options; use the model-list metadata to see what is offered.

When you omit the suffix, Manarouter uses the model's configured `default_reasoning_level`, if any. Otherwise it uses `routing.default_reasoning_level`, which defaults to `high`; if the selected model does not offer that configured default, Manarouter substitutes the model's highest offered level. The model-list entry reports the effective default. A combo without a suffix uses each step's own effective default; a combo with a suffix requests the same level for each step, subject to each step's available levels. One detail: `none` is not a universal synonym for unqualified model naming. A model that offers `none` has a `-none` suffix listed, but if a model has no reasoning tiers at all, its base name runs without a reasoning fragment.

If your request already contains an explicit reasoning field, such as `reasoning_effort`, Manarouter does not overwrite that field when it merges its level fragment. Avoid specifying contradictory reasoning both in the model suffix and in the request body; the explicit body field remains authoritative where the two overlap.

## What the level means on the upstream wire

A suffix is a Manarouter routing instruction, not generally a model ID understood by the upstream provider. The relay retargets the body to the upstream model ID and adds the selected dialect-specific reasoning fragment when needed. Defaults include:

| Upstream dialect | Reasoning fragment |
|---|---|
| OpenAI-compatible, Copilot, Command Code Go | `reasoning_effort` such as `low`, `medium`, or `high`; `ultra`/`max` normalize to the dialect's supported ceiling `xhigh`. |
| Codex Responses | `reasoning: {"effort":"…","summary":"auto"}`; `ultra`/`max` normalize to `xhigh`. |
| Anthropic | `thinking: {"type":"enabled","budget_tokens":N}`; default budgets are 1,024 for `minimal`/`low`, 10,240 for `medium`, and 131,072 for `high` and above. |
| Google / Antigravity | `generationConfig.thinkingConfig`; `thinkingLevel` is `low` below `high`, `high` at `high` and above, alongside a budget. |

These are translation defaults, not a guarantee that every model accepts every tier. Provider behavior can change, and the advertised model metadata is the router's current best account of that model. `none` sends no default reasoning fragment. A model-specific `reasoning_wire` override replaces the default fragment for the configured level; it is an operator configuration detail, not something to add to an API request.

## When a model name fails

| Response or symptom | What to check |
|---|---|
| `404 model_not_found` and a message requiring a provider prefix | Use the exact `provider/model` ID from `GET /v1/models`, or choose a configured combo/alias. A bare model ID is not resolved by provider order. |
| `404` says `unknown provider` | Check the provider prefix before `/` against your configured provider IDs. |
| `404` says a provider exists but does not offer the model | Check the rest of the name for spelling/case and verify the provider has that model configured and visible. |
| A model responds at a lower tier than you requested | Check `manarouter.reasoning_levels` in `GET /v1/models`. The requested level may be out of range and substituted with the model's highest available level. |
| Upstream returns `400` after a reasoning request | Confirm the model really supports the requested capability and that your explicit request-body reasoning fields are valid for that provider. Model metadata can be stale or operator-entered. |

See [Calling the API](03-calling-the-api.md) for base URLs, authentication, routes, and HTTP status guidance; see [Troubleshooting](10-troubleshooting.md) for other relay/provider errors. For configuration, use the panel or CLI; the relevant schema fields are `ModelEntry.reasoning_levels`, `ModelEntry.default_reasoning_level`, `ModelEntry.reasoning_wire`, and `routing.default_reasoning_level`.

## Not yet implemented

The panel's general models table does not display each model's reasoning levels or effective default today; check `GET /v1/models` for that metadata. It also does not show a per-request reasoning substitution in a user-friendly usage view. The relay logs out-of-range direct model-level substitutions, but do not assume an arbitrary client will receive a special response field identifying the final upstream tier.
