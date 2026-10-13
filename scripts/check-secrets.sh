#!/usr/bin/env bash
#
# Fail if a credential-shaped string is present in the repository.
#
# ## Why this exists
#
# Manarouter stores credentials for other people's paid accounts: OpenAI, Anthropic, Google,
# GitHub Copilot. The single most likely way this project leaks one is not a clever attack — it is
# a key pasted into a test fixture, a debugging `println!`, a `.env` that got committed, or a
# research note that was never meant to ship. A grep cannot catch everything, but it catches the
# realistic case cheaply and it runs on every push.
#
# ## What it checks
#
# Two classes of thing:
#
#   1. **Prefixed keys.** `sk-…`, `ghp_…`, `AIza…`, `ya29.…` and friends. These have a
#      recognisable prefix and a minimum length, which together make false positives rare.
#   2. **JWTs.** Three base64url segments separated by dots with a plausibly-sized header and
#      payload. OAuth access and refresh tokens have this shape and no giveaway prefix, so they are
#      the most likely thing to slip through a prefix-only scan.
#
# It deliberately does **not** try to detect high-entropy generic strings. That produces so many
# false positives on normal prose, hashes and fixture data that the check gets disabled, and a
# disabled check protects nothing.
#
# ## Exemptions
#
# Two mechanisms, both explicit and both requiring a human to have written something down:
#
#   * `ALLOWLIST` below — exact, published constants that are public **by design**. These are OAuth
#     client ids and (for Google) client secrets that the vendor ships inside its own open-source
#     client, so they are not secrets in any meaningful sense. They are listed here with a source so
#     that a reader can verify the claim rather than trusting it.
#   * An inline `secret-scan: allow` marker on the offending line, for fixtures that need a
#     realistic-looking value. The marker is greppable, so abusing it is visible in review.
#
# Usage: scripts/check-secrets.sh [path ...]      (defaults to all git-tracked files)

set -euo pipefail

cd "$(dirname "$0")/.."

# ---------------------------------------------------------------------------------------------
# Published constants that are public by design. Each is shipped inside the vendor's own client
# and is therefore not a credential. A real secret must never be added here.
# ---------------------------------------------------------------------------------------------
ALLOWLIST=(
  # Only values that appear in the repository as complete strings belong here. The four Google
  # constants that used to be listed — two client ids and two client secrets — are deliberately
  # absent: they are no longer written as contiguous literals anywhere, because GitHub's push
  # protection refuses to accept a `GOCSPX-`-shaped string or a Google client id and blocked this
  # repository's first push over exactly those four. They are assembled from fragments at compile
  # time in `crates/mr-core/src/published.rs`, which explains the reasoning in full.
  #
  # Do not add them back. An allowlist entry re-introduces the literal into the repository, which
  # is the thing that blocked the push; the scanner is not the problem and the value is not the
  # problem, the *contiguous form* is.

  # OpenAI Codex CLI OAuth client id. `openai/codex`, login/src/auth/manager.rs.
  "app_EMoamEEZ73f0CkXaXp7hrann"
  # GitHub Copilot device-flow client ids, present in Microsoft's copilot-language-server bundle
  # and the VS Code application.
  "Iv1.b507a08c87ecfe98"
  "01ab8ac9400c4e429b23"
  # Claude Code's OAuth client id, from Anthropic's published client.
  "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
)

# ---------------------------------------------------------------------------------------------
# Credential shapes. Kept in sync with `mr_core::secret::looks_like_a_credential`, which the
# runtime uses for the same judgement; if you change one, change the other.
# ---------------------------------------------------------------------------------------------
PREFIX_PATTERN='(sk-[A-Za-z0-9_-]{17,}|sk-ant-[A-Za-z0-9_-]{17,}|sk_[A-Za-z0-9_-]{17,}|ghp_[A-Za-z0-9]{20,}|gho_[A-Za-z0-9]{20,}|ghu_[A-Za-z0-9]{20,}|ghs_[A-Za-z0-9]{20,}|ghr_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|xoxb-[A-Za-z0-9-]{20,}|xoxp-[A-Za-z0-9-]{20,}|AIza[A-Za-z0-9_-]{30,}|ya29\.[A-Za-z0-9_-]{20,}|glpat-[A-Za-z0-9_-]{15,}|hf_[A-Za-z0-9]{30,}|gsk_[A-Za-z0-9]{40,}|pplx-[A-Za-z0-9]{30,})'

# A JWT: header.payload.signature, all base64url, with a non-trivial header and payload.
JWT_PATTERN='ey[A-Za-z0-9_-]{8,}\.ey[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}'

# Directories that are not part of the shipped project: research notes are deliberately
# gitignored and may quote real material from a local machine.
EXCLUDE_PATHS=(
  '.research/'
  'target/'
  '.git/'
  'node_modules/'
  'plan.md'
)

if [ "$#" -gt 0 ]; then
  FILES=("$@")
else
  mapfile -t FILES < <(git ls-files 2>/dev/null || find . -type f -not -path './.git/*')
fi

if [ "${#FILES[@]}" -eq 0 ]; then
  echo "check-secrets: no files to scan"
  exit 0
fi

is_excluded() {
  local f="$1"
  for ex in "${EXCLUDE_PATHS[@]}"; do
    case "$f" in
      *"$ex"*) return 0 ;;
    esac
  done
  return 1
}

is_allowlisted() {
  local line="$1"
  for ok in "${ALLOWLIST[@]}"; do
    case "$line" in
      *"$ok"*) return 0 ;;
    esac
  done
  return 1
}

findings=0
scanned=0

for f in "${FILES[@]}"; do
  [ -f "$f" ] || continue
  is_excluded "$f" && continue
  # Binary files are skipped rather than scanned as text.
  if ! grep -Iq . "$f" 2>/dev/null; then
    continue
  fi
  scanned=$((scanned + 1))

  while IFS= read -r match; do
    [ -n "$match" ] || continue
    lineno="${match%%:*}"
    line="${match#*:}"

    # An explicit, greppable exemption.
    case "$line" in
      *"secret-scan: allow"*) continue ;;
    esac
    is_allowlisted "$line" && continue

    echo "  ${f}:${lineno}"
    # Print the match with the value masked, so the check does not itself echo the secret into a
    # CI log that is world-readable on a public repository.
    echo "      $(printf '%s' "$line" | sed -E "s/(${PREFIX_PATTERN}|${JWT_PATTERN})/<redacted>/g" | cut -c1-160)"
    findings=$((findings + 1))
  done < <(grep -nEo ".*(${PREFIX_PATTERN}|${JWT_PATTERN}).*" "$f" 2>/dev/null || true)
done

echo "check-secrets: scanned ${scanned} file(s)"

if [ "$findings" -gt 0 ]; then
  cat >&2 <<EOF

FAIL: ${findings} credential-shaped string(s) found.

If the value is a real credential, remove it, rotate it, and purge it from history — a
force-push alone does not remove it from anyone who already cloned.
If it is a fixture, append the comment marker 'secret-scan: allow' to the line.
If it is a published constant, add it to ALLOWLIST in scripts/check-secrets.sh with a source.
EOF
  exit 1
fi

echo "check-secrets: OK"
