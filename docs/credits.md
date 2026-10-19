# Credits

The quota fetchers in `crates/mr-core/src/quota/` are read-only ports or derived work based on OmniRoute (https://github.com/diegosouzapw/omniroute), licensed under the MIT License, © 2026 diegosouzapw. These fetchers read upstream usage and quota endpoints; they do not include OmniRoute's application or modify upstream account state.

The provider fetcher source mappings are:

- `antigravity.rs` — `open-sse/services/usage/antigravity.ts`, `open-sse/services/usage/antigravityWeeklyQuota.ts`
- `claude.rs` — `open-sse/services/usage/claude.ts`, `open-sse/services/usage/claudeQuota.ts`
- `codex.rs` — `open-sse/services/usage/codex.ts`, `open-sse/services/codexQuotaFetcher.ts`
- `commandcode.rs` — `open-sse/services/usage/command-code.ts`
- `cursor.rs` — `open-sse/services/usage/cursor.ts`
- `deepseek.rs` — `open-sse/services/usage/deepseek.ts`, `open-sse/services/deepseekQuotaFetcher.ts`
- `github.rs` — `open-sse/services/usage/github.ts`
- `glm.rs` — `open-sse/services/usage/glm.ts`, `open-sse/services/glmResetCards.ts`
- `grok.rs` — `open-sse/services/usage/xai.ts`, `open-sse/services/usage/grokCli.ts`
- `kimi.rs` — `open-sse/services/usage/kimi.ts`
- `minimax.rs` — `open-sse/services/usage/minimax.ts`
- `moonshot.rs` — `open-sse/services/usage/moonshotOpenPlatform.ts`
- `openai_compat.rs` — `open-sse/services/usage/openaiCompatible.ts`
- `openrouter.rs` — `open-sse/services/usage/openrouter.ts`
- `qwen.rs` — `open-sse/services/usage/qwen-token-plan.ts`, `open-sse/services/qwenTokenPlanQuotaFetcher.ts`

The shared normalization and window types are derived from `open-sse/services/usage/quota.ts` and `open-sse/services/usage/quotaWindowLabel.ts`. Tests use derived JSON fixtures and do not call upstream services.
