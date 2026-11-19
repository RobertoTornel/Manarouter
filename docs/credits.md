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
- `opencode.rs` — no OmniRoute counterpart: written for this project against OpenCode's documented `/zen/go/v1/usage` endpoint
- `openrouter.rs` — `open-sse/services/usage/openrouter.ts`
- `qwen.rs` — `open-sse/services/usage/qwen-token-plan.ts`, `open-sse/services/qwenTokenPlanQuotaFetcher.ts`

The shared normalization and window types are derived from `open-sse/services/usage/quota.ts` and `open-sse/services/usage/quotaWindowLabel.ts`. Tests use derived JSON fixtures and do not call upstream services.

### Generic `quotaEndpoint` (OpenAI-compatible) and Vercel AI Gateway

For providers with no dedicated fetcher, any account can declare a quota endpoint under
`extra.quotaEndpoint`. The generic reader resolves `used` / `remaining` / `total` JSON paths (or uses
conventional key names for a bare URL) and derives a missing cap as `used + remaining` — which is
exactly the shape Vercel AI Gateway's `GET /v1/credits` returns:

```json
{
  "extra": {
    "quotaEndpoint": {
      "url": "https://ai-gateway.vercel.sh/v1/credits",
      "quotas": {
        "credits": {
          "used": "$.total_used",
          "remaining": "$.balance",
          "currency": "USD"
        }
      }
    }
  }
}
```

The endpoint requires `Authorization: Bearer <api key>` with a token that has **REST usage-read
permission** ([Vercel AI Gateway REST API](https://vercel.com/docs/ai-gateway/sdks-and-apis/rest-api)).
An inference-only gateway token answers `401` on `/v1/credits`; attach a token with the read scope to
the account and refresh (`POST /api/providers/vercel/accounts/{id}/quota/refresh`) to populate the
quota view.
