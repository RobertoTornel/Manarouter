#!/usr/bin/env bash
# Exercise installer source acquisition/build/install and generated systemd user units in isolation.
# Never call the host's git/cargo/systemctl or write outside a disposable HOME.
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

HOME="$TMP/home"
mkdir -p "$HOME" "$TMP/bin" "$TMP/log"
# The PATH is restricted so the host's git, cargo and systemctl cannot be reached by accident, and
# `/usr/local/bin` is included because that is where macOS keeps `python3`, which the ordering-cycle
# check needs.
export HOME PATH="$TMP/bin:/usr/bin:/bin:/usr/local/bin" MOCK_LOG="$TMP/log/calls"
# The XDG directories are pinned to the disposable HOME. The installer writes its units under
# `${XDG_CONFIG_HOME:-$HOME/.config}`, so a runner that exports XDG_CONFIG_HOME would otherwise
# put them outside the tree this test looks at — a failure that has nothing to do with the
# installer.
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_STATE_HOME="$HOME/.local/state"
export XDG_DATA_HOME="$HOME/.local/share"

fail() { printf 'installer test: %s\n' "$*" >&2; exit 1; }
assert_file() { [[ -f "$1" ]] || fail "expected file: $1"; }
assert_contains() { grep -Fq -- "$2" "$1" || fail "expected '$2' in $1"; }

cat >"$TMP/bin/git" <<'MOCK_GIT'
#!/usr/bin/env bash
set -Eeuo pipefail
printf 'git %s\n' "$*" >>"$MOCK_LOG"
case "$1" in
  clone)
    dest="${@: -1}"
    mkdir -p "$dest/.git"
    ;;
  -C)
    # The update flow only needs a successful `git pull --ff-only`.
    ;;
  *) exit 9 ;;
esac
MOCK_GIT

cat >"$TMP/bin/cargo" <<'MOCK_CARGO'
#!/usr/bin/env bash
set -Eeuo pipefail
printf 'cargo %s\n' "$*" >>"$MOCK_LOG"
[[ "$1" == build && "$2" == --release && "$3" == --locked ]] || exit 8
mkdir -p target/release
for binary in manarouter-relay manarouter-conductor mr; do
  printf '#!/bin/sh\nexit 0\n' >"target/release/$binary"
  chmod 0755 "target/release/$binary"
done
MOCK_CARGO

cat >"$TMP/bin/systemctl" <<'MOCK_SYSTEMCTL'
#!/usr/bin/env bash
set -Eeuo pipefail
printf 'systemctl %s\n' "$*" >>"$MOCK_LOG"
case "$*" in
  '--user show-environment'|'--user daemon-reload'|'--user enable manarouter-conductor.service manarouter-relay.service'|\
  '--user restart manarouter-conductor.service manarouter-relay.service') exit 0 ;;
  *) printf 'unexpected systemctl invocation: %s\n' "$*" >&2; exit 7 ;;
esac
MOCK_SYSTEMCTL
chmod 0755 "$TMP/bin/git" "$TMP/bin/cargo" "$TMP/bin/systemctl"

# Install path is isolated. A fake clone gives the fake build a valid cwd.
export MANAROUTER_INSTALL_DIR="$HOME/.local/share/manarouter/source"
export MANAROUTER_BIN_DIR="$HOME/.local/bin"
export MANAROUTER_REPO_URL=https://example.invalid/manarouter.git

"$ROOT/installer.sh" --systemd-user >"$TMP/install.log"

UNIT_DIR="$HOME/.config/systemd/user"
CONDUCTOR="$UNIT_DIR/manarouter-conductor.service"
RELAY="$UNIT_DIR/manarouter-relay.service"
assert_file "$CONDUCTOR"
assert_file "$RELAY"
assert_file "$HOME/.local/bin/mr"
assert_file "$HOME/.local/bin/manarouter-conductor"
assert_file "$HOME/.local/bin/manarouter-relay"

# Unit semantics: both launched, restart policy, correct ordering, and writable config/state paths.
assert_contains "$CONDUCTOR" 'Description=Manarouter control plane and web panel'
assert_contains "$CONDUCTOR" "ExecStart=$HOME/.local/bin/manarouter-conductor"
assert_contains "$CONDUCTOR" "WorkingDirectory=$MANAROUTER_INSTALL_DIR"
assert_contains "$CONDUCTOR" 'Restart=on-failure'
assert_contains "$CONDUCTOR" "ReadWritePaths=$HOME/.config/manarouter $HOME/.local/state/manarouter"
assert_contains "$RELAY" 'Description=Manarouter data plane relay'
assert_contains "$RELAY" "ExecStart=$HOME/.local/bin/manarouter-relay"
assert_contains "$RELAY" "WorkingDirectory=$MANAROUTER_INSTALL_DIR"
assert_contains "$RELAY" 'After=manarouter-conductor.service'
assert_contains "$RELAY" 'Wants=manarouter-conductor.service'
assert_contains "$RELAY" 'Restart=on-failure'

# The generated units must not form an ordering cycle.
#
# This is the check that a real reboot taught us to have. An earlier release ordered the conductor
# `After=default.target` while also installing it `WantedBy=default.target`; at boot systemd found
# the cycle, deleted the relay's start job, and the relay never came up — with no failed unit and no
# error, just `inactive`. Nothing else in this file could catch it: `systemd-analyze verify` does not
# examine the ordering graph, and starting a unit by hand does not reproduce the boot-time ordering.
if command -v python3 >/dev/null 2>&1; then
  python3 "$ROOT/scripts/check-systemd-cycle.py" "$UNIT_DIR" ||
    fail 'the generated systemd units contain an ordering cycle (see above)'
else
  # A missing python3 must not silently retire the check.
  fail 'python3 is required to check the generated units for ordering cycles'
fi

# Systemctl must only have been invoked through the fake and installation calls include enable+restart.
assert_contains "$MOCK_LOG" 'systemctl --user daemon-reload'
assert_contains "$MOCK_LOG" 'systemctl --user enable manarouter-conductor.service manarouter-relay.service'
assert_contains "$MOCK_LOG" 'systemctl --user restart manarouter-conductor.service manarouter-relay.service'
assert_contains "$MOCK_LOG" 'cargo build --release --locked'
assert_contains "$MOCK_LOG" "git clone --depth 1 $MANAROUTER_REPO_URL $MANAROUTER_INSTALL_DIR"

# Updating reuses the checkout (pull --ff-only), rebuilds, and restarts both services.
: >"$MOCK_LOG"
"$ROOT/installer.sh" --systemd-user >"$TMP/update.log"
assert_contains "$MOCK_LOG" "git -C $MANAROUTER_INSTALL_DIR pull --ff-only"
assert_contains "$MOCK_LOG" 'cargo build --release --locked'
assert_contains "$MOCK_LOG" 'systemctl --user restart manarouter-conductor.service manarouter-relay.service'

# --no-start writes units but must not enable/start them.
: >"$MOCK_LOG"
"$ROOT/installer.sh" --systemd-user --no-start >"$TMP/no-start.log"
assert_contains "$MOCK_LOG" 'systemctl --user daemon-reload'
if grep -Eq 'systemctl --user (enable|start|restart) ' "$MOCK_LOG"; then
  fail '--no-start unexpectedly enabled or started units'
fi

printf 'installer systemd tests: passed (isolated HOME; install, update, --no-start)\n'
