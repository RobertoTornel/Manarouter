# The web panel

The panel is a small local web interface for the operator: providers, accounts, combos, keys,
aliases, quota, and the documentation. It runs on the **control plane**, port 20901 by default,
which is a different process and a different port from the relay on 20900.

That separation is the one architectural rule the panel must never break.

## Why it is a separate process

A request path and an admin page have incompatible cost profiles. The request path must do a
predictable, tiny amount of work per request; an admin page runs queries, renders lists, and is
allowed to be slow because a human is waiting for it.

Putting them in one process means they share an event loop, and a shared event loop means the slow
thing eventually blocks the fast thing. This is not hypothetical: the reference router measured in
[Latency benchmarks §1](19-latency-benchmarks.md#1-the-baseline-that-motivated-the-project) took
5.2–9.2 seconds to serve a purely local `/v1/models` request. An admin route mounted on the same
router was part of why.

So the panel cannot slow down a completion. The worst a broken panel can do is fail to render, and
the worst an admin query can do is fail to return — neither touches a token of anyone's inference.

The two processes share state through files, not through IPC: the conductor writes
`config.json` and the compiled snapshot, and the relay reads the snapshot. See
[Routing snapshot](04-routing-snapshot.md).

## Views

| View | Shows | Can change |
|---|---|---|
| **Overview** | Relay and conductor status, uptime, snapshot digest and age, configuration counts, whether a panel password is set, pending-restart state | Nothing |
| **Providers** | Every provider, its dialect, base URL, auth, key strategy, account count, and health | Add, edit, enable/disable, remove; add, enable/disable and remove accounts |
| **Models** | Discovered and hand-entered models per provider | Hide/show and delete a model; sync and hand-entry live in the Providers drawer |
| **Combos** | Every combo with its steps **in priority order** | Create, edit, reorder steps, enable/disable, remove |
| **Keys** | Client keys by label and prefix | Create (plaintext shown once), edit the policy, rotate the secret in place (new plaintext shown once, old secret invalidated after commit), enable/disable, delete |
| **Aliases** | Alias to target mappings | Add, remove |
| **Quota** | Per-account windows, cooldowns, reset instants, and usage over time | Nothing (windows are declared in the configuration) |
| **Settings** | Ports, the relay connection pool (including a low-resource profile), the panel password, and the raw configuration | Edit; **ports apply on next restart** |
| **Documentation** | The top-level files in `docs/`, shown as markdown source | Nothing |

The **Models** view distinguishes discovered from hand-entered metadata explicitly, because a sync
must never overwrite what a human typed. If the two ever disagreed and the panel did not say which
was which, the operator would have no way to know why their edit vanished.

## The design language

The whole panel follows one rule that is more useful than any palette:

> **Greys carry all the structure. A single accent is used for emphasis and interaction.
> The accent is never used for destructive actions or errors.**

So borders, headings, tables, dividers, disabled controls and secondary text are all greys. The
accent appears on the focused control, the selected tab, and the primary action — the things a
person is meant to look at or click. Nothing else.

Semantic colours handle meaning, and they are a separate vocabulary from the accent:

| Colour | Hex | Means |
|---|---|---|
| Green | `#4cc38a` | Healthy, ready, enabled |
| Amber | `#d9a441` | Degraded, cooling down, action needed |
| Red | `#ef6a6a` | Broken, refused, destructive |

Two consequences of taking that seriously:

**The accent is never red.** A destructive button does not become "the important one" by being
painted the emphasis colour; it is red, and it is still secondary to the primary action. Painting a
"Delete" button the same colour as "Save" is how interfaces get misclicked, and no amount of
confirmation dialog makes it acceptable.

**A semantic colour is never decorative.** Green in this panel always means a healthy state and never
"this looks nice". The moment green is used for styling, the operator can no longer scan for green
and find health.

### The case that catches people out

A provider whose accounts are all deliberately disabled is **`Unconfigured`, not `Dead`**.

This is a real distinction in the code — `ProviderHealth::from_counts(total, usable)` returns
`Unconfigured` when `total == 0` — and it has to survive to the screen. An operator who disabled
three accounts on purpose while changing providers should not see a red failure indicator for a
decision they made. Red is reserved for things that are wrong, and this is not wrong; it is parked.

Getting this wrong is not cosmetic. An operator who learns to ignore red because it is usually
crying wolf will also ignore the red that matters.

## Ports, restart, and the amber banner

The panel can change the relay and conductor ports, and a port change is **persisted but not
applied**. It is labelled "next restart", an amber banner appears, and the banner stays until the
restart happens.

The banner is not dismissible at all. It stays until the restart happens, because the cost of
forgetting that a change is pending is a confusing failure later ("I set the port, why is it still on
the old one?").

A **Restart** button applies it. The restart is never automatic, because rebinding a listening socket
means the old one closes, and everything connected to it — including an in-flight completion —
is dropped. An operator expects a settings change to take effect; they do not expect it to kill a
request that is mid-answer. Making the restart explicit turns a surprise into a decision.

## Authentication

- Bound to `127.0.0.1` by default, which requires no password.
- A non-loopback bind **without a password is refused at startup**, with a message saying how to set
  one. It does not warn and continue (the one way past it is the explicit
  `control.allow_remote_without_password` opt-out): a panel that silently exposes provider credentials
  and client keys to the network because the operator did not read a log line is not a defensible
  default.
- Passwords are stored as **Argon2id** hashes. Session lookup is constant-time, so a timing
  difference cannot be used to probe a token.
- Every mutating request must carry an acceptable `Origin`, which is checked before the handler runs.
  A cross-origin form post from a page the operator is merely visiting is the realistic attack on
  a loopback admin interface, and browsers will send it.
- Sessions last 12 hours. Logging out revokes that session; changing the password revokes all of
  them.

The full threat model is in [Security](21-security.md). Cookie-based authentication against model
providers is **never** supported, for any provider, and the panel does not offer it.

## Resource discipline

The panel polls for health and status. It **stops polling while the tab is hidden**, and resumes on
focus.

This is a small thing that says something about the project. A background tab left open for a week
making a request every few seconds is thousands of pointless requests — to a loopback socket, on the
same machine that is serving inference. It is exactly the kind of waste the router exists to remove,
and it would be strange for the router's own panel to be the thing doing it.

The same reasoning drives the rest of the panel: it holds no more history than it displays, it does
no work while no one is looking, and it starts no timers it cannot stop.

## Implementation constraints

The panel is **plain HTML, CSS and ES modules**, served as static files by the conductor. There is no
framework, no build step, and no CDN.

Each of those is a choice with a cost:

- **No framework** means the views are small hand-written modules. It also means there is no
  dependency tree to audit, and no supply-chain risk in a component that handles provider
  credentials.
- **No build step** means editing the panel is edit-and-reload. It also means the panel cannot use
  JSX, TypeScript, or single-file components — a real limitation, accepted because the panel is
  small enough not to need them.
- **No CDN** means the panel works with no internet access, which is the normal state of a machine
  running a local router. A panel that fails to load its own stylesheet because the network is down
  would be an obvious own goal.

The panel is served from the `panel/` directory on disk, located once at startup beside the working
directory or the executable, so there is no build step, no `node_modules`, and no version skew against
the API it talks to. A missing `panel/` directory degrades to a placeholder page rather than a crash.

## See also

- [Security](21-security.md) — the threat model, sessions, and the origin check.
- [Configuration](03-configuration.md) — what the panel is actually editing.
- [Combos](06-combos.md) — the view an operator uses most.
- [Latency benchmarks](19-latency-benchmarks.md) — why the panel is not on the request path.
- [Troubleshooting](20-troubleshooting.md) — what to check when the panel will not load.
