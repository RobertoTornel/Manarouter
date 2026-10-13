# Sync design (v2)

> **v1 does not implement this.** There is no sync code in this repository, no peer protocol, and no
> transport. Nothing in this document describes behaviour you can rely on today.

It is written anyway, for a specific reason: **v1's schema choices only make sense in the light of
it.** A reader who finds an unused `revision` column, a `deleted_at` on every table, and an `oplog`
that nothing reads deserves to know why they are there, and a future implementer deserves a starting
point rather than a blank page.

## The problem

An operator runs Manarouter on more than one machine. They change a combo on their laptop and expect
to see it on the desktop. Today they export and import by hand, or they keep two configurations in
their head and get them wrong.

## Why it is v2 and not v1

This is a cost statement, not a dodge. A correct implementation needs:

- **Conflict resolution**, which means deciding what happens when the same combo was edited on two
  machines while both were offline.
- **A transport**, with authentication between peers and a story for peers that are unreachable.
- **Credential handling**, which is the hard part: the thing most worth syncing is the thing most
  dangerous to sync.
- **Version skew**, because a peer will run an older build.

Doing all four badly is worse than not doing them. A sync layer that silently loses a combo, or
helpfully uploads a provider key to a machine the operator did not intend, is a serious harm; an
absent feature is an inconvenience. So the feature ships when it can be done properly, and in the
meantime v1 pays the small cost of writing columns nothing reads.

## The groundwork v1 builds

Each of these is cheap now and expensive later.

### UUIDv7 identifiers

Every entity id is a UUIDv7: unique without coordination, and sortable by creation time. A
coordinating server assigning ids would be a single point of failure for a feature that is supposed
to work between machines that are frequently offline.

### A monotonic `revision` per record

Every row carries a `revision` that increments on every write. It is what lets a merge distinguish
"changed" from "unchanged" without comparing payloads, and it is what makes "this side is newer" a
decidable question rather than a guess at timestamps.

### `deleted_at` tombstones instead of hard deletes

**This is the most important choice in the list.**

With a hard delete, two situations are indistinguishable:

- The record was deleted on this machine, and the peer should delete it too.
- The peer has simply never heard of this record.

Replicating the first as the second means every peer resurrects every deletion — the deleted combo
comes back on the next round trip, forever. There is no way to fix this after the fact without a
full resynchronisation.

A tombstone distinguishes them: the row is present with `deleted_at` set, the peer sees an explicit
deletion, and the record stays dead. The cost is that deleted rows accumulate, which is what the
retention window is for.

### Secrets addressed by id, never inlined

`config.json` never contains a credential — only an id, with the value in the encrypted vault. That
means a sync layer can decide to send ciphertext, send a wrapped key, or send nothing at all, without
touching any other table. If credentials were inlined, that decision would be spread across every
entity that references one.

### All writes through one repository layer

Every mutation goes through the same place, so a replication log has **exactly one choke point**.
Writing the log becomes a matter of adding one call there, rather than finding every `INSERT` in the
codebase. This is the reason `store.rs` exists as a layer rather than raw queries at the call sites.

### An `oplog` table, written from day one

```
oplog(entity, id, revision, op, updated_at, payload_hash)
```

It is written on every mutation and read only for a "pending changes" count. Nothing consumes it
today.

Writing it from day one is what avoids a painful backfill. The alternative is to add replication in
v2 and discover that there is no history to replicate from — which means either syncing the full
state on every connect (fine for ten rows, not for a year of usage records) or accepting that
anything changed before the upgrade never syncs.

### Unknown-key preservation

The configuration format preserves fields it does not recognise
([Configuration §forward compatibility](03-configuration.md)). That is what stops an older instance
from destroying a newer instance's settings during a round trip — without it, a downgrade is a silent
data-loss event.

## Transport

Intended shape: **configurable peers, an authenticated channel, pull-then-push.**

```
peer A                                      peer B
  |  GET  /api/sync/changes?since=<rev>  ->   |
  |  <-  records with revision > <rev>        |
  |  apply locally (merge)                     |
  |  POST /api/sync/changes               ->   |
  |      records with revision > B's watermark |
  |                                            |  apply locally (merge)
  |  <-  200 with the new watermark            |
```

Pull before push so that the merge happens against the peer's latest state rather than a stale view.

### Rejected alternatives

| Option | Why not |
|---|---|
| **A shared SQLite file over a network filesystem** | Network filesystem locking is unreliable, and SQLite over NFS corrupts databases. The failure mode is losing the whole configuration, which is not a trade worth making for the convenience of avoiding a sync protocol. |
| **A message broker** | A new dependency, a new service to run and secure, and a new failure mode — for a feature that most installs will not use. A self-hosted router that requires RabbitMQ to replicate three combos has misjudged its audience. |
| **Gossip / CRDT** | Genuinely good at scale, and the wrong tool here. A self-hosted router has two to five instances, edits are rare, and a CRDT's convergence properties are unnecessary. The complexity would be paid continuously for a benefit that appears only in a scenario this software does not have. |

## Conflict resolution

Stated precisely, because "last writer wins" is used loosely and the difference matters:

| Entity | Granularity | Why |
|---|---|---|
| Settings (ports, timeouts, quota policy) | **Per field** | They are independent scalars. Taking a newer port from one peer and a newer timeout from another is coherent and loses nothing. |
| Credentials | **Per record** | A credential is one opaque value. A field-level merge has nothing to merge. |
| Combos and aliases | **Per record** | A combo is an **ordered list**, and the order is the routing policy. |

The combo case deserves the explanation. If two peers reorder the same combo, a field-by-field merge
of the `steps` array produces an interleaving that **neither operator chose**. The result is a valid
configuration, a valid routing order, and not what either person asked for — a wrong answer that is
much harder to notice than a conflict would have been.

So combos merge per record, the loser is retained in the oplog, and the panel reports the conflict.
Nothing is silently lost; the operator decides.

## Credentials

**Plaintext is never synced.** Three options were considered:

| Option | What it means | Cost |
|---|---|---|
| **Send nothing** (default) | Each instance holds its own credentials; the operator authenticates once per machine | Repetitive for OAuth platforms |
| **Send ciphertext, share the vault key** | Credentials replicate encrypted | Every peer holds the key that opens every credential, so one compromised peer compromises all |
| **Send a re-wrapped key per peer** | Each peer gets the credential encrypted to its own key | A real key-management protocol, and the only option that survives one peer being compromised |

The default is **send nothing**, and it is the right default because it is the only one with no new
blast radius. An operator who wants credentials on both machines can authenticate twice; an operator
who has been compromised by a sync feature cannot un-compromise the second machine.

## What is never synced

- **Usage and quota observations.** They are facts about *this* instance's network path and its
  provider relationship. A rate limit observed on one machine is not a rate limit on another, and
  merging them would produce a cooldown that has no basis in either machine's reality.
- **Account health.** Same reasoning: it is per-instance observed state, not shared configuration.
- **Sessions.** A login on one instance is not a login on another.

The dividing line is consistent: **configuration is shared, observation is not.**

## Failure modes

| Situation | Expected behaviour |
|---|---|
| A peer is offline for a week | Edits accumulate in the oplog. On reconnect, the peer pulls everything since its watermark and merges. Nothing is lost, and nothing syncs until then. |
| Clock skew between peers | `revision` decides, not wall-clock time. Timestamps are for display. A peer with a wrong clock can mislabel *when* something happened but cannot win a merge it should lose. |
| A peer runs an older version | Unknown fields are preserved, so the older peer's round trip does not destroy them. If the older peer does not know about an entity type, it passes the records through untouched. |
| Two peers edit the same combo at once | One wins per record; the loser is retained in the oplog and reported as a conflict. The operator resolves it. |
| A tombstone arrives for a record edited recently locally | The tombstone carries a revision. If the local edit is **newer**, the tombstone loses and the local record survives — otherwise a deletion made a week ago would silently undo today's work. |
| A peer never returns | Its records stay. There is no automatic expiry, because "unreachable" and "deleted" are not the same thing and must never be conflated. |

## What would have to be true to build this

The concrete remaining work, so a future implementer starts from something:

1. **A peer table and a watermark per peer** in `state.sqlite` — not `config.json`, because it is
   observed state.
2. **`oplog` reads**, not just writes: a query for records with `revision > watermark`, and a way to
   compact it once every peer has passed a revision.
3. **An authenticated peer channel.** Peer identity, a shared secret or a key pair, and a refusal
   path. This is the security-critical piece and deserves its own review.
4. **A merge function per entity**, with the granularities this document specifies.
5. **Conflict reporting in the panel**, because a conflict the operator never sees is a silent data
   loss with extra steps.
6. **Tombstone retention**, so the oplog and the tombstone set do not grow without bound.
7. **A load test between two real instances**, because every interesting bug here appears only with
   two writers.

## See also

- [Configuration](03-configuration.md) — unknown-key preservation and the format version.
- [Architecture](01-architecture.md) — why there are two processes and one store.
- [Security](21-security.md) — the vault, and why plaintext credentials are never replicated.
- [Quota tracking](08-quota-tracking.md) — an example of observation that is deliberately not shared.
