#!/usr/bin/env python3
"""Generate ``catalog/model_meta.json``: bundled per-model reasoning levels and input modalities.

Purpose
-------
Manarouter compiles this file into the binary with ``include_str!`` and uses it to
auto-seed a model's *available reasoning levels* and *accepted input types* when the
operator has not declared them by hand (see ``plan.md`` §"Effective levels").

Source (local, no network)
--------------------------
``.research/omniroute_models.json`` — a JSON dump of OmniRoute's ``GET /v1/models``
response: an object with a ``data`` array of ~9500 model entries. Per entry we read:

* ``id``             — upstream model id (lowercased for every key we emit)
* ``owned_by``       — provider id that serves the model (lowercased)
* ``effort_tiers``   — provider-declared reasoning tiers (606 rows), when present
* ``capabilities.effort_tiers`` — OmniRoute's catalog form of the same field (4127 rows)
* ``supportsThinking`` (top level) and ``capabilities.thinking`` — thinking support flags
* ``input_modalities`` — e.g. ``["text","image","pdf","video","file"]``

The optional ``name``/``output_modalities`` fields are intentionally ignored: names are
not stable identifiers, and Manarouter seeds *input* modalities only.

Reasoning-tier vocabulary (cross-checked against the installed OmniRoute source)
-------------------------------------------------------------------------------
Verified against ``/home/mana/.local/lib/node_modules/omniroute/`` (OmniRoute 3.8.51):

* ``src/shared/reasoning/effortStandardization.ts``
  - ``CANONICAL_EFFORT_VALUES = ["none","low","medium","high","xhigh","max"]`` — the
    generic, provider-agnostic vocabulary, and the tier set OmniRoute synthesises when a
    model declares thinking but no explicit tier list.
  - ``EFFORT_TIER_ALIASES = { extra: "xhigh" }`` — the UI synonym ``extra`` is ``xhigh``.
  - ``extendCodexGpt56EffortValues()`` adds ``max``/``ultra`` for specific Codex models
    (``gpt-5.6-{sol,terra,luna}``, ``gpt-6-{astra,sol,luna}``); ``ultra`` is *not* part of
    the generic vocabulary. ``minimal`` appears only in provider-specific tier lists
    (e.g. ``open-sse/config/providers/registry/perplexity/agent``, xAI ``thinking.ts``).
* ``open-sse/utils/syncedEffortVariants.ts`` — upstream ``reasoning.supported_efforts`` is
  surfaced verbatim as ``capabilities.effort_tiers`` (no renaming upstream).
* ``src/lib/modelMetadataRegistry.ts`` (lines ~490-520) — when a model declares thinking
  but has no declared tiers, OmniRoute fills in ``CANONICAL_EFFORT_VALUES``. It reads the
  declared tiers from ``capabilities.effort_tiers`` first and falls back to the top-level
  ``supportsThinking`` flag, which is why this script unions both tier fields.
* ``src/lib/vscode/reasoningMetadata.ts`` — the UI accepts
  ``none|low|medium|high|xhigh|max|ultra`` as suffixes.

Mapping onto Manarouter's canonical vocabulary
(``none, minimal, low, medium, high, xhigh, ultra, max``):

    none | off              -> none
    minimal | min           -> minimal
    low                     -> low
    medium | mid            -> medium
    high | default          -> high
    xhigh | x-high | x_high | extra_high | extrahigh | extra -> xhigh
    ultra                   -> ultra
    max | maximum | top     -> max

Normalisation is case-insensitive and ignores ``-``/``_``/spaces, so ``x-high``,
``X_High`` and ``ExtraHigh`` all land on ``xhigh``. Any spelling outside this table is
dropped (and counted in the run summary) rather than guessed at.

Level rule (documented, consistent choice)
------------------------------------------
1. Take the **union** of the entry's top-level ``effort_tiers`` and
   ``capabilities.effort_tiers`` (either may be absent; upstream in this dump the
   capability copy is a superset of the top-level one, but the union is what is specified
   and it is robust if upstream ever diverges). If that union yields at least one
   canonical level, use exactly those levels (normalised, de-duplicated, canonical order).
2. Otherwise, if ``supportsThinking`` is true **or** ``capabilities.thinking`` is true, use
   ``["none","low","medium","high","xhigh","max"]``.
   ``minimal`` and ``ultra`` are deliberately excluded from this *fallback*: OmniRoute's own
   generic fill-in (``CANONICAL_EFFORT_VALUES``, above) is exactly this six-value set, and
   ``ultra`` is only granted to specific Codex models. No claim is made that a generic
   thinking model supports those two, so we do not advertise them.
   (Note: in this dataset ``capabilities.supportsThinking`` is a strict subset of
   ``capabilities.thinking``, so consulting it too would change nothing.)
3. Otherwise the model gets no ``levels`` key.

Provider families (alias normalisation)
---------------------------------------
``owned_by`` is OmniRoute's provider id, and OmniRoute also lets the *same* provider
appear as a short alias — either in ``owned_by`` (``github``, ``agy``, ``codex``) or as a
redundant prefix on the model id (``gh/...``, ``cmd/deepseek/...``). ``PROVIDER_ALIASES``
maps every alias seen in this dump onto one canonical family name. Sources for the pairs:

* the installed registry's own ``alias:`` fields
  (``open-sse/config/providers/registry/{command-code,github,agy,cheaperinference,
  codex-app-server}/index.ts``);
* ``src/lib/modelsDevSync/transform.ts``'s provider alias table
  (``"github-copilot": ["github","gh"]``, ``bedrock: ["kiro","kr"]``, …);
* the leading id segment actually observed per ``owned_by`` in the dump (e.g. every
  ``cloudflare-playground`` model id starts ``cfp/``).

Canonicalisation is a single lookup: ``PROVIDER_ALIASES.get(name, name)``. Aliases whose
canonical name is not a Manarouter provider id are still mapped to the family name that
Manarouter's own catalog uses where one exists (``github``/``gh`` -> ``github-copilot``,
``agy`` -> ``antigravity``, ``codex``/``cx`` -> ``openai-codex``); the last of those is a
judgement call, since the dump also has a separate ``codex-app-server`` provider.

Two further normalisations follow from it:

* **by_provider_model keys** are ``"<canonical family>/<model id>"`` with a *redundant
  leading provider prefix stripped* — repeatedly, while that segment canonicalises to the
  same family. So ``cmd/deepseek/deepseek-v4.1-flash`` and
  ``command-code/deepseek/deepseek-v4.1-flash`` (both owned by ``command-code``) collapse
  to the single key ``command-code/deepseek/deepseek-v4.1-flash``: exactly one key form per
  family, never ``command-code/cmd/...`` or ``command-code/command-code/...``.
  A leading ``no-think/`` modifier is deliberately **not** stripped: it marks a
  thinking-disabled variant, not a provider, so stripping it would merge distinct models
  and mis-seed their levels.
* ``providers`` in a value lists canonical family names; ``provider_ids`` lists the **raw**
  ``owned_by`` spellings seen for that entry, so no information is lost.

Output rules
------------
* Every model id, provider id and family name is lowercased; original provider spelling is
  preserved only inside ``provider_ids``.
* Duplicate model ids across providers are merged: ``levels`` and ``modalities`` are the
  union, ``providers``/``provider_ids`` collect every value seen for the id.
* ``by_model`` is keyed by the raw lowercased OmniRoute model id (as in the original spec);
  ``by_provider_model`` is the per-family view of the same value shape, keyed as described
  above, and its ``providers``/``provider_ids`` hold only that one family/raw pair (the key
  already names it). Consumers that want the cross-provider union use ``by_model``.
* ``levels`` is omitted when empty; ``modalities`` is omitted when empty or exactly
  ``["text"]``. An entry — and with it its provider lists — is emitted only when it carries
  at least one of those two seedable keys; provider lists alone are not a reason to emit a
  model.
* Lists are sorted: ``levels``/``modalities`` in canonical vocabulary order, provider lists
  alphabetically. Value keys are written in sorted order
  (``levels, modalities, provider_ids, providers``).

Determinism
-----------
Pure function of the input file plus this script: no clock, no network, no environment.
Keys are sorted (model maps by key; each value's keys sorted), timestamps are never
embedded, and the layout is one entry per line with no trailing whitespace, so regenerating
produces a minimal line diff. Running the script twice yields byte-identical output.

Usage
-----
    python3 tools/gen-model-meta.py               # writes catalog/model_meta.json
    python3 tools/gen-model-meta.py --out /tmp/x.json
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

# Canonical vocabulary, in the order consumers expect (Manarouter's ReasoningLevel enum
# is None < Low < Medium < High < XHigh < Ultra < Max; `minimal` is new and slots in
# between `none` and `low`).
CANONICAL_LEVELS = ["none", "minimal", "low", "medium", "high", "xhigh", "ultra", "max"]
CANONICAL_MODALITIES = ["text", "image", "audio", "video", "pdf", "file"]

# OmniRoute's generic thinking-but-no-tiers fill-in (CANONICAL_EFFORT_VALUES).
THINKING_FALLBACK_LEVELS = ["none", "low", "medium", "high", "xhigh", "max"]

# Spelling -> canonical level. Keys are lowercased with "-", "_" and spaces removed.
LEVEL_ALIASES = {
    "none": "none",
    "off": "none",
    "minimal": "minimal",
    "low": "low",
    "medium": "medium",
    "high": "high",
    "default": "high",
    "xhigh": "xhigh",
    "extra": "xhigh",
    "extrahigh": "xhigh",
    "ultra": "ultra",
    "max": "max",
    "maximum": "max",
    "top": "max",
}

# Provider alias / short prefix -> one canonical family name. Identity mappings need no
# entry: canonical_family() falls back to the name itself. See the module docstring for
# where each pair comes from; every pair below is exercised by the dump.
PROVIDER_ALIASES = {
    # explicit request
    "github": "github-copilot",
    "gh": "github-copilot",
    "cmd": "command-code",
    "command_code": "command-code",
    # registry `alias:` fields / models.dev table / observed id prefixes
    "agy": "antigravity",
    "aug": "auggie",
    "af": "api-airforce",
    "cinf": "cheaperinference",
    "cw": "claude-web",
    "cl": "cline",
    "cf": "cloudflare-ai",
    "cfp": "cloudflare-playground",
    "cx": "openai-codex",
    "codex": "openai-codex",
    "cxa": "codex-app-server",
    "ds-web": "deepseek-web",
    "dva": "devin-cli-agentic",
    "ddgw": "duckduckgo-web",
    "faik": "freeaiapikey",
    "gweb": "gemini-web",
    "kr": "kiro",
    "llamacpp": "llama-cpp",
    "llmkiwi": "llm-kiwi",
    "ollamacloud": "ollama-cloud",
    "oad": "openadapter",
    "oc": "opencode",
    "pol": "pollinations",
    "unc": "uncloseai",
    "veo-free": "veoaifree-web",
    "vag": "vercel-ai-gateway",
    "zw": "zai-web",
    "zc": "zcode",
}

# Modifiers that are part of a model id's meaning, not a provider prefix. Never stripped.
NON_PROVIDER_LEADING_SEGMENTS = {"no-think"}

DEFAULT_OUT = Path(__file__).resolve().parent.parent / "catalog" / "model_meta.json"
DEFAULT_SOURCE = Path(__file__).resolve().parent.parent / ".research" / "omniroute_models.json"

SOURCE_LABEL = "omniroute v16.3.5 model metadata (.research/omniroute_models.json)"
FORMAT_VERSION = 1

VALUE_KEY_ORDER = ("levels", "modalities", "provider_ids", "providers")


def normalise_level(raw: object) -> str | None:
    """Map one upstream tier spelling onto the canonical vocabulary (or None)."""
    if not isinstance(raw, str):
        return None
    key = "".join(ch for ch in raw.strip().lower() if ch not in "-_ ")
    return LEVEL_ALIASES.get(key)


def normalise_modality(raw: object) -> str | None:
    if not isinstance(raw, str):
        return None
    value = raw.strip().lower()
    return value if value in CANONICAL_MODALITIES else None


def canonical_family(name: str) -> str:
    """Map a raw ``owned_by`` / id-prefix spelling onto one canonical family name."""
    return PROVIDER_ALIASES.get(name, name)


def strip_provider_prefix(model_id: str, family: str) -> tuple[str, int]:
    """Drop redundant leading ``<provider>/`` segments, returning (id, stripped count).

    A segment is redundant when it canonicalises to this entry's own family, e.g.
    ``command-code/cmd/deepseek/x`` owned by ``command-code`` -> ``deepseek/x``.
    ``no-think/...`` (and anything else that is not this family) is left alone.
    """
    stripped = 0
    while True:
        head, sep, rest = model_id.partition("/")
        if not sep or not rest:
            return model_id, stripped
        if head in NON_PROVIDER_LEADING_SEGMENTS or canonical_family(head) != family:
            return model_id, stripped
        model_id = rest
        stripped += 1


def canonical_order(values: set[str], vocabulary: list[str]) -> list[str]:
    return sorted(values, key=lambda v: vocabulary.index(v))


class Accumulator:
    """Union of levels/modalities/providers for one merge key."""

    __slots__ = ("levels", "modalities", "providers", "provider_ids")

    def __init__(self) -> None:
        self.levels: set[str] = set()
        self.modalities: set[str] = set()
        self.providers: set[str] = set()
        self.provider_ids: set[str] = set()

    def value(self) -> dict[str, object] | None:
        """The emitted value, or None when neither ``levels`` nor ``modalities`` applies.

        Provider lists alone are not enough to justify an entry: an entry is emitted only
        when it carries at least one of the two seedable keys.
        """
        value: dict[str, object] = {}
        levels = canonical_order(self.levels, CANONICAL_LEVELS)
        modalities = canonical_order(self.modalities, CANONICAL_MODALITIES)
        has_modalities = bool(modalities) and modalities != ["text"]
        if not levels and not has_modalities:
            return None
        if levels:
            value["levels"] = levels
        if has_modalities:
            value["modalities"] = modalities
        if self.provider_ids:
            value["provider_ids"] = sorted(self.provider_ids)
        if self.providers:
            value["providers"] = sorted(self.providers)
        return {key: value[key] for key in VALUE_KEY_ORDER if key in value}


def declared_tiers(entry: dict) -> list[str]:
    """Union of the top-level and ``capabilities.effort_tiers`` lists, normalised."""
    tiers: list[str] = []
    for raw in (entry.get("effort_tiers"), (entry.get("capabilities") or {}).get("effort_tiers")):
        if isinstance(raw, list):
            tiers.extend(t for t in raw if isinstance(t, str))
    levels = {lvl for lvl in (normalise_level(t) for t in tiers) if lvl is not None}
    return canonical_order(levels, CANONICAL_LEVELS)


def entry_levels(entry: dict) -> list[str]:
    """Rule: union of declared tier lists, else the generic thinking fallback, else nothing."""
    levels = declared_tiers(entry)
    if levels:
        return levels

    capabilities = entry.get("capabilities")
    capabilities = capabilities if isinstance(capabilities, dict) else {}
    supports_thinking = entry.get("supportsThinking") is True or capabilities.get("thinking") is True
    if supports_thinking:
        return list(THINKING_FALLBACK_LEVELS)
    return []


def entry_modalities(entry: dict) -> list[str]:
    raw = entry.get("input_modalities")
    if not isinstance(raw, list):
        return []
    found = {mod for mod in (normalise_modality(m) for m in raw) if mod is not None}
    return canonical_order(found, CANONICAL_MODALITIES)


def build_document(models: list[dict]) -> tuple[dict, dict]:
    by_model: dict[str, Accumulator] = {}
    by_provider_model: dict[str, Accumulator] = {}
    stats = {
        "entries": 0,
        "skipped_no_id": 0,
        "unknown_levels": 0,
        "aliased_providers": 0,
        "stripped_prefixes": 0,
        "entries_without_provider": 0,
    }

    for entry in models:
        if not isinstance(entry, dict):
            continue
        stats["entries"] += 1
        model_id = str(entry.get("id") or "").strip().lower()
        if not model_id:
            stats["skipped_no_id"] += 1
            continue

        raw_provider = str(entry.get("owned_by") or "").strip().lower()
        family = canonical_family(raw_provider)
        if raw_provider and family != raw_provider:
            stats["aliased_providers"] += 1

        levels = entry_levels(entry)
        modalities = entry_modalities(entry)

        for field in ("effort_tiers", (entry.get("capabilities") or {}).get("effort_tiers")):
            if isinstance(field, list):
                for tier in field:
                    if isinstance(tier, str) and normalise_level(tier) is None:
                        stats["unknown_levels"] += 1

        target = by_model.setdefault(model_id, Accumulator())
        target.levels.update(levels)
        target.modalities.update(modalities)
        if raw_provider:
            target.providers.add(family)
            target.provider_ids.add(raw_provider)

        if raw_provider:
            key_id, stripped = strip_provider_prefix(model_id, family)
            stats["stripped_prefixes"] += stripped
            pair = by_provider_model.setdefault(f"{family}/{key_id}", Accumulator())
            pair.levels.update(levels)
            pair.modalities.update(modalities)
            pair.providers.add(family)
            pair.provider_ids.add(raw_provider)
        else:
            stats["entries_without_provider"] += 1

    by_model_out: dict[str, dict] = {}
    for key in sorted(by_model):
        value = by_model[key].value()
        if value is not None:
            by_model_out[key] = value

    by_provider_model_out: dict[str, dict] = {}
    for key in sorted(by_provider_model):
        value = by_provider_model[key].value()
        if value is not None:
            by_provider_model_out[key] = value

    document = {
        "format_version": FORMAT_VERSION,
        "source": SOURCE_LABEL,
        "levels": list(CANONICAL_LEVELS),
        "modalities": list(CANONICAL_MODALITIES),
        "by_model": by_model_out,
        "by_provider_model": by_provider_model_out,
    }
    return document, stats


def render(document: dict) -> str:
    """One JSON value per line, fixed key order, single trailing newline."""
    compact = lambda obj: json.dumps(obj, ensure_ascii=False, separators=(",", ":"))

    lines = [
        "{",
        f'  "format_version": {document["format_version"]},',
        f'  "source": {compact(document["source"])},',
        f'  "levels": {compact(document["levels"])},',
        f'  "modalities": {compact(document["modalities"])},',
    ]
    for section in ("by_model", "by_provider_model"):
        lines.append(f'  "{section}": {{')
        items = list(document[section].items())
        for index, (key, value) in enumerate(items):
            comma = "," if index < len(items) - 1 else ""
            lines.append(f"    {compact(key)}: {compact(value)}{comma}")
        lines.append("  }," if section == "by_model" else "  }")
    lines.append("}")
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--out",
        default=str(DEFAULT_OUT),
        help=f"output path (default: {DEFAULT_OUT})",
    )
    parser.add_argument(
        "--source",
        default=str(DEFAULT_SOURCE),
        help=f"OmniRoute models JSON (default: {DEFAULT_SOURCE})",
    )
    args = parser.parse_args(argv)

    source_path = Path(args.source)
    if not source_path.is_file():
        print(f"error: source file not found: {source_path}", file=sys.stderr)
        return 1

    with source_path.open(encoding="utf-8") as handle:
        payload = json.load(handle)
    models = payload.get("data") if isinstance(payload, dict) else None
    if not isinstance(models, list):
        print(f'error: {source_path}: expected an object with a "data" array', file=sys.stderr)
        return 1

    document, stats = build_document(models)
    text = render(document)

    # Fail loudly rather than ship a broken include_str! payload.
    if json.loads(text) != document:  # pragma: no cover - defensive
        print("error: rendered output does not round-trip through json.loads", file=sys.stderr)
        return 1

    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with out_path.open("w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)

    families = {key.split("/", 1)[0] for key in document["by_provider_model"]}
    provider_values = {p for value in document["by_model"].values() for p in value.get("providers", [])}
    size_kib = out_path.stat().st_size / 1024
    print(f"source entries:             {stats['entries']}")
    if stats["skipped_no_id"]:
        print(f"entries skipped (no id):    {stats['skipped_no_id']}")
    if stats["unknown_levels"]:
        print(f"unmappable tier spellings:  {stats['unknown_levels']}")
    print(f"alias-normalised providers: {stats['aliased_providers']}")
    print(f"redundant prefixes stripped:{stats['stripped_prefixes']:>6}")
    print(f"by_model entries:           {len(document['by_model'])}")
    print(f"by_provider_model entries:  {len(document['by_provider_model'])}")
    print(f"by_provider_model families: {len(families)}")
    print(f"with levels:                {sum(1 for v in document['by_model'].values() if 'levels' in v)}")
    print(f"with modalities:            {sum(1 for v in document['by_model'].values() if 'modalities' in v)}")
    print(f"providers w/o a family key: {sorted(provider_values - families)}")
    print(f"wrote:                      {out_path} ({size_kib:.1f} KiB)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
