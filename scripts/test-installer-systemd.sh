#!/usr/bin/env bash
# Exercise installer source acquisition/build/install and generated systemd user units in isolation.
# Never call the host's git/cargo/systemctl or write outside a disposable HOME.
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

HOME="$TMP/home"
mkdir -p "$HOME" "$TMP/bin" "$TMP/log"
export HOME PATH="$TMP/bin:/usr/bin:/bin" MOCK_LOG="$TMP/log/calls"

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
