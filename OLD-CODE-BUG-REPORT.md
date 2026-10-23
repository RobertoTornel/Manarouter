# Old-code defect report — Manarouter baseline (commit e8119cb, branch main)

Author: coordinator (Claude/DeepSeek harness) — written from direct source reading and live probing of
the running router, plus corroboration from the 200+ per-file reviews.
Date: 2026-10-23. Copy under test: `/mnt/pool/2-nvme1tb/testing` (branch `testing`), taken from
`/home/mana/Documents/seek/Router` at ~16:31.

Severity: **critical** = remote compromise or data loss without credentials; **high** = failure,
credential loss, auth weakness or corruption reachable in normal operation; **medium** = DoS,
mis-accounting, wrong protocol behaviour, or a hardening gap that needs a second mistake;
**low** = hardening/robustness.

Status: `fixed-in-copy` means a per-file reviewer changed the file in the testing copy and the
workspace compiles; `open` means it needs a cross-file or product decision and is documented here.

---

## 1. Confirmed defects

| # | Sev | File | Defect | Status |
|---|-----|------|--------|--------|
| 1 | high | `crates/manarouter-relay/src/server.rs:1869` | `read_body` collected the entire body before testing `MAX_BODY_BYTES` — the 32 MiB cap did not exist while bytes arrived | fixed-in-copy |
| 2 | high | `crates/manarouter-conductor/src/auth.rs:322` | Origin check accepted an origin with no port for *any* loopback port and ignored the scheme | fixed-in-copy |
| 3 | high | `crates/manarouter-relay/src/proxy.rs` (SOCKS5 CONNECT reply) | domain-typed BND.ADDR skip was `1+len+2` after the length octet was already consumed — ate one byte of the tunnel (TLS ClientHello) | fixed-in-copy |
| 4 | high | `crates/mr-core/src/secret.rs` | `open()` minted a fresh `vault.key` when only `vault.enc` existed → silent loss of every credential | fixed-in-copy |
| 5 | high | `crates/mr-core/src/secret.rs` | `rekey()` swapped the cipher before decrypting; any failure left an undecryptable mix that Drop flushed | fixed-in-copy |
| 6 | high | `crates/mr-core/src/secret.rs` | fixed temp name `vault.enc.tmp` shared by relay and conductor → a concurrent flush could publish a partial vault | fixed-in-copy |
| 7 | high | `crates/manarouter-relay/src/state.rs` | `reload()` rebuilt the route table but not the client-key ring → a revoked/disabled key kept authenticating until restart | fixed-in-copy |
| 8 | high | `crates/manarouter-relay/src/state.rs` | an empty key ring meant "open" even when the config declared keys whose hashes failed to parse | fixed-in-copy |
| 9 | high | `crates/mr-core/src/model.rs:296` | `Account::allows_model` deny-list ignored prefix globs (`claude-*` blocked nothing) — policy bypass | fixed-in-copy |
| 10 | high | `crates/mr-dialects/src/openai.rs` | `[DONE]` emitted a second `message_delta` with hard-coded `end_turn`, overwriting tool-use/truncation stop reasons | fixed-in-copy |
| 11 | high | `crates/mr-core/src/oauth/flow.rs` (url_decode) | `&str` sliced at byte offsets → panic on `%aé` from the loopback callback | fixed in the stale copy; re-applied after re-sync |
| 12 | high | `crates/mr-dialects/src/sse.rs` | `next_frame`/take_tail consumed a frame then returned `None` on invalid UTF-8 → silently dropped frames and raw bytes to the client | fixed-in-copy |
| 13 | high | `crates/manarouter-relay/src/recovery.rs` | tool-call guard kept only the last ≤16 bytes of the *current* chunk → a marker split over 3 chunks was missed, splicing two models' tool calls | fixed-in-copy |
| 14 | high | `crates/mr-cli/src/main.rs` | `--config-dir` set the config dir but not the state dir → a second instance overwrote the live install's `routes.snapshot` | fixed-in-copy |
| 15 | high | `crates/mr-cli/src/provider.rs` | `provider test` printed query-param API keys verbatim in the table/JSON and in four error strings | fixed-in-copy |
| 16 | high | `crates/mr-cli/src/account.rs` | `account remove` deleted the vault secret *before* the config write was validated → credential destroyed, config still listing the account | fixed-in-copy |
| 17 | high | `crates/mr-core/src/usage.rs` | `TokenUsage::total` added provider-supplied u64s → debug panic / release wrap-to-zero (free request) | fixed-in-copy |
| 18 | med | `crates/manarouter-relay/src/select.rs` | `earliest_reset` advertised a cooldown that can never heal (AuthFailed) as a recoverable Retry-After | fixed-in-copy |
| 19 | med | `crates/manarouter-relay/src/quota.rs` | `InFlight::by_key` never dropped entries and admit created an entry before the budget check → growth for every key ever seen, O(all keys) per settlement | fixed-in-copy |
| 20 | med | `crates/mr-core/src/store.rs` | `prune_key_usage` left the FK-less `usage_tokens` rows behind → unbounded growth | fixed-in-copy |
| 21 | med | `crates/mr-core/src/config.rs` | `routing_digest` omitted `routing` → panel edits to modality fallbacks and the default reasoning level never reached the data plane | fixed-in-copy |
| 22 | med | `crates/mr-core/src/ids.rs:169` | `remaining_ms_until` raw subtraction → overflow panic/wrap, making refresh/quota deadlines never fire | fixed-in-copy |
| 23 | med | `crates/mr-core/src/quota/http.rs` | quota responses were buffered with no size cap (OOM from a hostile/broken endpoint) | fixed-in-copy (8 MiB `Limited`) |
| 24 | med | `crates/mr-core/src/quota/antigravity.rs` | a fraction-less live quota bucket overrode a reported catalog quota, flipping the aggregate to "unknown" | fixed-in-copy |
| 25 | med | `crates/mr-core/src/quota/moonshot.rs` | `supports()` checked the account base URL while `fetch()` checked the provider URL → kimi balances never loaded | fixed-in-copy |
| 26 | med | `crates/mr-core/src/oauth/codex.rs` | device flow dropped `id_token`; 404 was treated as "pending" forever | fixed-in-copy |
| 27 | med | `crates/mr-dialects/src/anthropic.rs` | `max_tokens:0` forwarded verbatim; `data:` URLs without `;base64` and `file://` images were forwarded as base64/url parts | fixed-in-copy |
| 28 | med | `crates/mr-dialects/src/responses.rs` | `content_filter` finish reason reported as `length` → clients retried a filtered generation (double charge) | fixed-in-copy |
| 29 | med | `panel/app.js` | malformed `#/route/%E0%A4%A` threw after clearing `<main>` (permanent "Loading…"); stale render clobber; concurrent 401s wiped the login form | fixed-in-copy |
| 30 | med | `panel/view-combos.js:431` | editor ignored its `target` argument (New-combo form and Edit drawer empty); drag/drop accepted empty payload → step 0 | fixed-in-copy |
| 31 | med | `panel/view-settings.js` | password card stayed in "set" mode after the first set; post-save refresh reported "Save failed" on success | fixed-in-copy |

## 2. Verified by running the code (coordinator)

* **Body-limit claim was false.** A 40 MB `POST /v1/chat/completions` with a valid client key against
  the live relay returned `413 payload_too_large` *after* the process had buffered the whole body
  (the error is produced by the post-collect length test, `server.rs`). 1 MB and 3 MB bodies were
  fully accepted and routed. The doc comment above `read_body` claims the opposite. This is the
  clearest "documentation says safe, code is not" defect in the project.
* **Auth ordering is correct.** Auth is evaluated before the body is read (`server.rs:667-678`), so an
  unauthenticated client cannot make the relay buffer 32 MB. The memory amplification is therefore
  reachable only with a valid client key — still a real DoS between tenants, not a remote one.
* **Copy integrity.** All 204 tracked files hashed identical to the original at snapshot time
  (`sha256sum` diff empty).

## 3. Open risks (need cross-file or product decisions)

1. **Process-global stream state in the translators (high, unfixed).** `openai.rs` and `google.rs`
   keep per-stream state in `thread_local!`; tokio multiplexes many requests onto one worker thread,
   and `commandcode.rs` already moved to caller-owned state because of exactly this. Two concurrent
   streams can interleave state: wrong `id`/`model`, dropped frames after the other stream sets
   `done`, cross-stream tool-call leakage. Fix: thread a per-stream state object from `server.rs`
   through `mr_dialects`, and reset on every exit path (success, error, client abort).
2. **SSE framer has no maximum frame size (medium).** `sse.rs::Framer::push` accumulates until a blank
   line and `find_boundary` rescans from offset 0 each call → O(n²) CPU and unbounded RAM if an
   upstream never terminates a record. `server.rs::translate_stream_chunk` has the same shape for
   its line carry. Fix: cap the carry (e.g. 4 MiB) and treat overflow as a stream error.
3. **Config lost updates (high).** `mr-cli` `Ctx::load` → mutate → `Ctx::save` has no lock or
   compare-and-swap; the conductor's `commit` bypasses `mutation_lock` in ~17 handlers. Two CLI
   runs, or a CLI run and the panel, silently overwrite each other. `mr key create` prints a key the
   loser immediately invalidates.
4. **Timeouts are declared but not enforced in places (medium).** `upstream.rs` `connect_timeout`
   only *classifies* errors; it is never applied to the connect (a black-holed TCP/TLS handshake
   waits for the OS default). The egress proxy's origin TLS handshake has no bound. `total_timeout`
   bounds time-to-headers only; streamed bodies and `read_all` have no deadline. The relay's body
   read has no deadline (slowloris).
5. **Quota endpoint SSRF / credential exfiltration (medium).** `quota/registry.rs` trusts
   `account.extra.quotaEndpoint` and sends the account credential there; operator headers are
   appended last and can override `Authorization`. Safe only if only an admin can write account
   extras. Needs an ownership check plus scheme/host policy.
6. **Dashboard origin behind a reverse proxy (medium).** After fixing the default-port gap, the
   Origin comparison uses the listen port; behind TLS termination every mutating request is refused.
   Needs a configurable public origin.
7. **Login has no throttling and runs Argon2id on a tokio worker (medium).** ~19 MiB / 50-100 ms per
   attempt on an async worker; loopback-only by default limits the blast radius, but a
   `allow_remote_without_password` deployment is brute-forceable.
8. **Syntax-atomic config/vault/snapshot writes (medium).** `config::write_config_atomically`, the
   vault flush and `snapshot.rs` use fixed temp names with default permissions; a pre-planted
   symlink or a concurrent writer can publish a partial file. Vault payload includes base URLs
   (possibly credentialed), secret-ref ids and client-key hashes.
9. **OAuth transport reads whole bodies (medium).** `oauth/flow.rs` `Http::send` collects the
   token-endpoint response with no cap; the loopback callback accept loop is sequential, so one
   stalled local connection can starve the real callback for the whole deadline.
10. **Panel has no request timeout (medium).** `panel/api.js` has no `AbortController`; a hung
    conductor leaves views on "Loading…" with refresh controls disabled.

## 4. What the per-file pass changed

204 tracked files were each assigned to their own reviewer against the copy; each was asked to
report findings, fix only unambiguous in-file defects, never touch another file, and never run
cargo (the shared build cache). Every reviewer wrote `.review/<flattened-path>.md`.

* Files with edits: see `git diff --stat` in the copy.
* Full per-file reports: `.review/*.md` (one per reviewed file).
* Reports that found nothing actionable are as valuable as the fixes: the quota fetchers, snapshot
  codec, `oauth/mod.rs`, `dialect.rs`, `types.rs`, `quota/github.rs`, `egress.rs`, `bench/harness.rs`
  and several panel helpers came back clean, with only hardening notes.

## 5. E2E verification

(to be appended: build, clippy, unit tests, panel tests, mock-upstream smoke, live relay probes)
