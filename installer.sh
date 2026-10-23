#!/usr/bin/env bash
# Install Manarouter from its public GitHub source checkout.
set -Eeuo pipefail

REPO_URL="${MANAROUTER_REPO_URL:-https://github.com/RobertoTornel/Manarouter.git}"
INSTALL_DIR="${MANAROUTER_INSTALL_DIR:-${HOME:?HOME must be set}/.local/share/manarouter/source}"
BIN_DIR="${MANAROUTER_BIN_DIR:-${HOME:?HOME must be set}/.local/bin}"
INSTALL_USER_SERVICES="${MANAROUTER_SYSTEMD_USER:-0}"

usage() {
  cat <<'USAGE'
Usage: installer.sh [--systemd-user] [--no-start] [--from-checkout]

  --systemd-user  Install and enable separate conductor/relay systemd user units.
  --no-start      Install/reload units but do not enable or start them.
  --from-checkout Build from the script's directory instead of cloning/updating the install checkout.

Environment overrides: MANAROUTER_INSTALL_DIR, MANAROUTER_BIN_DIR,
MANAROUTER_REPO_URL, MANAROUTER_SYSTEMD_USER=1.
USAGE
}
START_USER_SERVICES=1
for arg in "$@"; do
  case "$arg" in
    --systemd-user) INSTALL_USER_SERVICES=1 ;;
    --no-start) START_USER_SERVICES=0 ;;
    --from-checkout) INSTALL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)" ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'Unknown option: %s\n' "$arg" >&2; usage >&2; exit 2 ;;
  esac
done

log() { printf 'Manarouter installer: %s\n' "$*"; }
die() { log "ERROR: $*" >&2; exit 1; }

for tool in git; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool is required. Install it and run this installer again."
done

if ! command -v cargo >/dev/null 2>&1; then
  if command -v rustup >/dev/null 2>&1; then
    export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
  else
    die "Rust is required. Install rustup from https://rustup.rs, then run this installer again."
  fi
fi
command -v cargo >/dev/null 2>&1 || die "cargo was not found in PATH. Install Rust from https://rustup.rs and open a new terminal."
command -v rustup >/dev/null 2>&1 || log "rustup was not found; using the cargo toolchain already installed."

if [[ -e "$INSTALL_DIR" && ! -e "$INSTALL_DIR/.git" ]]; then
  die "install path exists and is not a Git checkout: $INSTALL_DIR (set MANAROUTER_INSTALL_DIR to choose another path)"
fi
mkdir -p "$(dirname "$INSTALL_DIR")" "$BIN_DIR"
if [[ -e "$INSTALL_DIR/.git" ]]; then
  log "Updating source in $INSTALL_DIR"
  git -C "$INSTALL_DIR" pull --ff-only || die "could not fast-forward existing checkout; resolve it manually or choose a fresh MANAROUTER_INSTALL_DIR"
else
  log "Downloading source from $REPO_URL"
  git clone --depth 1 "$REPO_URL" "$INSTALL_DIR" || die "download failed"
fi

log "Building optimized binaries (this can take a few minutes)"
(
  cd "$INSTALL_DIR"
  cargo build --release --locked
) || die "build failed; see the Rust/Cargo error above"

for binary in manarouter-relay manarouter-conductor mr; do
  source="$INSTALL_DIR/target/release/$binary"
  [[ -x "$source" ]] || die "expected build output is missing: $source"
  ln -sfn "$source" "$BIN_DIR/$binary"
done

log "Installed binaries into $BIN_DIR"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) log "Add this to your shell profile so commands are available:"
     printf '  export PATH="%s:$PATH"\n' "$BIN_DIR" ;;
esac
log "Quick help: $BIN_DIR/mr --help"

if [[ "$INSTALL_USER_SERVICES" == "1" ]]; then
  if [[ "$BIN_DIR" == *" "* || "$HOME" == *" "* ]]; then
    die "systemd unit paths cannot currently contain spaces; set MANAROUTER_BIN_DIR and HOME to whitespace-free paths"
  fi
  command -v systemctl >/dev/null 2>&1 || die "systemctl is required for --systemd-user"
  [[ "$INSTALL_DIR" != *" "* ]] || die "systemd WorkingDirectory cannot contain spaces; set MANAROUTER_INSTALL_DIR to a whitespace-free path"
  systemctl --user show-environment >/dev/null 2>&1 || die "systemd user manager is unavailable; run from a logged-in systemd session (or enable lingering with loginctl enable-linger ${USER:-$(id -un)})"
  UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  mkdir -p "$UNIT_DIR" "$HOME/.config/manarouter" "$HOME/.local/state/manarouter"
  chmod 0700 "$HOME/.config/manarouter" "$HOME/.local/state/manarouter"
  # Ordering note, because this exact mistake cost a boot: the conductor must NOT be ordered
  # `After=default.target`. `default.target` is what pulls these units in via `WantedBy`, so a unit
  # that is both wanted-by and ordered-after it forms a cycle with anything that waits for that unit
  # — which is precisely what the relay does. systemd breaks the cycle by *deleting a start job*,
  # and the deletion is silent from the outside: the relay simply never starts, `systemctl is-active`
  # says inactive, and there is no failed unit to notice. It reproduces only on a real boot, since
  # starting a unit by hand does not re-evaluate `default.target`'s ordering.
  cat >"$UNIT_DIR/manarouter-conductor.service" <<EOF
[Unit]
Description=Manarouter control plane and web panel
After=network.target

[Service]
Type=simple
WorkingDirectory=$INSTALL_DIR
ExecStart=$BIN_DIR/manarouter-conductor
Restart=on-failure
RestartSec=3
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=full
ProtectHome=read-only
ReadWritePaths=$HOME/.config/manarouter $HOME/.local/state/manarouter
Environment=MANAROUTER_CONFIG_DIR=$HOME/.config/manarouter
Environment=MANAROUTER_STATE_DIR=$HOME/.local/state/manarouter

[Install]
WantedBy=default.target
EOF
  cat >"$UNIT_DIR/manarouter-relay.service" <<EOF
[Unit]
Description=Manarouter data plane relay
After=manarouter-conductor.service
Wants=manarouter-conductor.service

[Service]
Type=simple
WorkingDirectory=$INSTALL_DIR
ExecStart=$BIN_DIR/manarouter-relay
Restart=on-failure
RestartSec=3
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=full
ProtectHome=read-only
ReadWritePaths=$HOME/.config/manarouter $HOME/.local/state/manarouter
Environment=MANAROUTER_CONFIG_DIR=$HOME/.config/manarouter
Environment=MANAROUTER_STATE_DIR=$HOME/.local/state/manarouter

[Install]
WantedBy=default.target
EOF
  chmod 0644 "$UNIT_DIR/manarouter-conductor.service" "$UNIT_DIR/manarouter-relay.service"
  systemctl --user daemon-reload
  if [[ "$START_USER_SERVICES" == "1" ]]; then
    systemctl --user enable manarouter-conductor.service manarouter-relay.service
    systemctl --user restart manarouter-conductor.service manarouter-relay.service || systemctl --user start manarouter-conductor.service manarouter-relay.service
    log "Enabled and started Manarouter systemd user services (web panel + API relay)."
  else
    log "Systemd user units installed; not started (--no-start)."
  fi
  log "Status: systemctl --user status manarouter-conductor manarouter-relay"
  log "Logs: journalctl --user -u manarouter-conductor -u manarouter-relay -f"
else
  log "To install and start systemd user services, rerun with --systemd-user."
fi
log "Note: configure a provider and client key before sending requests."
