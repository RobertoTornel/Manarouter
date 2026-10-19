# OmniRoute reference notes

**What OmniRoute is, in this repository:** a separate, personal AI router the author runs on their
own machine, mostly for testing and for comparison. It is **not** an implementation Manarouter
mirrors, and Manarouter is not a port of it. Nothing here is a parity promise, a compatibility
target, or a checklist of work still owed — where Manarouter differs from that install, the
difference is intentional.

This file is kept for two honest reasons. First, the provider catalogue was assembled by reading
that install's registry, so the notes record where the entries came from and which of them were
never confirmed against a vendor's own documentation. Second, the OAuth adapters were written
against the same upstream protocol evidence, and the links below are the evidence.

Everything the catalogue derived from that source is marked **UNCERTAIN** in
[the provider catalogue](providers/catalog.md), and the MIT attribution for the derived work is in
[the credits](credits.md). Where this project needs a provider's own documentation, that is the
authority — not this file, and not the other router.

## Provider-flow inventory

This is a provider-flow inventory, not a promise that every OmniRoute integration has been ported. OmniRoute contains many provider-specific OAuth, device-code, IDE import, and proprietary account flows; those cannot safely be treated as interchangeable generic OAuth. Only adapters with adequate protocol evidence, an appropriate Manarouter provider/dialect, and deterministic tests should be registered as working login options.

## Current Manarouter adapters

- OpenAI Codex — loopback authorization code with PKCE by default, plus an OpenAI device-code fallback.
- GitHub Copilot — device authorization.
- Google Antigravity — loopback OAuth and Cloud Code account onboarding; experimental T2 client metadata remains unverified and paid-tier onboarding requires a project.

Cline appears as a catalog migration target but does not have an implemented adapter. The Anthropic catalog provider is API-key authentication; OmniRoute's Claude/Claude Code OAuth is a different account entitlement and provider protocol, so this work must not silently convert that template to OAuth. xAI's listed API-key provider similarly does not imply the distinct Grok CLI/xAI OAuth offerings.

OmniRoute's current provider registry includes (among others) Claude, Codex, Antigravity/agy, Qoder, Kimi Coding, GitHub and GHE Copilot, GitLab Duo, Kiro/Amazon Q, Cursor, Trae, KiloCode, Cline, Devin, Grok CLI, xAI OAuth, Openference, CodeBuddy variants, WorkBuddy, Zed (including Zed Hosted), and Muse Code. Some are device flows, import-only/manual credential paths, environment-configured, dynamically hosted, or proprietary IDE integrations. They need individual protocol, product/dialect, and policy review; this is not equivalent to a standard OAuth authorization-code flow.

Upstream references: [OmniRoute OAuth registry](https://github.com/diegosouzapw/OmniRoute/blob/main/src/lib/oauth/providers/index.ts), [OAuth constants](https://github.com/diegosouzapw/OmniRoute/blob/main/src/lib/oauth/constants/oauth.ts), [provider source tree](https://github.com/diegosouzapw/OmniRoute/tree/main/src/lib/oauth/providers).

## Client-key rotation

Manarouter's key feature is **client gateway key rotation**, not upstream provider credential reset. `POST /api/keys/{id}/rotate` replaces that record's verifier, retains its id and policy, invalidates the old key on commit, and returns the new plaintext exactly once. The panel confirms the action and displays a copy-once reveal. `mr key rotate <id-or-label>` provides the same local-config operation; `--print` emits only the new key on stdout. Keep the replacement secure: existing client keys cannot be recovered from their hashes.

OmniRoute reference behavior: [regeneration route](https://github.com/diegosouzapw/OmniRoute/blob/release/v3.8.52/src/app/api/keys/%5Bid%5D/regenerate/route.ts), [regeneration tests](https://github.com/diegosouzapw/OmniRoute/blob/release/v3.8.52/tests/unit/api-key-regeneration.test.ts), [key manager UX](https://github.com/diegosouzapw/OmniRoute/blob/release/v3.8.52/src/app/%28dashboard%29/dashboard/api-manager/ApiManagerPageClient.tsx).
