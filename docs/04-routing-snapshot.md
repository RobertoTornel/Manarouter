# The routing snapshot

The snapshot is the compiled routing table the relay reads on every request. It is the mechanical
consequence of the project's design rule: everything that can be decided before a request arrives is
decided once, in the conductor, and written into a flat binary file that the relay decodes at load
time and never interprets again.

This document describes what is actually in `crates/mr-core/src/snapshot.rs` — the frame, the index,
the resolution variants, the reload path — and is explicit about the one claim the module itself
declines to make.

---

## 1. Why a compiled table rather than a per-request decision

The authored configuration in `config.json` is rich and hand-editable. It can hold disabled
providers, disabled accounts, hidden models, aliases that chain, hedge participant lists expressed as
"the first N eligible steps", and steps that name providers which have since been deleted. None of
that should be interpreted while a request is in flight, for two reasons that compound.

The first is cost. Resolving a combo name against the authored form means walking a `Vec<Combo>`,
filtering disabled steps, checking each step's provider still exists, sorting candidate accounts by
priority, applying `models_allow`/`models_deny`, clamping hedge participants to the surviving step
set, and following an alias chain that may be several hops long. Every one of those is a branch and a
possible surprise, and every one of them is the same answer for every request until the configuration
changes.

The second is that the answer can change *while a request is in flight*. If a request reads the
configuration halfway through a save, it can see a combo whose steps point at a provider that has just
been removed. A snapshot removes that class of bug by construction: the relay holds an immutable
`Arc<RoutingSnapshot>`, and an in-flight request keeps the one it started with until it finishes.

### The naive alternative, and why it is worse

The obvious alternative is a two-process design where the relay asks the conductor per request: "what
does `Brains` mean?" That is what a naive split of the same monolith looks like, and it trades a
latency problem for a worse one:

- **A socket round trip**, which on loopback is ~0.14 ms measured — already at the p50 budget of
  150 µs before any serialisation happens.
- **A serialisation and deserialisation** of the answer, on both sides, for every request.
- **A context switch and a scheduler decision** on the conductor, which means the request now waits
  behind whatever else the conductor is doing. The conductor is the process that owns SQLite, OAuth
  refresh and the panel — precisely the work that made the reference router take seconds.
- **A new failure mode**: traffic stops when the helper is busy, not when it is down. A slow
  `fsync` in the control plane becomes a latency spike in the data plane, which is the exact coupling
  the two-process split exists to break.

The snapshot converts "decide per request, with a dependency on another process" into "read a pointer
and hash a string". The conductor is on the path only at the moment a human saves a change.

---

## 2. What "zero interpretation on the request path" does and does not mean

The module documentation is careful here, and the distinction is worth repeating because it is easy
to overclaim.

**What is bought.** Every interpretation decision is made by `RoutingSnapshot::compile` and never
again. Account order is resolved (disabled accounts dropped, the rest ordered by ascending priority
with ties keeping authored order), hidden models are gone, hedge participant indices are remapped onto
the surviving steps, alias chains are followed to a final target, and the lookup index that answers
combo names, `provider/model` pairs and bare model ids is built once. The request path is an atomic
load of an `Arc` plus a hash lookup. It never walks a `Config`, sorts anything, or resolves an alias
chain.

**What is *not* bought.** It is **not** literally zero-copy over a memory-mapped file.
`RoutingSnapshot::decode` validates the frame and then builds owned `String`s and `Vec`s, so a decoded
snapshot lives on the ordinary heap. The module says so directly: truly zero-copy would require a
layout-stable `repr(C)` schema and an mmap whose lifetime is tied to the file, and the portability and
validation costs of that are not worth paying for a table read once per reload. The earlier ambition
of reading fields straight out of shared memory was given up in favour of a format that can be
validated and rejected cleanly.

The trade is deliberate: a decode that allocates costs microseconds **once**, at load; a format that
cannot be validated costs correctness every time it is wrong. The property that matters on the request
path is *zero interpretation*, and that is what the snapshot actually delivers.

Note also that `crates/mr-core/src/lib.rs` still describes the compiled form as a "flat, `repr(C)`-ish
layout … read out of shared memory without invoking a parser". The first half is aspirational
colour and the second half is the true claim; this document follows `snapshot.rs`, which is where the
bytes are.

---

## 3. The frame

Every snapshot file is exactly this, and nothing else:

```text
magic[8] | version u32 | digest u64 | payload_len u32 | checksum u64 | payload[..]
```

| Field | Offset | Type | Protects against |
|---|---|---|---|
| `magic` | 0 | 8 bytes | Being handed an unrelated file. The constant is `SNAPSHOT_MAGIC = *b"MROUTES1"` in `mr-core/src/lib.rs`. A mismatch is reported as *"this is not a compiled routing snapshot"*, before any structure is built. |
| `version` | 8 | `u32` LE | Loading a layout this binary cannot interpret. `SNAPSHOT_VERSION = 4`. A mismatch is refused, not guessed at. |
| `digest` | 12 | `u64` LE | Being unable to tell "the file changed" from "the bytes changed" without comparing whole snapshots. Copied from `Config::routing_digest()`. |
| `payload_len` | 20 | `u32` LE | A truncated or padded file. Checked against the actual byte count in both directions: fewer bytes is a precise "truncated" error, more bytes is a "trailing bytes" error. |
| `checksum` | 24 | `u64` LE | A single flipped byte anywhere meaningful. |
| `payload` | 32 | bytes | — |

`HEADER_LEN` is 32, and the offsets above are that constant decomposed rather than a second source of
truth.

The checksum is **FNV-1a 64** (offset basis `0xcbf2_9ce4_8422_2325`, prime `0x0000_0100_0000_01b3`)
computed over `version || digest || payload`. Folding the version and digest into the hashed region
means a corrupted *header* is caught by the same check that catches a corrupted *payload*, so
`verify_header` can trust every value it returns. FNV-1a is not a cryptographic hash and is not meant
to be: it detects accidental corruption, which is what a file rewritten by an interrupted write or a
bad disk produces. An adversary who can rewrite the snapshot can rewrite the checksum too — the
snapshot is not signed, and nothing in the design pretends otherwise.

`RoutingSnapshot::encode` refuses to write a payload larger than the `u32` the frame can address,
rather than truncating silently. Encoding is deterministic for a given table: the same snapshot
produces the same bytes, so two machines can compare snapshots byte-for-byte.

`verify_header` exists as a separate public function so a relay can decide whether a file is worth
decoding at all — for example to skip a reload whose digest is already installed. It does hash the
payload, because the checksum is the only thing that distinguishes a truncated file from a valid one;
what it avoids is allocating the snapshot's owned structures.

---

## 4. The payload: string table, then structures

The payload begins with a **string table**, and every string elsewhere in the payload is a `u32`
index into it:

```text
string_count u32 | (length u32 | bytes) × string_count | body...
```

The string table exists because the same strings repeat heavily across the table. Provider ids, base
URLs, header names, model ids and auth-kind parameter names appear on many steps and many accounts. A
step stores only its provider index and its model id; every other per-step field is reconstructed from
the provider it points at, so there is exactly one copy of each base URL and timeout set in the
compiled form. Without the table, the frame would be dominated by the same slug copied a hundred
times, and the file would grow with `steps × providers` instead of with the distinct string set.

A "no string" slot is the sentinel `NO_STRING = u32::MAX`, which is safe because a real string index
is never `u32::MAX` — the count is bounded by the payload size, which is bounded by a `u32`.

---

## 5. What compile does

`RoutingSnapshot::compile(&Config)` is the only place that interprets routing configuration. Its
failure modes are chosen, not incidental:

- A combo whose **routable** steps all name a provider that does not exist is an `Error::Snapshot`
  naming the combo. That is an operator typo, and failing the whole compile is better than failing
  that combo at request time.
- A combo whose steps are merely disabled, or point only at providers the operator has disabled, is
  **silently omitted**. Disabling a provider is a deliberate act; it must not make the snapshot fail
  to compile, and the panel is where the omission is visible.
- An **alias cycle** is an `Error::Snapshot` naming the alias. A chain that never terminates has no
  correct flattened target, so there is nothing sensible to store.
- Disabled providers, disabled accounts and disabled combos are removed at this stage. Hidden models
  are dropped from the direct-lookup step list while the model entry itself survives.

Each account becomes an `AccountPlan` carrying scheduling metadata and a **reference** to its
credential — a vault id, never a value. The compiler resolves the conventional `account_secret_id`
fallback so the relay never builds a string to find a key. Hiding a model removes it from a provider's
direct-lookup step list, and a step naming a provider that has been disabled never enters the table.

Account **permission** filtering is the one thing that stays on the request path. `models_allow` and
`models_deny` are copied verbatim into each `AccountPlan`, because the answer depends on the concrete
model being requested — which the compiler does not know. `Account::candidate_accounts` is the
function that applies them, and its doc comment claims the set "is computed at compile time and
stored in the snapshot"; the code does not do that, and cannot, because the list is per-request. What
the snapshot stores is the allow/deny lists themselves, and the relay evaluates them when it selects
an account. [Key selection](05-key-selection.md) §5 works through what that means in practice.

Hedge handling has one subtlety worth naming: `hedge.enabled` in the plan is
`combo.hedge.enabled && participants.len() >= 2`. A "race" of one participant is not a race, so the
compiler downgrades it rather than executing race bookkeeping for a single request. Likewise, if
`on_winner_fail` names a stable fallback step that is no longer routable, the mode is replaced with
its default rather than left dangling at an index into a shrunken step list.

### Level-aware compilation

Reasoning levels are compiled, not parsed per request, in the same way everything else here is.
`plan_step` turns a `ModelEntry` plus its `ProviderPlan` into a `StepPlan`, and the level fields it
fills in are what the relay merges into the upstream body:

| `StepPlan` field | Type | What it carries |
|---|---|---|
| `model` | `String` | The model id **as the upstream expects it**, and therefore **never** carrying a `-<level>` suffix. The suffix selected `level`; forwarding it would name a model the upstream does not have. |
| `level` | `u8` | The level a direct request to this step runs at, compiled as the same value as `default_level`. |
| `levels` | `u32` | Bitmask of the levels this model genuinely offers, bit `n` = `ReasoningLevel::rank() == n`. Zero is a real answer: the model advertises no levels. |
| `default_level` | `u8` | The level a request gets when it names this model with no suffix. Recorded as `none` whenever the model's level set does **not** contain `none` — which is always true of a model with no levels at all — because such a model has no default reasoning tier to send. |
| `reasoning_json` | `[Option<String>; 8]` | Per level, the encoded JSON object fragment the relay merges into the upstream body. `None` means "send nothing for this level" — the correct answer for `none`, for a dialect with nothing to say, and for a level the model does not offer. |
| `input_modalities` | `u32` | Bitmask of accepted input kinds, over `InputModality`'s discriminant. |
| `pricing` | `Option<PricingPlan>` | Input/output and cache rates per million tokens, plus where the numbers came from. Copied in so a cost estimate never has to look the model up again. |

`StepPlan::supports_level`, `StepPlan::levels`, `StepPlan::accepts_modality` and
`StepPlan::reasoning_fragment` are the accessors; `levels()` materialises the bitmask in ascending
order, which is what the listing and the panel read.

**Why the level lives in the step rather than in the request name.** The name the client sent is not
available to the merge step as a parsed value — the relay deliberately never fully parses a body it
can pass through — and a level has to be decided even when the name carried no suffix. Compiling the
default into the step is what makes "no suffix" and "an explicit suffix that matches the default"
the same code path.

**Which levels a model offers is derived here, in a fixed order:** the operator's explicit
`reasoning_levels` on the model entry wins outright; otherwise the bundled table
`catalog/model_meta.json`, keyed `provider/model` and falling back to a provider-agnostic `model`
key; otherwise, when the model or its provider declares the `reasoning` capability — or the dialect
serves only reasoning models, currently `codex` — the generic set `none, low, medium, high, xhigh,
max`. A model that reaches none of the three has `levels == 0` and its name gets no `-<level>`
suffix. The generic set deliberately omits `minimal`, `ultra` and `max`: they are provider-specific
tiers, and advertising one a provider rejects turns a working request into a 400.

---

## 6. The index, and precedence

One requested string can mean several things at once. `Brains` might be an alias, a combo id, a combo
name, and a bare model id all at the same time. The index resolves that with a fixed precedence, which
is also exactly the order in which the panel and `mr` will warn about shadowing.

```text
RANK_ALIAS            = 0   the alias `from` value
RANK_COMBO_ID         = 1   ComboId
RANK_COMBO_NAME       = 2   the callable combo name, optionally with a -<level> suffix
RANK_QUALIFIED        = 3   provider/<exact model id>
RANK_BARE             = 4   a bare model id
RANK_QUALIFIED_LEVEL  = 5   provider/<base model>-<level>
```

Lower rank wins. So an alias always beats a combo name, and a combo name always beats the bare model
id it shadows — which is what makes "name a combo after the model the client already uses" a
non-breaking change. `RANK_MAX` is `RANK_QUALIFIED_LEVEL`, and `link_entries` rejects any entry
carrying a higher rank rather than letting an unknown precedence into the table.

**Level-qualified keys, and why `provider/<exact model id>` outranks them.** Feature 3 added two
kinds of derived key, and the rank order between them is the whole point:

- `comboName-<level>`, compiled at `RANK_COMBO_NAME` — the same rank as the combo name itself. It
  resolves to the same combo with an explicit level, which is how a route is pinned to a tier
  without editing it: `coding-high` runs `coding` at `high`. A combo name that merely contains a dash
  is untouched, because a suffix that is not a level name compiles no key at all.
- `provider/<base model>-<level>` for each level, at `RANK_QUALIFIED_LEVEL` = 5, the *lowest*
  precedence there is. An exact model id at rank 3 therefore always beats a `base-level` reading of
  itself. `antigravity/gemini-3.8-flash-low` is a real model id; it is not read as
  `gemini-3.8-flash` plus the level `low`, and the entry for the shorter model is only consulted when
  no model by the longer name exists.

**A `base-level` key is indexed even for a level the model does not offer.** That is not an
inconsistency: it is how an out-of-range level succeeds instead of failing. The entry's `level` is
the model's **highest available** level, while its `requested` field records what the name actually
asked for. `Resolution::Direct::substituted` is then `level != requested` — a field read rather than
a re-parse of the name. So `-max` on a model that tops out at `xhigh` reroutes to `xhigh`; a model
reporting no levels at all reroutes to `none`, which emits no fragment rather than inventing
support. A model whose own id ends in a level word gets **no** `base-level` keys at all, because
those names would resolve to nothing: the exact-model-id rule wins.

**A miss is explained, not just reported.** `NotFound` carries a `reason` string produced by
`explain_miss`, and the three cases need different fixes. A name with no provider prefix says that
it must be a combo or an alias, and that a model is addressed as `provider/model`, optionally with a
`-<level>` suffix. A name whose prefix is not a configured provider names the typo and lists the
configured providers. A known provider with an unmatched model says so — it does not guess at a
closest candidate.

The index is a `HashMap<u64, u32>` from a hash of the folded key to the head of a chain in
`index_entries`. Each `IndexEntry` stores:

- `key` — the lookup key, ASCII-lowercased at compile time.
- `rank` — the precedence above.
- `target` — an `IndexTarget::{Alias, Combo, Step}`.
- `next` — the next entry in the same hash bucket.

**A hash collision is never trusted.** `lookup_target` computes `fnv1a64_lower_ascii(requested)`, then
walks the bucket and compares `entry.key.eq_ignore_ascii_case(requested)` — the full string, case
insensitively. A collision costs one extra string comparison and can never route to the wrong target.
When several genuinely equal keys share a bucket, the lowest rank wins. The chain walk is bounded by
the number of entries, so a snapshot whose `next` pointers were built wrongly — or a hand-edited
in-memory table — cannot hang a request.

The `next` chains are not persisted. `link_entries` rebuilds them from `index_entries` in vector
order, which is the single place the index invariant is established, and the decoder reproduces the
same order. That is why `encode` can store a plain array and still round-trip exactly.

The hash is FNV-1a over ASCII-lowercased bytes, not the crate-level `ids::fnv1a64`, which is
case-sensitive. Both use the same offset basis and prime; the snapshot's variant folds case in as it
hashes, and `IndexEntry` stores the already-folded key so the comparison in the walk is against a
lowercased string.

`lookup(&self, requested: &str) -> Option<&ComboPlan>` is the narrow accessor for callers that only
care about combos. `resolve` is the general one.

---

## 7. `Resolution`, and what the relay does with it

```rust
pub enum Resolution<'a> {
    Combo { plan: &'a ComboPlan, level: Option<u8> },
    Direct { step: &'a StepPlan, level: u8, substituted: bool },
    NotFound { reason: std::borrow::Cow<'a, str> },
}
```

- **`Combo`** — the name matched a combo. The relay walks `plan.steps` in the operator's order under
  the plan's `strategy`, applies the plan's `hedge` if enabled, and selects an account using the
  target provider's `key_strategy`. Everything it needs — step order, remapped participants, timeouts
  — is already in the plan. `level` is `Some` only when the client named a tier (`combo-name-high`);
  `None` means the name carried no suffix, and **each step then runs at its own model's default
  level**. That distinction is deliberate: forcing the configuration default onto every step would
  override a per-model `default_reasoning_level` the operator set on purpose. A combo named *with* a
  suffix forces that tier onto every step, and a step whose model does not offer it falls back to
  that model's highest — the same substitution rule as a direct name.
- **`Direct`** — the name matched a concrete provider/model step. The relay calls that provider with
  that model, at `level`. `substituted` is `true` when the level asked for was not one the model
  offers and its highest available level was used instead; the relay logs it and the panel shows it.
  Nothing else about the resolution changes — the request still succeeds. A `provider/model` request
  and a bare-model request both land here; the difference is only which index rank matched.
- **`NotFound`** — nothing matched, and `reason` says why (see §6). The relay answers with a routing
  error — a 404 carrying `model_not_found` and that reason as the message — and the client gets a
  distinct, actionable failure rather than a generic upstream 503. The relay does not attempt an
  upstream call and does not guess.

`resolve` follows alias hops up to `MAX_ALIAS_HOPS = 8`. Compile-time flattening means a runtime chain
longer than one hop indicates the snapshot was built by something else or is corrupt, and the bound
exists so a hand-edited or malicious file degrades to `NotFound` instead of hanging the relay. Even
the "should not occur" `IndexTarget::Alias` case is handled rather than assumed away.

`resolve` is `&self` and returns borrowed plans; it allocates nothing and takes no locks.

---

## 8. The only mutable state, and where the rest lives

The compiled snapshot is **immutable**. `RoutingSnapshot` has no atomics, no `Mutex`, no interior
mutability of any kind, and its fields are private specifically so that the relay cannot mutate the
table around the plans it borrows. There are deliberately **no per-step round-robin counters inside
the snapshot**: a counter in the table would make a routing decision a write, would make two workers
disagree about which account is "next", and would make a hot swap lose or double-count the rotation
position.

The only mutable state on the request path is the `ArcSwap` inside `SnapshotHandle` and the
relay-local state a mutable strategy needs. Round-robin cursors live in the relay's own `RouteTable`
(relaxed `AtomicU64` counters beside the `ArcSwap`, `manarouter-relay/src/state.rs`), and live account
health — cooldowns, `available_after`, failure counts — lives in the relay process's in-memory `Health`
map, not in the snapshot and not in the state database. The relay consumes the *order* the conductor
published and the strategy the plan names.

This is a deliberate split with a cost, and the cost is worth stating: because the snapshot is
immutable, `KeyStrategy::RoundRobin` cannot be a counter embedded in the table. Its exact placement
is a conductor-side decision, and [key selection](05-key-selection.md) documents the algorithm and
what it costs.

Notably, this is the one place where a reader of this documentation may have been told otherwise.
`docs/LLM_INDEX.md` and an earlier outline of this document describe "per-step round-robin counters as
relaxed atomics" in the snapshot. **That is not what the code does.** `snapshot.rs` contains no
`Atomic*` type and no relaxed ordering; the table is fully immutable and is described as such in its
own module docs. This document describes the code.

---

## 9. Hot swap

```rust
pub struct SnapshotHandle { inner: Arc<ArcSwap<RoutingSnapshot>> }
```

- `new(initial)` wraps a freshly compiled snapshot.
- `current()` is the hot path: one atomic load plus a refcount increment. The contract is explicit — it
  performs **no** allocation and clones no routing data, and it must stay that way. A future change
  that wants to transform or filter here belongs in the compiler instead.
- `install(next)` publishes a newly compiled snapshot, visible to every worker immediately.
- `reload_from(path)` decodes a file, installs it, and returns its digest.

The conductor is the usual publisher, but not the only one: the relay also polls `config.json` (every
two seconds, keyed on mtime and length) and recompiles and installs a fresh table itself when the file
changed, so a hand-edit is picked up without waiting for the conductor.

**Readers never block.** `arc_swap::ArcSwap` stores a pointer; a reader loads it and increments a
refcount. There is no writer lock on the read side, so a hot swap cannot introduce a stall in a
request that is already running.

**In-flight requests complete against the old table.** A reader that has called `current()` holds an
`Arc` to the old snapshot, so the swap does not invalidate it; the request finishes with a consistent
table and drops its reference afterwards. Nothing is ever observed half-replaced.

**Memory behaviour is refcount-driven.** The old snapshot is freed when the last `Arc` to it is
dropped — normally just after the last in-flight request that started before the swap completes.
There is no copying of the routing table on swap, and no grace-period timer. An intended bound is
named in the configuration — `latency.snapshot_swap_budget_ms` (default 50) — but nothing in the relay
or the conductor currently reads it, so today the bound is only the refcount.

**A failed reload changes nothing.** `reload_from` returns `Err` and does not install, so a corrupt
file leaves the relay serving the previous table. This is the same degradation rule as a failed
compile: stale routing is recoverable, an outage is not.

The handle is `Clone` and shares one `ArcSwap`, so every worker in the process observes the same table
and sees an install at the same moment. `#[derive(Default)]` on `RoutingSnapshot` produces an empty
table with a zero digest; it is a placeholder for a handle that has not been given a compiled table
yet, not a routing decision — a `Default` snapshot resolves everything to `NotFound`.

---

## 10. Version and compatibility

`SNAPSHOT_MAGIC` is `*b"MROUTES1"` and `SNAPSHOT_VERSION` is `4`, both in
`crates/mr-core/src/lib.rs`.

The version history is worth recording, because Feature 3 spent two of it. Version 3 added the
per-step reasoning-level fields (`levels`, `default_level`, `reasoning_json`), `input_modalities` and
`pricing`, plus the proxy pool and the per-key limit table — but it declared the fields without
writing them, so a version-3 frame would decode with empty level tables. Version 4 completed the
feature: a step's level metadata is now actually encoded, and every lookup entry carries the level it
resolves to (`level` plus the `requested` it is compared against). A version-2 or version-3 snapshot
is therefore refused rather than read as though it carried levels it does not.

The rule is absolute: **a change to the compiled layout requires a `SNAPSHOT_VERSION` bump.** The
version is not a semver hint; the decoder has no compatibility paths, and a table written under a
different layout cannot be interpreted by reading it with the new field offsets. Bumping the version
is what makes "this file is not mine" a clean refusal instead of a misread.

On a version mismatch, `verify_header` returns an `Error::Snapshot` naming both the file's version and
the version this binary speaks. `decode` propagates it. `reload_from` does not install anything, so
the relay keeps serving its previous snapshot.

**Refusing is correct**, and it is worth spelling out why the alternative is worse. The alternative —
best-effort decoding of a version you do not understand — has three outcomes, all bad: you read the
right bytes at the wrong offsets and route a *different* provider's model; you read past the end of a
shortened structure and hit a bounds check that would have been a clean error at load, now on a
request; or you succeed and silently ignore a new field that was supposed to change routing. The
first is a correctness bug that looks like success. Meanwhile the cost of refusing is bounded and
temporary: the conductor is the only writer, it recompiles on demand, and the worst case is stale
routing for the interval until it does. The magic check rejects a plain non-snapshot file for the same
reason — a `config.json` or a database should never be interpretable as routing data.

`SNAPSHOT_MAGIC` and `SNAPSHOT_VERSION` are separate from `CONFIG_FORMAT_VERSION` (still 1) and
`SCHEMA_VERSION` (now 2). They version three different things — the authored file, the compiled
table, and the database — and conflating them would force an unnecessary migration in two of the
three whenever the third changed. The three are free to move independently, and now do.

---

## 11. Atomic write

`RoutingSnapshot::write_to` is a temp-file/`fsync`/rename sequence:

1. Encode the whole frame in memory first. A failure to encode means no file is touched.
2. Create the parent directory if needed.
3. Create a **sibling** temporary file — `.<name>.<pid>.tmp` in the same directory, so the rename is
   within one filesystem.
4. `write_all` the bytes.
5. `sync_all` — a `rename` that lands before the data does can leave a zero-length file after a power
   loss.
6. `rename` over the destination.
7. On any error, remove the temporary file and return an `Error::Snapshot` naming the destination path.

**Why a truncated snapshot is dangerous.** The relay's job at startup is to install one table. A
snapshot that is half-written and then decoded is either a decode error — the good case, because it is
loud — or, worse, a frame whose `payload_len` and checksum happen to be consistent with a shorter
table: a routing table with combos silently missing. That is why the write is atomic and why
`payload_len` is checked against the real byte count in both directions. A file that is either fully
old or fully new is the only acceptable state.

The error path is explicit rather than best-effort for the same reason: a failed write must not leave
a `.tmp` file behind that a later operator mistakes for state, and the caller must learn that the new
table is not published. `write_to_is_atomic_and_leaves_no_temporary_file` in `snapshot.rs` asserts
exactly that.

`Config::save` uses the same discipline on the authored file, and `docs/03-configuration.md` §12
describes it there.

---

## 12. What triggers a recompile

The trigger is a change to `Config::routing_digest()`. It is a `DefaultHasher` (SipHash) over
everything the compiler reads, and deliberately nothing else:

| Included | Why it is routing-relevant |
|---|---|
| Provider **array position** | It is the provider's priority order, so a drag-reorder in the panel must recompile. |
| Provider `id`, `enabled`, `key_strategy` | They change what exists and how an account is picked. |
| Account **array position**, `id`, `priority`, `weight`, `enabled` | They change the selection order the relay reads directly. |
| Account `models_allow`, `models_deny` | They change which account may serve a model. |
| Model `id`, `hidden` | The id is the lookup key, and a hidden model is dropped from the direct-lookup index. **The model's level metadata is not hashed** — see the note below. |
| Combo `id` and `signature()` | `signature()` folds name, strategy, the hedge fields, `sticky_success_window_secs`, `midstream_policy`, retry counts and each step's provider/model/enabled. (`weight` is not folded, matching the strategies that ignore it.) |
| Alias `from`, `to` | Alias resolution is compile-time. |
| Client key `id`, `enabled`, `allow_models`, `allow_routes` | They change authentication and authorization at the edge of routing. |
| `quota.enabled`, `quota.live_signals_win`, `quota.failures_before_dead` | They change when an account becomes unusable. |
| `latency.hedge_width`, `hedge_window_ms`, `hedge_max_requests` | **Options the relay ignores for now.** They are validated (a zero is refused) and hashed into the signature, but no combo inherits them: a hedge's width and window come from the combo's own `hedge` block, and `hedge_max_requests` caps nothing. They are part of the schema and change nothing; see `docs/03-configuration.md` §5. |

The list is hashed **field by field** rather than by deriving `Hash`, because both config blocks carry
a flattened `extra` map for unknown-key preservation and a map cannot be hashed. That restriction is
also the correctness win: only fields that change a routing decision are included, so a cosmetic edit
— a label, a note, a `sort_order` — does not republish the table.

**A gap Feature 3 left open, recorded here because it is load-bearing.** The per-model level metadata
that `compile` now reads — `reasoning_levels`, `default_reasoning_level`, `reasoning_wire`,
`input_modalities`, `pricing` — is **not** in the digest, and neither is
`routing.default_reasoning_level`. Editing any of them changes the compiled step (`levels`,
`default_level`, `reasoning_json`, `input_modalities`, `pricing`) without changing
`Config::routing_digest()`, so a conductor that skips a recompile on an unchanged digest will keep
serving the previous table until something else routing-relevant moves. The fields are compiled
correctly; they are simply not yet part of the trigger. Anyone touching this area should add them to
`routing_digest` rather than assume the table refreshed.

When the digest is unchanged, the conductor has nothing to do: the compiled table is identical, so
swapping the relay's pointer would cost a cache line and buy nothing. This is what the comment in
`routing_digest` means by "matters because a recompile swaps the relay's tables and briefly costs a
cache line".

What is deliberately *outside* the digest: account health, quota counters and usage totals. Those are
observed at request time — the relay keeps account health in memory and does not persist it, and no
usage writer exists yet — and they change continuously. Folding them into the digest would mean
recompiling the routing table whenever a request was counted. They change routing *decisions* without
changing the routing *table*, which is exactly the split this design rests on.

---

## 13. A worked example

Take a deliberately small configuration: two providers, one of them with two accounts, and one combo.

```json
{
  "providers": [
    {
      "id": "openai", "enabled": true, "key_strategy": "priority",
      "base_url": "https://api.openai.com/v1",
      "accounts": [
        { "id": "a1", "priority": 0, "enabled": true },
        { "id": "a2", "priority": 5, "enabled": false }
      ],
      "models": [ { "id": "gpt-5.5", "hidden": false }, { "id": "gpt-5.5-mini", "hidden": true } ]
    },
    {
      "id": "anthropic", "enabled": true, "key_strategy": "round-robin",
      "base_url": "https://api.anthropic.com/v1",
      "accounts": [ { "id": "b1", "priority": 0, "enabled": true } ],
      "models": [ { "id": "claude-sonnet-4-5", "hidden": false } ]
    }
  ],
  "combos": [
    {
      "id": "c1", "name": "Good", "enabled": true, "strategy": "priority",
      "steps": [
        { "provider": "openai",    "model": "gpt-5.5",           "enabled": true },
        { "provider": "anthropic", "model": "claude-sonnet-4-5", "enabled": true }
      ]
    }
  ],
  "aliases": [ { "from": "gpt-5", "to": "Good", "generated": false } ]
}
```

What `compile` produces, conceptually:

```text
digest          = routing_digest() over the fields in §12
built_at        = now

strings         ["openai", "https://api.openai.com/v1", "a1", "gpt-5.5",
                 "anthropic", "https://api.anthropic.com/v1", "b1",
                 "claude-sonnet-4-5", "Good", "gpt-5", ...]
                 (the reference variant of ids::fnv1a64, plus each id's secret-ref path)

providers[0]    id=openai dialect=openai key_strategy=priority
                accounts = [ a1(priority 0) ]          ← a2 dropped: disabled
                steps    = [ gpt-5.5 ]                 ← gpt-5.5-mini dropped: hidden
providers[1]    id=anthropic dialect=anthropic key_strategy=round-robin
                accounts = [ b1(priority 0) ]
                steps    = [ claude-sonnet-4-5 ]

combos[0]       id=c1 name="Good" strategy=priority
                steps = [ {openai, gpt-5.5}, {anthropic, claude-sonnet-4-5} ]
                hedge.enabled = false                  ← combo did not enable it

aliases[0]      from="gpt-5" to="Good" generated=false

index (by folded key, chain by rank)
  "gpt-5"                     → ALIAS   → aliases[0]
  "c1"                        → COMBO   → combos[0]          (rank 1)
  "good"                      → COMBO   → combos[0]          (rank 2)
  "openai/gpt-5.5"            → STEP    → providers[0].steps[0]
  "gpt-5.5"                   → STEP    → providers[0].steps[0]
  "anthropic/claude-sonnet-4-5" → STEP  → providers[1].steps[0]
  "claude-sonnet-4-5"         → STEP    → providers[1].steps[0]
```

Resolving the three interesting request names:

| Client sends | Lookup | `Resolution` | Relay does |
|---|---|---|---|
| `"Good"` | combo name, rank 2 | `Combo { plan: &combos[0], level: None }` | walks steps in order, starting at `openai/gpt-5.5`, each step at its own default level |
| `"gpt-5"` | alias, rank 0 | `Combo` after one hop to `"Good"` | same as above |
| `"openai/gpt-5.5"` | qualified, rank 3 | `Direct { step, level, substituted: false }` | calls OpenAI directly, bypassing the combo |
| `"gpt-5.5"` | bare, rank 4 | `Direct { step, .. }` | same, because no combo shadows the name |
| `"gpt-4o"` | miss | `NotFound { reason }` | 404 `model_not_found` carrying the reason; no upstream call |

Two things this example is meant to make concrete. First, the **account** `a2` is already gone from
the compiled table — the relay never evaluates `enabled` on the request path. Second, the **bare
model** `gpt-5.5` resolves directly even though the combo `Good` starts with the same model, because
the combo is named `Good` and therefore does not shadow it. Name the combo `gpt-5.5` instead and the
rank-2 entry beats the rank-4 one, so the same request now goes through the combo. That is the
shadowing mechanism from §6, and it is the cheapest way to add a fallback to a client you cannot
reconfigure.

The index in this example contains two further entries only when the models advertise levels: a
`gpt-5.5-high` key at rank 2 for a combo named `gpt-5.5`, and `openai/gpt-5.5-<level>` keys at rank 5
for each level OpenAI's `gpt-5.5` entry offers. Neither appears for the other model, which offers
none.

---

## See also

- [Architecture](01-architecture.md) — the two processes and the request path
- [Configuration](03-configuration.md) — the authored form the compiler reads
- [API reference](18-api-reference.md) — model addressing, reasoning levels and `/v1/models`
- [Key selection](05-key-selection.md) — what the relay does with an `AccountPlan`
- [Combos](06-combos.md) — what `ComboPlan` means to an operator
- [The hedged race](07-hedged-race.md) — the remapped participants and the eligibility gate
- [Latency engineering](16-latency-engineering.md) — why the lookup is the only work on the path
- [LLM_INDEX](LLM_INDEX.md) — the route table and invariant list
