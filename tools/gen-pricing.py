#!/usr/bin/env python3
"""Generate ``catalog/pricing.json``: a bundled offline price table (USD per 1M tokens).

Purpose
-------
Manarouter ships this file in the binary (``include_str!``) so the models page and the
quota/cost views can show ``$/1M`` prices and estimate the cost of a request without any
network access at run time.

Source
------
LiteLLM's public price table:

    https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json

The fetch uses ``urllib`` with a 30 s timeout and follows redirects (urllib's default
``HTTPRedirectHandler``). If the download or parse fails the script prints a clear error
and exits non-zero **without writing anything** — a partial or fabricated price table
would silently mis-price real requests, so it is never produced. There is no bundled
fallback and no cached copy: the only input is the live upstream JSON.

Normalisation
-------------
LiteLLM states costs as USD **per token**; every cost is multiplied by ``1_000_000`` and
rounded to 6 decimals to give USD per 1M tokens. Per entry (key lowercased):

    input            <- input_cost_per_token            * 1e6
    output           <- output_cost_per_token           * 1e6
    cache_read       <- cache_read_input_token_cost     * 1e6
    cache_write      <- cache_creation_input_token_cost * 1e6
    mode             <- mode                  (e.g. "chat", "embedding")
    max_input_tokens <- max_input_tokens
    max_output_tokens<- max_output_tokens
    provider         <- litellm_provider

Costs are omitted when absent or non-numeric, so a present ``0`` (genuinely free) is kept
while a missing field is not invented. An entry is skipped entirely when it has neither an
``input`` nor an ``output`` cost. This is a *presence* test, not a truthiness test: LiteLLM's
own ``sample_spec`` documentation row declares ``0.0``/``0.0`` and is therefore retained
verbatim, as are free models. Numeric strings are accepted and coerced to numbers.

Determinism / idempotency
-------------------------
The output is a pure function of (upstream JSON, calendar date): no timestamps beyond the
date-only ``retrieved`` field, no environment values, no run-dependent ordering. Upstream
rows are processed in sorted order (so a key collision after lowercasing resolves the same
way every time), the ``models`` map is keyed in sorted order, each value's keys are written
in a fixed order, and the layout is one entry per line with no trailing whitespace.
Re-running on the same day with an unchanged upstream file yields byte-identical output;
``retrieved`` is the one field that changes when the date does.

Usage
-----
    python3 tools/gen-pricing.py                 # writes catalog/pricing.json
    python3 tools/gen-pricing.py --out /tmp/p.json
"""

from __future__ import annotations

import argparse
import datetime as _datetime
import json
import sys
import urllib.error
import urllib.request
from pathlib import Path

SOURCE_URL = (
    "https://raw.githubusercontent.com/BerriAI/litellm/main/"
    "model_prices_and_context_window.json"
)
SOURCE_LABEL = "LiteLLM model_prices_and_context_window.json"
FORMAT_VERSION = 1
CURRENCY = "USD"
PER = "1M tokens"
TOKENS_PER_UNIT = 1_000_000
COST_DECIMALS = 6

DEFAULT_OUT = Path(__file__).resolve().parent.parent / "catalog" / "pricing.json"

# Upstream cost field -> Manarouter output key.
COST_FIELDS = (
    ("input_cost_per_token", "input"),
    ("output_cost_per_token", "output"),
    ("cache_read_input_token_cost", "cache_read"),
    ("cache_creation_input_token_cost", "cache_write"),
)
# Output key order, fixed for a stable per-line diff.
VALUE_ORDER = (
    "input",
    "output",
    "cache_read",
    "cache_write",
    "mode",
    "max_input_tokens",
    "max_output_tokens",
    "provider",
)

FETCH_TIMEOUT_SECONDS = 30


def as_number(raw: object) -> float | None:
    """Coerce an upstream numeric value (or numeric string) to float, else None."""
    if isinstance(raw, bool) or raw is None:
        return None
    if isinstance(raw, (int, float)):
        return float(raw)
    if isinstance(raw, str):
        try:
            return float(raw.strip())
        except ValueError:
            return None
    return None


def as_int(raw: object) -> int | None:
    number = as_number(raw)
    if number is None:
        return None
    return int(number)


def per_million(raw: object) -> float | None:
    number = as_number(raw)
    if number is None:
        return None
    return round(number * TOKENS_PER_UNIT, COST_DECIMALS)


def fetch_source(url: str) -> str:
    request = urllib.request.Request(url, headers={"User-Agent": "manarouter-gen-pricing/1"})
    with urllib.request.urlopen(request, timeout=FETCH_TIMEOUT_SECONDS) as response:
        return response.read().decode("utf-8")


def build_entry(row: dict) -> dict | None:
    value: dict[str, object] = {}
    for upstream_key, out_key in COST_FIELDS:
        cost = per_million(row.get(upstream_key))
        if cost is not None:
            value[out_key] = cost

    if "input" not in value and "output" not in value:
        return None

    mode = row.get("mode")
    if isinstance(mode, str) and mode:
        value["mode"] = mode

    for key in ("max_input_tokens", "max_output_tokens"):
        number = as_int(row.get(key))
        if number is not None:
            value[key] = number

    provider = row.get("litellm_provider")
    if isinstance(provider, str) and provider:
        value["provider"] = provider

    return {key: value[key] for key in VALUE_ORDER if key in value}


def build_models(upstream: dict) -> tuple[dict, dict]:
    stats = {"rows": 0, "skipped_no_cost": 0, "skipped_bad_row": 0, "collisions": 0}
    models: dict[str, dict] = {}

    rows = []
    for key, row in upstream.items():
        if isinstance(key, str) and isinstance(row, dict):
            rows.append((key, row))
    # Sorted so a post-lowercasing collision resolves identically on every run.
    rows.sort(key=lambda item: item[0].lower())

    for key, row in rows:
        stats["rows"] += 1
        lowered = key.strip().lower()
        if not lowered:
            stats["skipped_bad_row"] += 1
            continue
        entry = build_entry(row)
        if entry is None:
            stats["skipped_no_cost"] += 1
            continue
        if lowered in models:
            stats["collisions"] += 1
            continue
        models[lowered] = entry

    return {key: models[key] for key in sorted(models)}, stats


def render(document: dict) -> str:
    compact = lambda obj: json.dumps(obj, ensure_ascii=False, separators=(",", ":"))
    lines = [
        "{",
        f'  "format_version": {document["format_version"]},',
        f'  "source": {compact(document["source"])},',
        f'  "retrieved": {compact(document["retrieved"])},',
        f'  "currency": {compact(document["currency"])},',
        f'  "per": {compact(document["per"])},',
        '  "models": {',
    ]
    items = list(document["models"].items())
    for index, (key, value) in enumerate(items):
        comma = "," if index < len(items) - 1 else ""
        lines.append(f"    {compact(key)}: {compact(value)}{comma}")
    lines.append("  }")
    lines.append("}")
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--out",
        default=str(DEFAULT_OUT),
        help=f"output path (default: {DEFAULT_OUT})",
    )
    args = parser.parse_args(argv)

    try:
        raw = fetch_source(SOURCE_URL)
    except (urllib.error.URLError, urllib.error.HTTPError, OSError, TimeoutError) as error:
        print(f"error: could not fetch {SOURCE_URL}: {error}", file=sys.stderr)
        print("error: refusing to write a partial price table", file=sys.stderr)
        return 1

    try:
        upstream = json.loads(raw)
    except json.JSONDecodeError as error:
        print(f"error: {SOURCE_URL} did not return valid JSON: {error}", file=sys.stderr)
        print("error: refusing to write a partial price table", file=sys.stderr)
        return 1

    if not isinstance(upstream, dict) or not upstream:
        print(f"error: {SOURCE_URL} did not return a non-empty JSON object", file=sys.stderr)
        return 1

    models, stats = build_models(upstream)
    if not models:
        print("error: no usable entries found in the upstream table", file=sys.stderr)
        return 1

    retrieved = _datetime.date.today().isoformat()
    document = {
        "format_version": FORMAT_VERSION,
        "source": SOURCE_LABEL,
        "retrieved": retrieved,
        "currency": CURRENCY,
        "per": PER,
        "models": models,
    }
    text = render(document)

    if json.loads(text) != document:  # pragma: no cover - defensive
        print("error: rendered output does not round-trip through json.loads", file=sys.stderr)
        return 1

    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with out_path.open("w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)

    size_kib = out_path.stat().st_size / 1024
    print(f"source:      {SOURCE_URL}")
    print(f"retrieved:   {retrieved}")
    print(f"upstream rows considered:  {stats['rows']}")
    print(f"skipped (no input/output): {stats['skipped_no_cost']}")
    if stats["skipped_bad_row"]:
        print(f"skipped (bad row):         {stats['skipped_bad_row']}")
    if stats["collisions"]:
        print(f"lowercase collisions:      {stats['collisions']}")
    print(f"models written:            {len(models)}")
    print(f"wrote:       {out_path} ({size_kib:.1f} KiB)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
