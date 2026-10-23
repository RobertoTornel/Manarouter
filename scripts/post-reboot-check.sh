#!/usr/bin/env bash
# Verify Manarouter came up correctly after a reboot.
#
# This exists because the failure it checks for is silent. A systemd ordering cycle makes systemd
# delete a start job, so the unit is *not* marked failed — it is simply never started, `systemctl
# is-active` says `inactive`, and there is no error to grep for. That is exactly what happened once:
# the relay never came up after a boot, and every check that started a service by hand passed.
#
# Run it after a reboot, and after any change to the systemd units.
#
# Usage:
#   scripts/post-reboot-check.sh
#
# Exit codes: 0 all checks passed, 1 one or more failed.
set -Eeuo pipefail

RELAY_PORT="${MANAROUTER_RELAY_PORT:-20900}"
CONTROL_PORT="${MANAROUTER_CONTROL_PORT:-20901}"
FAILED=0
# The ports are interpolated into a regex below, so a non-numeric value would match the wrong
# listener and turn a broken boot into a PASS. Reject it instead of checking the wrong thing.
for port_value in "$RELAY_PORT" "$CONTROL_PORT"; do
  [[ "$port_value" =~ ^[0-9]{1,5}$ ]] || { printf 'invalid port: %q\n' "$port_value" >&2; exit 1; }
done

pass() { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
skip() { printf '  \033[33mSKIP\033[0m %s\n' "$*"; }
head_() { printf '\n%s\n' "$*"; }

head_ "1. Services are running"
for unit in manarouter-conductor manarouter-relay; do
  state="$(systemctl --user is-active "$unit.service" 2>/dev/null || true)"
  if [[ "$state" == "active" ]]; then
    pass "$unit is active"
  else
    # `inactive` after a reboot is the ordering-cycle signature, not a crash: a crash is `failed`.
    fail "$unit is '$state' (expected active) — if this is right after a reboot, check for an"
    printf '       ordering cycle: systemd --user --test 2>&1 | grep -i "ordering cycle"\n'
  fi
done

head_ "2. No ordering cycle in the current boot"
# The manager logs this at boot. The pattern is anchored to systemd's own message text — a target or
# unit name, then `Found ordering cycle` — on purpose. A looser grep for "ordering cycle" matches the
# *command line* of any process that merely mentions the phrase, which is how an earlier version of
# this script reported a failure against its own log entry. That false positive was worth fixing
# rather than tolerating: a check that cries wolf on a healthy boot is worse than no check.
CYCLE_RE='systemd\[[0-9]+\]: [A-Za-z0-9@._-]+: Found ordering cycle'
# Read the journal once, and never with `grep -q`: it exits on the first match, journalctl then
# dies of SIGPIPE, and `set -o pipefail` turns that into a non-zero pipeline — so the check would
# report *no* cycle on exactly the boot it exists to detect. Only the matching lines are kept.
cycles="$(journalctl --user -b --no-pager 2>/dev/null | grep -E "$CYCLE_RE" || true)"
if [[ -n "$cycles" ]]; then
  fail "an ordering cycle was logged this boot:"
  printf '%s\n' "$cycles" | tail -3 | sed 's/^/       /'
  printf '       Check the units with: python3 scripts/check-systemd-cycle.py ~/.config/systemd/user\n'
else
  pass "no ordering cycle logged this boot"
fi

head_ "3. Listeners are bound"
for entry in "relay:$RELAY_PORT" "conductor:$CONTROL_PORT"; do
  name="${entry%%:*}"; port="${entry##*:}"
  if ! command -v ss >/dev/null 2>&1; then
    # Same pipefail/SIGPIPE trap as above: drain grep's input instead of using -q.
    skip "$name: 'ss' is not installed, cannot check the listener"
  elif ss -ltn 2>/dev/null | grep -E "127\.0\.0\.1:$port|\[::1\]:$port" >/dev/null; then
    pass "$name is listening on $port"
  else
    fail "$name is not listening on $port"
  fi
done

head_ "4. The control plane answers"
if curl -sf -m 10 "http://127.0.0.1:$CONTROL_PORT/api/session" >/dev/null 2>&1; then
  pass "panel API responded on $CONTROL_PORT"
else
  fail "no response from http://127.0.0.1:$CONTROL_PORT/api/session"
fi

head_ "5. The relay enforces client keys"
# A 401 proves two things at once — the relay is serving, and it is not in the keyless mode that
# accepts every request. No real key is needed, and none is used.
code="$(curl -s -o /dev/null -w '%{http_code}' -m 10 \
  "http://127.0.0.1:$RELAY_PORT/v1/models" -H 'Authorization: Bearer not-a-real-key' || true)"
if [[ "$code" == "401" ]]; then
  pass "an invalid key is refused with 401"
elif [[ "$code" == "200" ]]; then
  fail "an invalid key was ACCEPTED (HTTP 200) — the relay has no client keys configured"
  printf '       Create one with: mr key create --label laptop --print\n'
else
  fail "unexpected HTTP $code from the relay's /v1/models"
fi

head_ "6. Provider and model state survived"
if providers="$(curl -sf -m 10 "http://127.0.0.1:$CONTROL_PORT/api/providers" 2>/dev/null)"; then
  summary="$(printf '%s' "$providers" | python3 -c '
import json, sys
try:
    data = json.load(sys.stdin).get("providers", [])
except Exception:
    print("unparseable"); raise SystemExit
models = sum(len(p.get("models") or []) for p in data)
accounts = sum(len(p.get("accounts") or []) for p in data)
print(f"{len(data)} providers, {models} models, {accounts} accounts")
' 2>/dev/null || echo unparseable)"
  if [[ "$summary" == "0 providers, 0 models, 0 accounts" ]]; then
    # Not a failure: a fresh install legitimately has nothing configured yet.
    skip "no providers configured yet ($summary)"
  elif [[ "$summary" == "unparseable" ]]; then
    fail "the providers endpoint returned something unparseable"
  else
    pass "$summary"
  fi
else
  # /api/providers sits behind the panel's session guard, so a 401 means the control plane is up
  # and only this check lacks a session. Failing the whole run for that is a false alarm on any
  # install with a panel password set.
  api_code="$(curl -s -o /dev/null -w '%{http_code}' -m 10 \
    "http://127.0.0.1:$CONTROL_PORT/api/providers" 2>/dev/null || true)"
  if [[ "$api_code" == "401" || "$api_code" == "403" ]]; then
    skip "the panel requires a session, so provider state was not checked"
  else
    fail "could not read /api/providers (HTTP ${api_code:-no response})"
  fi
fi

head_ "7. Optional: a real completion"
# Skipped unless a key is supplied, because it spends provider quota. Nothing is read from a file or
# guessed: without MANAROUTER_TEST_KEY this check does not run at all.
if [[ -n "${MANAROUTER_TEST_KEY:-}" && -n "${MANAROUTER_TEST_MODEL:-}" ]]; then
  body="$(curl -s -m 120 "http://127.0.0.1:$RELAY_PORT/v1/chat/completions" \
    -H "Authorization: Bearer $MANAROUTER_TEST_KEY" -H 'Content-Type: application/json' \
    -d "{\"model\":\"$MANAROUTER_TEST_MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"Reply with exactly: OK\"}],\"max_tokens\":16}" \
    2>/dev/null || true)"
  if [[ "$body" == *'"choices"'* ]]; then
    pass "a completion succeeded through $MANAROUTER_TEST_MODEL"
  else
    fail "the completion failed: ${body:0:200}"
  fi
else
  skip "set MANAROUTER_TEST_KEY and MANAROUTER_TEST_MODEL to include a real completion"
fi

printf '\n'
if [[ "$FAILED" -eq 0 ]]; then
  printf 'post-reboot check: all checks passed\n'
else
  printf 'post-reboot check: FAILURES above\n'
fi
exit "$FAILED"
