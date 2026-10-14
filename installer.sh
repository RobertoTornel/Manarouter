#!/usr/bin/env bash
# Install Manarouter from its public GitHub source checkout.
set -Eeuo pipefail

REPO_URL="${MANAROUTER_REPO_URL:-https://github.com/RobertoTornel/Manarouter.git}"
INSTALL_DIR="${MANAROUTER_INSTALL_DIR:-${HOME:?HOME must be set}/.local/share/manarouter/source}"
BIN_DIR="${MANAROUTER_BIN_DIR:-${HOME:?HOME must be set}/.local/bin}"

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

if [[ -e "$INSTALL_DIR" && ! -d "$INSTALL_DIR/.git" ]]; then
  die "install path exists and is not a Git checkout: $INSTALL_DIR (set MANAROUTER_INSTALL_DIR to choose another path)"
fi
mkdir -p "$(dirname "$INSTALL_DIR")" "$BIN_DIR"
if [[ -d "$INSTALL_DIR/.git" ]]; then
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
log "Note: this installs the binaries only. You still need to configure a provider and a client key before sending requests."
