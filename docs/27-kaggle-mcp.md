# 27 — Kaggle's hosted MCP server, probed

Everything below was measured against the live endpoint on 2026-11-12 from this machine, with
`curl`. Nothing here is taken from a search snippet: where a claim comes from a page instead of a
probe, the page is named. The reference implementation is the endpoint itself; Kaggle documents it at
<https://www.kaggle.com/docs/mcp> and announced it at
<https://www.kaggle.com/blog/kaggles-official-mcp-server>.

## 1. What it is

`https://www.kaggle.com/mcp` answers as an MCP server over streamable HTTP. Its own `initialize`
reply names it:

```json
{"result":{"protocolVersion":"2025-06-18",
 "capabilities":{"logging":{},"prompts":{},"resources":{},"tools":{}},
 "serverInfo":{"name":"Kaggle.Web","version":"1.0.0.0"}},"id":1,"jsonrpc":"2.0"}
```

## 2. How to talk to it

One JSON-RPC message per POST. Responses arrive as Server-Sent Events, so the body to parse is the
line after `data:`, not a bare JSON document.

```bash
UA='Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36'
: "${KGAT:?set KGAT to a Kaggle API token before running this}"

# handshake, then the tool list
curl -sS -X POST https://www.kaggle.com/mcp \
  -H "User-Agent: $UA" -H "Authorization: Bearer $KGAT" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"manarouter","version":"0.2.4"}}}'

curl -sS -X POST https://www.kaggle.com/mcp \
  -H "User-Agent: $UA" -H "Authorization: Bearer $KGAT" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
```

Three properties a client has to know, each of them observed rather than assumed:

* **The server is stateless.** No `Mcp-Session-Id` header is issued, and
  `notifications/initialized` is refused (`-32601`, method not available). The working sequence is
  `initialize` → `tools/list` → `tools/call`, each an independent POST.
* **A default `curl` User-Agent gets `404` with an empty body** — for `/mcp` and for `/api/v1/...`
  alike. The same request with a browser User-Agent returns `200`. A probe that forgets the header
  concludes, wrongly, that the endpoint does not exist; this is written down because it cost time
  here.
* **Authentication failure is a result, not a JSON-RPC error.** A tool that needs a signed-in user
  answers `200` with
  `{"result":{"content":[{"type":"text","text":"Unauthenticated"}],"isError":true}`. A client that
  only checks the HTTP status reads that as success.

## 3. Credentials

`Authorization: Bearer <token>` works without a browser, where the token is a Kaggle API token from
<https://www.kaggle.com/settings> with the `KGAT_` prefix. Proven, not inferred:
`tools/call` of `get_accelerator_quota` with the bearer returned live quota, and the same call with
no credential returned `Unauthenticated`. Kaggle's docs describe OAuth 2.0 as well; OAuth is not
needed for the headless case.

Two scopes coexist: public reads work with no credential at all (`search_datasets` answered
anonymously), while anything about *your* account needs the bearer.

## 4. What it exposes

`tools/list` returned **76 tools**, covering competitions, datasets, notebooks, models, forums,
hackathons, benchmarks and simulation, plus search across all of them. The ones a rental lane would
care about:

| Tool | What it is for |
|---|---|
| `get_accelerator_quota` | the account's GPU and TPU budget: used, reserved, allowed, and when it refreshes |
| `create_notebook_session` | start a notebook session (language, kernel type, machine shape, internet toggle…) |
| `get_notebook_session_status` | whether a session is running, completed or failed |
| `cancel_notebook_session` | stop one |
| `save_notebook` | save and run a notebook top to bottom |
| `get_notebook_info` | notebook metadata, source included |
| `list_notebook_files` / `download_notebook_output` | notebook contents and results |

The quota call is worth quoting exactly, because it is the number the Kaggle lane wants to report, and
it comes back without any scraping:

```json
{"quota_refresh_time":"2026-11-14T00:00:00Z",
 "gpu_quota":{"time_used":"0s","time_reserved":"0s","total_time_allowed":"108000s","minimum_time_allowed":"108000s"},
 "tpu_quota":{"time_used":"0s","time_reserved":"0s","total_time_allowed":"108000s","minimum_time_allowed":"108000s"}}
```

`108000 s` is 30 h. The refresh instant is a real timestamp, so a reset-aware lane can carry it
instead of guessing.

Two cautions before anybody turns that into a constant.

First, **the same figure comes back for both accelerator families.** `tpu_quota` reports 108 000 s
exactly as `gpu_quota` does, while the published TPU figures are lower — in the region of 20 TPU hours
a week, with sessions a little under nine hours on `v5litepod-8` (secondary sources, and §9 below).
`https://www.kaggle.com/docs/tpu` is client-rendered — a fetch of it measured earlier returned an HTTP
200 shell of about 5.5 KB containing no occurrence of "hour" — so the published page could not be read
first-hand here, and this document does not resolve which figure is current.
[26-gpu-lanes.md](26-gpu-lanes.md) §3 records the same split.

Second, **the period is not stated.** `total_time_allowed` is what this account is actually granted
*now*, next to a `quota_refresh_time`; the reply does not say the allowance is a per-week rate, and
the plan and the accelerator family both move it.

The rule that follows: **read the API, never hardcode a ceiling.** A lane that reports a budget reads
`total_time_allowed` and `time_used` at submission time and reports the refresh instant beside them.
Writing 30 h, or 20 h, or 9 h into the lane as a constant makes it wrong the moment the account, the
accelerator or the plan changes — and wrong in the direction that burns quota.

## 5. Why this matters here

The Kaggle lane currently reports `kaggle_cli_present: false` and `credential_configured: false` on
this host (see [26-gpu-lanes.md](26-gpu-lanes.md) §3, and the sweep recorded in its §11): driving
Kaggle was assumed to need the `kaggle` CLI, which is not installed. MCP removes that assumption — it
is plain HTTPS, one token, no binary, and it exposes exactly the session verbs a lane needs (create,
status, cancel) together with the accelerator budget and its refresh instant. Whether to move the lane
onto it is a product decision, and this document is the research for it: **nothing in the lane has
been changed to use MCP.**

## 6. A community pattern for vLLM on Kaggle, reviewed

[`aki-008/vLLM-kaggle`](https://github.com/aki-008/vLLM-kaggle) (last commit 2025-07-10) runs a vLLM
OpenAI-compatible server inside a Kaggle Notebook and publishes it through **ngrok** (`pyngrok`,
`ngrok.connect(1234)`), so a client calls `https://<id>.ngrok-free.app/v1`. Useful as evidence that
the shape works; not usable as-is:

* it needs an **ngrok account and auth token** — a third-party dependency Kaggle does not provide;
* it **commits a live ngrok authtoken into a public repository** (the README warns about exposure,
  the code does the opposite);
* it installs `vllm` unpinned, and pairs `--tensor-parallel-size 2` with a single-`nvidiaTeslaT4`
  notebook accelerator;
* it says nothing about Kaggle's session limit, idle timeout or restart, and states no Kaggle
  Terms-of-Service position on running a persistent public server.

Our own Kaggle lane (§3 of [26-gpu-lanes.md](26-gpu-lanes.md)) is batch-only, written around a 12 h
session bound (`deadline_secs`) and carrying no quota constant at all — it does not read the account
budget today, and its status route reports no quota figure. A route that does report one must read it
from `get_accelerator_quota` (§4) rather than assume either published ceiling, and the lane does not
assume ingress at all.

## 7. Where a scheduled run gets its credentials — Kaggle Secrets, and why not to rely on them

The pattern in §6 fails one requirement badly: a notebook started *by the router* still needs a token
(an HF token for a gated repo, an ingress token for a tunnel). Kaggle's answer to that is
**Secrets** — the mechanism described in
[Kaggle secrets 🤫](https://www.kaggle.com/discussions/getting-started/450378) (Harsh Jadhav,
2023-10-24): *Add-ons → 🔑 Secrets* in the notebook editor, then read the value at runtime with

```python
from kaggle_secrets import UserSecretsClient
value = UserSecretsClient().get_secret("MY_API_KEY")
```

Facts checked against Kaggle's own CLI documentation
([kaggle-api `docs/kernels.md`](https://github.com/Kaggle/kaggle-api/blob/main/docs/kernels.md)):

* secrets are **defined on kaggle.com only — "no CLI support"**; there is no `secrets` field in kernel
  metadata and no CLI flag, so a pushed notebook cannot *create* one;
* `kaggle_secrets` is pre-installed and works **only inside Kaggle's execution environment**;
* secrets are per user and per notebook ("attached"), which is why a token never has to appear in the
  notebook source — the contrast with §6, where the ngrok authtoken is committed to a public repo.

The part that matters for a router-controlled lane is an **open bug**:
[`[Bug] UserSecretsClient.get_secret returns HTTP 400 in API-triggered script kernels`](https://www.kaggle.com/discussions/product-feedback/755687)
(product-feedback, last activity 2026-11-04). The reporter's kernel has `HF_TOKEN` and
`KAGGLE_API_TOKEN` attached under the exact labels; an interactive UI run reads them fine, and an
**API/CLI-triggered** run of the *same kernel* raises `ConnectionError` with HTTP 400 for both
labels, reproducibly and not as a propagation delay. No fix was posted in the thread.

Consequences, decided here so the lane does not have to guess:

1. **Never carry a credential in notebook source or in kernel metadata.** Kaggle metadata has nowhere
   to put it, and a lane that writes a token into the notebook it pushes is committing a secret to a
   revision history.
2. **Do not depend on Secrets for a router-triggered run.** Until 755687 is resolved, a scheduled run
   must be assumed unable to read them. The lane's own MCP surface agrees: neither
   `create_notebook_session` nor `save_notebook` has any secret- or environment-carrying field (both
   schemas were read directly; no `secret*`/`env*` property exists).
3. **Therefore the Kaggle lane stays on public, non-gated models.** A gated repo is not something the
   lane can satisfy by itself; if one is requested, the lane should say so plainly rather than start a
   session that will fail, and the fallback is an operator-run interactive session, which is outside
   the lane's remit.
4. If Kaggle later fixes 755687, the wiring is small: the operator defines the label once in the UI,
   the notebook reads it with `UserSecretsClient`, and nothing about the lane's lifecycle changes.
   This is recorded as a **warning-worthy limitation**, not a hard block on the lane, in keeping with
   the project's advisory-not-fatal policy.

## 8. Reading a forum topic through MCP

The discussion pages themselves are client-rendered — `curl` returns a 7 KB shell whose only text is
the page title — so the topic has to be read through the API. `get_forum_topic` does it, and it needs
its arguments **nested under `request`**, which `tools/list` does not make obvious:

```bash
curl -sS -X POST https://www.kaggle.com/mcp -H "User-Agent: $UA" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_forum_topic",
      "arguments":{"request":{"forumTopicId":450378,"includeComments":true,
      "hasInitialPageSize":true,"initialPageSize":60}}}}'
```

The flat form (`{"topic_id":450378}`) answers `isError: true` with the unhelpful text
`An error occurred invoking 'get_forum_topic'.`; the nested form returned the topic, its comments and
their replies **anonymously**, with no token. The same nesting applies to `list_topic_messages`
(`{"request":{"topicId":…}}`). A flat call to the wrong endpoint shape is also what produced the
earlier `HTTP 406 Not Acceptable`: the `Accept: application/json, text/event-stream` header is
required, not optional.

## 9. Serving a model from Kaggle's *own* TPU session — `ARahim3/kaggle-tpu-lab`, reviewed

Every pattern above is about *submitting work* to Kaggle. [`ARahim3/kaggle-tpu-lab`](https://github.com/ARahim3/kaggle-tpu-lab)
(MIT, ~795 stars) is about the opposite direction: publishing an OpenAI-compatible endpoint from a
Kaggle notebook's own accelerator, on Kaggle's **free** TPU quota. It is the most complete
implementation of that idea found so far, and it is worth reading before anyone repeats the attempt.

What it does, in its own terms:

* **Two paths, one server.** (a) run a notebook on a free `v5litepod-8` (eight TPU chips) whose *last
  cell is the server* — `vllm-tpu` plus a local patch, weights pulled from a public dataset; the cell
  prints a URL and an API key and then holds the session; (b) `gcp/` runs the same server on a rented
  TPU VM (`v5litepod-8` or `v6e-8`), which is faster and has "no queue, no nine-hour cap", reaches the
  outside through `gcloud compute tpus tpu-vm ssh ... -N -L 8000:localhost:8000`, and bills while it
  exists (spot instances were preempted "several times a day").
* **Both surfaces.** The server answers OpenAI `/v1/chat/completions` **and** Anthropic `/v1/messages`,
  which is why the README's client wiring sets `ANTHROPIC_BASE_URL` to the tunnel plus
  `ANTHROPIC_AUTH_TOKEN` (not `ANTHROPIC_API_KEY`) and a large context window to match the model.
* **Ingress is a Cloudflare quick tunnel** (`cloudflared --url http://127.0.0.1:8000`, downloaded at
  runtime from GitHub `latest`, three attempts) yielding `https://<name>.trycloudflare.com`, with 15 s
  keep-alive pings because "Cloudflare closes a response that stays silent for ~100 s". If the tunnel
  fails, the server is reachable only inside the kernel.
* **Its own quota discipline.** The key is generated in-session and printed once, never committed; a
  `keepalive_min` self-shutdown guard (480 min by default, 90 min in the notebook) stops a forgotten
  run from burning the allowance.
* **It restates a session figure (secondary, quoted from its README, not from Kaggle):** sessions run
  "to Kaggle's limit, a little under nine hours" on the free TPU path, after which a re-run yields a
  new URL — that, not the 30 h in the quota API, is the number to plan a *session* around. Neither
  figure is settled: §4 records what the API actually reported and what the published page would have
  to be read to confirm.

What is worth borrowing, and what is not:

1. **Borrow** the in-session self-shutdown guard and the once-printed, in-session-generated key. Both
   fit our lane's design directly (a run must never outlive its usefulness).
2. **Borrow** the keep-alive cadence as a *fact*: any ingress we use has to tolerate an idle period it
   does not control, and a lane that says nothing about idle teardown will report a tunnel as live
   after it has silently died.
3. **Do not borrow** the unpinned `cloudflared` download from GitHub `latest`, or unauthenticated
   ingress generally: our own rule is that a public surface is deliberate, authenticated and
   per-tenant, not a convenience.
4. **Do not borrow** the licensing posture: its draft model is CC BY-NC-ND 4.0 ("research use only"),
   which cannot be shipped as part of a product. Check the licence of any weights a lane serves.
5. **Do not borrow** free-tier Kaggle quota marketed as a persistent endpoint. A nine-hour-at-best,
   queue-backed, quota-metered session is a *rental*, which is exactly how our lane already models it.

**Consequence for our lane: unchanged.** It stays batch-only (§3 of
[26-gpu-lanes.md](26-gpu-lanes.md)) and gains one requirement from this review: if a session-based
route is ever added, the session's own quota and idle behaviour must be read from the platform
(`get_accelerator_quota`, §4) rather than assumed from either published ceiling, and its ingress must
be ours, not a third party's quick tunnel.
