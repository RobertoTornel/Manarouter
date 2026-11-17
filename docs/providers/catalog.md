# Provider catalogue

> **Mirrors `catalog/providers.toml` — the data is the truth.**
>
> There is no generator script, so this file is written by hand and a test in `mr-core` checks the
> tables below against the TOML, row for row: an id, a label, a dialect, a base URL, an auth kind or a
> capability list that disagrees fails the build. The prose under the tables is written by hand and
> is not checked.

212 provider templates ship. Each row names the `Dialect` the relay uses, the base URL it sends to,
and the `AuthKind` it authenticates with. Entries whose `note` field flags uncertainty are marked
**UNCERTAIN** and explained below the tables.

**Where these entries came from.** They were assembled by reading the registry of the author's own
OmniRoute install — a separate personal router used for testing and comparison, not an
implementation this project mirrors. An entry derived that way is marked **UNCERTAIN**, and where
the provider publishes its own documentation, that is the authority. See
[the reference notes](../omniroute-reference.md) and [the credits](../credits.md).

The 212 includes providers that need no credential at all — the loopback locals — and providers
Manarouter covers that the reference router did not. It is **not** the same figure as the 28
credentialed providers carried over from OmniRoute, which [the migration
document](../17-migrating-from-omniroute.md#why-the-provider-count-is-not-what-the-plan-said)
explains. Those are two different questions: how many providers does this file describe, and how
many of OmniRoute's credentialed providers could be carried across.


## Templated base URLs: `{account_id}` and `{account}`

Two of the 212 entries cannot spell their base URL without knowing something about *one particular
account*, so their `base_url` carries a placeholder:

| id | base URL | name |
|---|---|---|
| `cloudflare-ai` | `https://api.cloudflare.com/client/v4/accounts/{account_id}` | `account_id` |
| `snowflake` | `https://{account}.snowflakecomputing.com/api/v2` | `account` |

Note the second one: the placeholder is in the **host**, not the path. That is why the rule below is
about URL components rather than about path segments.

**How the value is chosen.** Each account carries a `placeholders` map, name to value, and the relay
fills the base URL from the account it is about to use. The map is per account, not per provider,
because one provider entry can serve several accounts (a personal one and a work one, each with its
own account id) — the whole point of the snapshot's account list.

```json
{ "id": "acc-1", "label": "work", "identity": "…", "secret_ref": "…",
  "placeholders": { "account_id": "023e105f4ecef8ad9ca31a8372d0c353" } }
```

**The value rule.** A value must be a single RFC 3986 *unreserved* component: letters, digits, `-`,
`.`, `_`, `~`, and nothing else. `/`, `:`, `%`, `?`, `#`, `@` and whitespace are refused, because the
value is spliced into a path *or a host* and an unchecked value could move the request to a different
host or break out of the path. A value that fails the rule is refused rather than percent-encoded:
percent-encoding an account id produces a URL the provider does not recognise, which is the failure
this feature exists to stop.

**What failure looks like.** A missing or unusable value is a `502` with code
`account_placeholder_unfilled`, naming the provider, the account and the missing name(s), with
`retry_accounts: true` — the value belongs to the account, so a combo with a second account of the
same provider carries on to it. The request is never sent with a literal `{account_id}` in the URL.

**Where to set it.** Three places, with the same rules:

* **The panel.** Every account row of a provider whose base URL carries a placeholder has a
  link-shaped button (next to the key button) that opens a `name = value` editor under the row.
  The editor is offered only where it can do something: a provider with no placeholder and an
  account with no stored map shows no button. An account that already holds values keeps its button
  even after the base URL stops being templated, so a stored value is never invisible or stuck.
  The editor lists the names the endpoint still wants, refuses a name outside
  `[A-Za-z_][A-Za-z0-9_]{0,63}` and a value outside the unreserved set *before* the save, and sends
  the whole account back with the map replaced — a field this editor cannot show is therefore never
  silently dropped. Saving a partial map is allowed and warns: a staged account is legitimate, but
  its requests will answer `502` until the gap is filled.
* **`config.json` / `PUT /api/config`.** The `placeholders` object on the account, as above.
* **Nothing else.** `mr account add` has no flag for it, and `mr config` is not implemented.

The rule itself lives in one place — `panel/placeholders.js` mirrors
`crates/mr-core/src/placeholders.rs`, and `panel/test/placeholders.test.mjs` pins both the shipped
URLs and the boundaries, so the editor cannot drift into offering a name the relay would ignore.

**Why this exists.** Before it, the literal `{account_id}` went upstream unchanged and Cloudflare
answered `404`: the relay reported a provider failure for what was, in fact, a missing configuration
value. The error body's `%7Baccount_id%7D` is what proved the braces were being sent. Two tests in
`mr-core` now hold the invariant: every shipped base URL uses only well-formed placeholders, and no
route override carries one.

---

## OAuth-backed (11)

| id | label | dialect | base URL | auth | capabilities |
|---|---|---|---|---|---|
| `antigravity` ⚠ | Google Antigravity / Gemini Code Assist | `antigravity` | `https://daily-cloudcode-pa.googleapis.com` | `oauth` (antigravity) | chat, models, tool-calling, reasoning, vision |
| `claude-code` | Claude Code (Claude subscription) | `claude-cli` | `https://api.anthropic.com/v1` | `oauth` (claude) | messages, models, tool-calling, reasoning, vision |
| `cline` | Cline | `open-ai` | `https://api.cline.bot/api/v1` | `oauth` (cline) | chat, models, tool-calling, reasoning |
| `codebuddy-cn` ⚠ | CodeBuddy CN | `open-ai` | `https://copilot.tencent.com/v2` | `oauth` (codebuddy-cn) | chat, models, tool-calling |
| `cursor` ⚠ | Cursor | `cursor` | `https://api2.cursor.sh` | `oauth` (cursor) | chat |
| `github-copilot` | GitHub Copilot | `copilot` | `https://api.githubcopilot.com` | `oauth` (github-copilot) | chat, responses, messages, embeddings, models, tool-calling, reasoning, vision |
| `grok-cli` ⚠ | Grok Build | `open-ai` | `https://cli-chat-proxy.grok.com/v1` | `oauth` (grok-cli) | chat, models, tool-calling |
| `kilocode` ⚠ | Kilo Code | `open-ai` | `https://api.kilo.ai/api/openrouter` | `oauth` (kilocode) | chat, models, tool-calling |
| `openai-codex` ⚠ | OpenAI Codex (ChatGPT subscription) | `codex` | `https://chatgpt.com/backend-api/codex` | `oauth` (openai-codex) | responses, tool-calling, reasoning, vision |
| `openference` ⚠ | Openference | `open-ai` | `https://api.openference.com/v1` | `oauth` (openference) | chat, models, tool-calling |
| `qoder` ⚠ | Qoder | `open-ai` | `https://api.qoder.com/v1` | `oauth` (qoder) | chat, models, tool-calling |
| `trae` ⚠ | Trae | `open-ai` | `https://core-normal.trae.ai/api/remote/v1` | `oauth` (trae) | chat, models, tool-calling |

`cline` is catalogued for its dialect, endpoint and capabilities only: the cline OAuth platform is not registered in this build, so there is no working login for it — [the implementation status](../23-implementation-status.md) records it as unavailable.

## Command Code bridge (1)

| id | label | dialect | base URL | auth | capabilities |
|---|---|---|---|---|---|
| `command-code` | Command Code | `command-code-go` | `https://api.commandcode.ai` | `bearer` | chat, models, tool-calling, reasoning |

## Loopback / local (3)

| id | label | dialect | base URL | auth | capabilities |
|---|---|---|---|---|---|
| `llama-cpp` | llama.cpp (local) | `open-ai` | `http://127.0.0.1:8080/v1` | `bearer` | chat, models, tool-calling, reasoning |
| `ollama` | Ollama (local) | `open-ai` | `http://127.0.0.1:11434/v1` | `bearer` | chat, models, tool-calling, reasoning, vision |
| `vllm` | vLLM (local) | `open-ai` | `http://127.0.0.1:8000/v1` | `bearer` | chat, responses, embeddings, models, tool-calling, reasoning, vision |

## API-key providers (192)

| id | label | dialect | base URL | auth | capabilities |
|---|---|---|---|---|---|
| `agentrouter` ⚠ | AgentRouter | `anthropic` | `https://agentrouter.org/v1` | `api-key-header` (x-api-key) | messages, models, tool-calling |
| `agnes` | Agnes AI | `open-ai` | `https://apihub.agnes-ai.com/v1` | `bearer` | chat, models, reasoning |
| `agnes-cn` ⚠ | Agnes AI (China) | `open-ai` | `https://api.agnes-ai.cn/v1` | `bearer` | chat, models, tool-calling |
| `ai21` ⚠ | AI21 Labs | `open-ai` | `https://api.ai21.com/studio/v1` | `bearer` | chat, models, tool-calling |
| `aimlapi` ⚠ | AI/ML API | `open-ai` | `https://api.aimlapi.com/v1` | `bearer` | chat, models, tool-calling |
| `ainative` ⚠ | AINative Studio | `open-ai` | `https://api.ainative.studio/api/v1` | `bearer` | chat, models, tool-calling |
| `aion` | Aion Labs | `open-ai` | `https://api.aionlabs.ai/v1` | `bearer` | chat, models, tool-calling, reasoning |
| `alibaba-cn` ⚠ | Alibaba (China) | `open-ai` | `https://dashscope.aliyuncs.com/compatible-mode/v1` | `bearer` | chat, models, tool-calling |
| `alibaba` ⚠ | Alibaba Cloud Model Studio | `open-ai` | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` | `bearer` | chat, models, tool-calling |
| `ant-ling` ⚠ | Ant Ling / Ring (inclusionAI) | `open-ai` | `https://api.ant-ling.com/v1` | `bearer` | chat, models, tool-calling |
| `anthropic` | Anthropic | `anthropic` | `https://api.anthropic.com/v1` | `api-key-header` (x-api-key) | messages, models, tool-calling, reasoning, vision |
| `anyapi` ⚠ | AnyAPI AI | `open-ai` | `https://api.anyapi.ai/v1` | `bearer` | chat, models, tool-calling |
| `api-airforce` | API.airforce | `open-ai` | `https://api.airforce/v1` | `bearer` | chat, models, tool-calling, reasoning |
| `arcee-ai` ⚠ | Arcee AI | `open-ai` | `https://api.arcee.ai/api/v1` | `bearer` | chat, models, tool-calling |
| `auriko` ⚠ | Auriko | `open-ai` | `https://api.auriko.ai/v1` | `bearer` | chat, models, tool-calling |
| `bai` ⚠ | b.ai | `open-ai` | `https://api.b.ai/v1` | `bearer` | chat, models, tool-calling |
| `baichuan` ⚠ | Baichuan | `open-ai` | `https://api.baichuan-ai.com/v1` | `bearer` | chat, models, tool-calling |
| `baidu` ⚠ | Baidu (ERNIE) | `open-ai` | `https://qianfan.baidubce.com/v2` | `bearer` | chat, models, tool-calling |
| `bailian-coding-plan` ⚠ | Alibaba Token Plan | `anthropic` | `https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic/v1` | `api-key-header` (x-api-key) | messages, models, tool-calling |
| `baseten` ⚠ | Baseten | `open-ai` | `https://inference.baseten.co/v1` | `bearer` | chat, models, tool-calling |
| `bazaarlink` ⚠ | BazaarLink | `open-ai` | `https://bazaarlink.ai/api/v1` | `bearer` | chat, models, tool-calling |
| `blackbox` ⚠ | Blackbox AI | `open-ai` | `https://api.blackbox.ai/v1` | `bearer` | chat, models, tool-calling |
| `bluesminds` ⚠ | BluesMinds | `open-ai` | `https://api.bluesminds.com/v1` | `bearer` | chat, models, tool-calling |
| `byteplus` ⚠ | BytePlus ModelArk | `open-ai` | `https://ark.ap-southeast.bytepluses.com/api/v3` | `bearer` | chat, models, tool-calling |
| `bytez` ⚠ | Bytez | `open-ai` | `https://api.bytez.com/models/v2/openai/v1` | `bearer` | chat, models |
| `cerebras` | Cerebras | `open-ai` | `https://api.cerebras.ai/v1` | `bearer` | chat, models, tool-calling, reasoning |
| `charm-hyper` ⚠ | Charm Hyper | `open-ai` | `https://hyper.charm.land/v1` | `bearer` | chat, models, tool-calling |
| `chat-oripe` ⚠ | Chat Oripe | `open-ai` | `https://api.oriper.com/v1` | `bearer` | chat, models, tool-calling |
| `chatanywhere` ⚠ | ChatAnywhere | `open-ai` | `https://api.chatanywhere.org/v1` | `bearer` | chat, models, tool-calling |
| `cheaperinference` ⚠ | Cheaper Inference | `open-ai` | `https://api.cheaperinference.com/v1` | `bearer` | chat, responses, models, reasoning |
| `chenzk` ⚠ | Chenzk API | `open-ai` | `https://chenzk.top/v1` | `bearer` | chat, models, tool-calling |
| `chutes` ⚠ | Chutes.ai | `open-ai` | `https://llm.chutes.ai/v1` | `bearer` | chat, models, tool-calling |
| `cloudcode-one` ⚠ | CloudCode.ONE | `open-ai` | `https://api.cloudcode.one/v1` | `bearer` | chat, models, tool-calling |
| `cloudflare-ai` ⚠ | Cloudflare Workers AI | `open-ai` | `https://api.cloudflare.com/client/v4/accounts/{account_id}` | `bearer` | chat, embeddings, models |
| `codestral` ⚠ | Codestral | `open-ai` | `https://codestral.mistral.ai/v1` | `bearer` | chat, models, tool-calling |
| `cohere` | Cohere | `open-ai` | `https://api.cohere.com/compatibility/v1` | `bearer` | chat, embeddings, models, tool-calling, reasoning, vision |
| `coze` ⚠ | Coze | `open-ai` | `https://api.coze.com/v1` | `bearer` | chat, models, tool-calling |
| `crof` ⚠ | CrofAI | `open-ai` | `https://crof.ai/v1` | `bearer` | chat, models, tool-calling |
| `dahl` ⚠ | Dahl | `open-ai` | `https://inference.dahl.global/v1` | `bearer` | chat, models, tool-calling |
| `deepgram` | Deepgram | `open-ai` | `https://api.deepgram.com` | `header` (Authorization) | audio-transcription |
| `deepinfra` | DeepInfra | `open-ai` | `https://api.deepinfra.com/v1/openai` | `bearer` | chat, embeddings, models, tool-calling, reasoning, vision |
| `deepseek` | DeepSeek | `open-ai` | `https://api.deepseek.com` | `bearer` | chat, responses, models, tool-calling, reasoning |
| `dgrid` ⚠ | DGrid | `open-ai` | `https://api.dgrid.ai/v1` | `bearer` | chat, models, tool-calling |
| `digitalocean` ⚠ | DigitalOcean | `open-ai` | `https://inference.do-ai.run/v1` | `bearer` | chat, models, tool-calling |
| `dit` ⚠ | DIT.ai | `open-ai` | `https://api.dit.ai/v1` | `bearer` | chat, models, tool-calling |
| `doubao` ⚠ | Doubao | `open-ai` | `https://ark.cn-beijing.volces.com/api/v3` | `bearer` | chat, models, tool-calling |
| `dxnt` ⚠ | DXNT / DX Token | `open-ai` | `https://www.dxnt.com/v1` | `bearer` | chat, models, tool-calling |
| `electronhub` ⚠ | Electron Hub | `open-ai` | `https://api.electronhub.ai/v1` | `bearer` | chat, models, tool-calling |
| `elevenlabs` ⚠ | ElevenLabs | `open-ai` | `https://api.elevenlabs.io` | `header` (xi-api-key) | audio-speech |
| `eurouter` ⚠ | EURouter | `open-ai` | `https://api.eurouter.ai/v1` | `bearer` | chat, models, tool-calling |
| `factory` ⚠ | Factory | `open-ai` | `https://api.factory.ai/v1` | `bearer` | chat, models, tool-calling |
| `fastrouter` ⚠ | FastRouter | `open-ai` | `https://api.fastrouter.ai/api/v1` | `bearer` | chat, models, tool-calling |
| `featherless-ai` ⚠ | Featherless AI | `open-ai` | `https://api.featherless.ai/v1` | `bearer` | chat, models, tool-calling |
| `fireworks` | Fireworks AI | `open-ai` | `https://api.fireworks.ai/inference/v1` | `bearer` | chat, embeddings, models, tool-calling, reasoning, vision |
| `fish-audio` | Fish Audio | `open-ai` | `https://api.fish.audio` | `bearer` | audio-speech |
| `free-ai` ⚠ | Free.ai | `open-ai` | `https://api.free.ai/v1` | `bearer` | chat, models |
| `freeaiapikey` | FreeAIAPIKey | `open-ai` | `https://api.freeaiapikey.com/v1` | `bearer` | chat, models, tool-calling, reasoning |
| `freebuff` ⚠ | Freebuff | `open-ai` | `https://www.codebuff.com/api/v1` | `bearer` | chat, models, tool-calling |
| `freeinference` ⚠ | FreeInference | `open-ai` | `https://freeinference.org/v1` | `bearer` | chat, models, tool-calling |
| `freemodel-dev` | FreeModel.dev | `open-ai` | `https://api.freemodel.dev/v1` | `bearer` | chat, models, tool-calling, reasoning |
| `freetheai` ⚠ | FreeTheAi | `open-ai` | `https://api.freetheai.xyz/v1` | `bearer` | chat, models, tool-calling |
| `friendliai` ⚠ | FriendliAI | `open-ai` | `https://api.friendli.ai/serverless/v1` | `bearer` | chat, models, tool-calling |
| `g4f-gemini` ⚠ | g4f.space — Gemini | `open-ai` | `https://g4f.space/api/gemini/v1` | `bearer` | chat, models, tool-calling |
| `g4f-groq` ⚠ | g4f.space — Groq | `open-ai` | `https://g4f.space/api/groq/v1` | `bearer` | chat, models, tool-calling |
| `g4f-nvidia` ⚠ | g4f.space — NVIDIA | `open-ai` | `https://g4f.space/api/nvidia/v1` | `bearer` | chat, models, tool-calling |
| `g4f-ollama` ⚠ | g4f.space — Ollama | `open-ai` | `https://g4f.space/api/ollama/v1` | `bearer` | chat, models, tool-calling |
| `g4f-pollinations` ⚠ | g4f.space — Pollinations | `open-ai` | `https://g4f.space/api/pollinations/v1` | `bearer` | chat, models, tool-calling |
| `galadriel` ⚠ | Galadriel | `open-ai` | `https://api.galadriel.ai/v1` | `bearer` | chat, models, tool-calling |
| `gemini` | Google Gemini (AI Studio) | `google-ai-studio` | `https://generativelanguage.googleapis.com/v1beta` | `api-key-header` (x-goog-api-key) | chat, embeddings, models, tool-calling, reasoning, vision |
| `gigachat` ⚠ | GigaChat (Sber) | `open-ai` | `https://gigachat.devices.sberbank.ru/api/v1` | `bearer` | chat, models, tool-calling |
| `glm` | Z.ai GLM Coding | `open-ai` | `https://api.z.ai/api/coding/paas/v4` | `bearer` | chat, models, tool-calling, reasoning |
| `glm-cn` ⚠ | GLM Coding (China) | `open-ai` | `https://open.bigmodel.cn/api/coding/paas/v4` | `bearer` | chat, models, tool-calling |
| `greenpt` ⚠ | GreenPT | `open-ai` | `https://api.greenpt.ai/v1` | `bearer` | chat, models, tool-calling |
| `groq` | Groq | `open-ai` | `https://api.groq.com/openai/v1` | `bearer` | chat, models, audio-transcription, audio-speech, tool-calling, reasoning |
| `hcnsec` ⚠ | Huancheng Public API | `open-ai` | `https://api.hcnsec.cn/v1` | `bearer` | chat, models, tool-calling |
| `helixmind` ⚠ | HelixMind | `open-ai` | `https://helixmind.online/v1` | `bearer` | chat, models, tool-calling |
| `helyxai` ⚠ | Helyx AI | `open-ai` | `https://helyxai.space/v1` | `bearer` | chat, models, tool-calling |
| `heroku` ⚠ | Heroku AI | `open-ai` | `https://us.inference.heroku.com/v1` | `bearer` | chat, models, tool-calling |
| `huggingface` ⚠ | HuggingFace Router | `open-ai` | `https://router.huggingface.co/v1` | `bearer` | chat, models |
| `hyperbolic` | Hyperbolic | `open-ai` | `https://api.hyperbolic.xyz/v1` | `bearer` | chat, models, tool-calling, reasoning |
| `iflytek` ⚠ | iFlytek Spark | `open-ai` | `https://spark-api-open.xf-yun.com/v1` | `bearer` | chat, models, tool-calling |
| `inception` | Inception | `open-ai` | `https://api.inceptionlabs.ai/v1` | `bearer` | chat, models, reasoning |
| `inference-net` ⚠ | Inference.net | `open-ai` | `https://api.inference.net/v1` | `bearer` | chat, models, tool-calling |
| `internlm` ⚠ | InternLM (Intern-S1) | `open-ai` | `https://chat.intern-ai.org.cn/api/v1` | `bearer` | chat, models, tool-calling |
| `jina` | Jina AI | `open-ai` | `https://api.jina.ai/v1` | `bearer` | embeddings |
| `kenari` ⚠ | Kenari | `open-ai` | `https://kenari.id/v1` | `bearer` | chat, models, tool-calling |
| `kilo-gateway` ⚠ | Kilo Gateway | `open-ai` | `https://api.kilo.ai/api/gateway` | `bearer` | chat, models, tool-calling |
| `kimi` ⚠ | Kimi (Moonshot) | `open-ai` | `https://api.moonshot.ai/v1` | `bearer` | chat, responses, models, tool-calling, reasoning |
| `lambda-ai` ⚠ | Lambda AI | `open-ai` | `https://api.lambda.ai/v1` | `bearer` | chat, models, tool-calling |
| `liquid` ⚠ | Liquid AI | `open-ai` | `https://inference.liquid.ai/v1` | `bearer` | chat, models, tool-calling |
| `literouter` ⚠ | LiteRouter | `open-ai` | `https://api.literouter.com/v1` | `bearer` | chat, models, tool-calling |
| `llamagate` ⚠ | LlamaGate | `open-ai` | `https://llamagate.ai/v1` | `bearer` | chat, models, tool-calling |
| `llm-kiwi` | LLM.Kiwi | `open-ai` | `https://api.llm.kiwi/v1` | `bearer` | chat, models |
| `llm7` ⚠ | LLM7.io | `open-ai` | `https://api.llm7.io/v1` | `bearer` | chat, models, tool-calling |
| `llmgateway` ⚠ | LLM Gateway | `open-ai` | `https://api.llmgateway.io/v1` | `bearer` | chat, models, tool-calling |
| `logfare` ⚠ | Logfare | `open-ai` | `https://logfare.ai/v1` | `bearer` | chat, models, tool-calling |
| `longcat` ⚠ | LongCat AI | `open-ai` | `https://api.longcat.chat/openai/v1` | `bearer` | chat, models, tool-calling |
| `lyceum` ⚠ | Lyceum | `open-ai` | `https://api.lyceum.technology/openai/v1` | `bearer` | chat, models, tool-calling |
| `meganova-ai` ⚠ | MegaNova AI | `open-ai` | `https://api.meganova.ai/v1` | `bearer` | chat, models, tool-calling |
| `meta-llama` ⚠ | Meta Llama API | `open-ai` | `https://api.llama.com/compat/v1` | `bearer` | chat, models, tool-calling |
| `minimax` | MiniMax Coding | `open-ai` | `https://api.minimax.io/v1` | `bearer` | chat, models, tool-calling, reasoning |
| `minimax-cn` ⚠ | Minimax (China) | `open-ai` | `https://api.minimaxi.com/v1` | `bearer` | chat, models, tool-calling |
| `mistral` | Mistral | `open-ai` | `https://api.mistral.ai/v1` | `bearer` | chat, embeddings, models, tool-calling, reasoning, vision |
| `mixlayer` ⚠ | Mixlayer | `open-ai` | `https://models.mixlayer.ai/v1` | `bearer` | chat, models, tool-calling |
| `mnn-ai` ⚠ | MNN AI | `open-ai` | `https://api.mnnai.ru/v1` | `bearer` | chat, models, tool-calling |
| `modal` ⚠ | Modal | `open-ai` | `https://api.modal.ai/v1` | `bearer` | chat, models, tool-calling |
| `modelscope` ⚠ | ModelScope | `open-ai` | `https://api-inference.modelscope.cn/v1` | `bearer` | chat, models, tool-calling |
| `monsterapi` ⚠ | MonsterAPI | `open-ai` | `https://api.monsterapi.ai/v1` | `bearer` | chat, models, tool-calling |
| `morph` ⚠ | Morph | `open-ai` | `https://api.morphllm.com/v1` | `bearer` | chat, models, tool-calling |
| `naga-ac` ⚠ | Naga.ac | `open-ai` | `https://api.naga.ac/v1` | `bearer` | chat, models, tool-calling |
| `naga-ai` ⚠ | Naga AI | `open-ai` | `https://api.naga.ac/v1` | `bearer` | chat, models, tool-calling |
| `nanogpt` ⚠ | NanoGPT | `open-ai` | `https://nano-gpt.com/api/v1` | `bearer` | chat, models, tool-calling |
| `nara` ⚠ | NaraRouter | `open-ai` | `https://router.bynara.id/v1` | `bearer` | chat, models, tool-calling |
| `navy` ⚠ | NavyAI | `open-ai` | `https://api.navy/v1` | `bearer` | chat, models, tool-calling |
| `nebius` ⚠ | Nebius AI | `open-ai` | `https://api.studio.nebius.ai/v1` | `bearer` | chat, embeddings, models, tool-calling, reasoning |
| `nous-research` ⚠ | Nous Research | `open-ai` | `https://inference-api.nousresearch.com/v1` | `bearer` | chat, models, tool-calling |
| `novita` | Novita AI | `open-ai` | `https://api.novita.ai/openai/v1` | `bearer` | chat, embeddings, models, tool-calling, reasoning, vision |
| `nscale` ⚠ | nScale | `open-ai` | `https://inference.api.nscale.com/v1` | `bearer` | chat, models, tool-calling |
| `nube` ⚠ | Nube.sh | `open-ai` | `https://ai.nube.sh/api/v1` | `bearer` | chat, models, tool-calling |
| `nvidia` | NVIDIA NIM | `open-ai` | `https://integrate.api.nvidia.com/v1` | `bearer` | chat, embeddings, models, tool-calling, reasoning, vision |
| `ofoxai` ⚠ | OfoxAI | `open-ai` | `https://api.ofox.ai/v1` | `bearer` | chat, models, tool-calling |
| `ollama-cloud` | Ollama Cloud | `open-ai` | `https://ollama.com/v1` | `bearer` | chat, models, tool-calling, reasoning, vision |
| `openadapter` | OpenAdapter | `open-ai` | `https://api.openadapter.in/v1` | `bearer` | chat, models, tool-calling, reasoning |
| `openai` | OpenAI | `open-ai` | `https://api.openai.com/v1` | `bearer` | chat, responses, embeddings, audio-transcription, audio-speech, images-generations, models, tool-calling, reasoning, vision |
| `opencode` | OpenCode Free | `open-ai` | `https://opencode.ai/zen/v1` | `bearer` | chat, models, reasoning |
| `opencode-go` | OpenCode Go | `open-ai` | `https://opencode.ai/zen/go/v1` | `bearer` | chat, responses, models, tool-calling, reasoning |
| `opencode-zen` | OpenCode Zen | `open-ai` | `https://opencode.ai/zen/v1` | `bearer` | chat, responses, models, tool-calling, reasoning |
| `openference-api` ⚠ | Openference API | `open-ai` | `https://api.openference.com/v1` | `bearer` | chat, models, tool-calling |
| `openrouter` | OpenRouter | `open-ai` | `https://openrouter.ai/api/v1` | `bearer` | chat, responses, embeddings, models, tool-calling, reasoning, vision |
| `openvecta` ⚠ | OpenVecta | `open-ai` | `https://api.openvecta.com/v1` | `bearer` | chat, models, tool-calling |
| `opper` ⚠ | Opper | `open-ai` | `https://api.opper.ai/v3/compat` | `bearer` | chat, models, tool-calling |
| `orcarouter` ⚠ | OrcaRouter | `open-ai` | `https://api.orcarouter.ai/v1` | `bearer` | chat, models, tool-calling |
| `ovhcloud` ⚠ | OVHcloud AI | `open-ai` | `https://oai.endpoints.kepler.ai.cloud.ovh.net/v1` | `bearer` | chat, models, tool-calling |
| `perplexity` | Perplexity | `open-ai` | `https://api.perplexity.ai` | `bearer` | chat, models, tool-calling, reasoning |
| `perplexity-agent` ⚠ | Perplexity Agent | `open-ai` | `https://api.perplexity.ai/v1` | `bearer` | chat, responses, models, tool-calling |
| `pioneer` ⚠ | Pioneer AI | `open-ai` | `https://api.pioneer.ai/v1` | `bearer` | chat, models, tool-calling |
| `plamo` ⚠ | PLaMo | `open-ai` | `https://api.platform.preferredai.jp/v1` | `bearer` | chat, models, tool-calling |
| `poe` ⚠ | Poe | `open-ai` | `https://api.poe.com/v1` | `bearer` | chat, models, tool-calling |
| `poixe-ai` ⚠ | Poixe AI | `open-ai` | `https://api.poixe.com/v1` | `bearer` | chat, models, tool-calling |
| `poolside` ⚠ | Poolside | `open-ai` | `https://inference.poolside.ai/v1` | `bearer` | chat, models, tool-calling |
| `predibase` ⚠ | Predibase | `open-ai` | `https://serving.app.predibase.com/v1` | `bearer` | chat, models, tool-calling |
| `publicai` ⚠ | PublicAI | `open-ai` | `https://api.publicai.co/v1` | `bearer` | chat, models, tool-calling |
| `qianfan` ⚠ | Baidu Qianfan | `open-ai` | `https://qianfan.baidubce.com/v2` | `bearer` | chat, models, tool-calling |
| `qiniu` ⚠ | Qiniu | `open-ai` | `https://api.qnaigc.com/v1` | `bearer` | chat, models, tool-calling |
| `qwen-cloud-token-plan` ⚠ | Qwen Cloud Token Plan | `open-ai` | `https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1` | `bearer` | chat, models, tool-calling |
| `qwen-cloud` ⚠ | Qwen Cloud | `open-ai` | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` | `bearer` | chat, models, tool-calling |
| `regolo` ⚠ | Regolo AI | `open-ai` | `https://api.regolo.ai/v1` | `bearer` | chat, models, tool-calling |
| `reka` ⚠ | Reka | `open-ai` | `https://api.reka.ai/v1` | `bearer` | chat, models, tool-calling |
| `requesty` | Requesty | `open-ai` | `https://router.requesty.ai/v1` | `bearer` | chat, models, tool-calling, reasoning |
| `routeway` | Routeway | `open-ai` | `https://api.routeway.ai/v1` | `bearer` | chat, models, tool-calling, reasoning |
| `sambanova` | SambaNova | `open-ai` | `https://api.sambanova.ai/v1` | `bearer` | chat, models, tool-calling, reasoning |
| `sarvam` ⚠ | Sarvam AI | `open-ai` | `https://api.sarvam.ai/v1` | `bearer` | chat, models, tool-calling |
| `scaleway` ⚠ | Scaleway AI | `open-ai` | `https://api.scaleway.ai/v1` | `bearer` | chat, models, tool-calling |
| `sealion` | SEA-LION | `open-ai` | `https://api.sea-lion.ai/v1` | `bearer` | chat, embeddings, models, reasoning |
| `seekai` ⚠ | SeekAi | `open-ai` | `https://seekai.cc/v1` | `bearer` | chat, models, tool-calling |
| `sensenova` ⚠ | SenseNova | `open-ai` | `https://token.sensenova.cn/v1` | `bearer` | chat, models, tool-calling |
| `siliconflow` ⚠ | SiliconFlow | `open-ai` | `https://api.siliconflow.com/v1` | `bearer` | chat, models, tool-calling |
| `snowflake` ⚠ | Snowflake Cortex | `open-ai` | `https://{account}.snowflakecomputing.com/api/v2` | `bearer` | chat, models, tool-calling |
| `sparkdesk` ⚠ | SparkDesk | `open-ai` | `https://spark-api-open.xf-yun.com/v1` | `bearer` | chat, models, tool-calling |
| `speka` ⚠ | Speka AI | `open-ai` | `https://speka.me/v1` | `bearer` | chat, models, tool-calling |
| `stability-ai` | Stability AI | `open-ai` | `https://api.stability.ai` | `bearer` | images-generations |
| `stepfun` ⚠ | StepFun | `open-ai` | `https://api.stepfun.com/v1` | `bearer` | chat, models, tool-calling |
| `sumopod` ⚠ | SumoPod | `open-ai` | `https://ai.sumopod.com/v1` | `bearer` | chat, models, tool-calling |
| `synthetic` ⚠ | Synthetic | `open-ai` | `https://api.synthetic.new/openai/v1` | `bearer` | chat, models, tool-calling |
| `tabitoken` ⚠ | TabiToken | `anthropic` | `https://tabitoken.com/v1` | `api-key-header` (x-api-key) | messages, models, tool-calling |
| `tencent` ⚠ | Tencent Hunyuan | `open-ai` | `https://api.hunyuan.cloud.tencent.com/v1` | `bearer` | chat, models, tool-calling |
| `together` | Together AI | `open-ai` | `https://api.together.xyz/v1` | `bearer` | chat, embeddings, models, tool-calling, reasoning, vision |
| `token-kiosk` ⚠ | Token Kiosk | `open-ai` | `https://agent-router.gaib.ai/v1` | `bearer` | chat, models, tool-calling |
| `tokenreply` ⚠ | TokenReply | `open-ai` | `https://api.tokenreply.com/v1` | `bearer` | chat, models, tool-calling |
| `tokenrouter` ⚠ | TokenRouter | `open-ai` | `https://api.tokenrouter.com/v1` | `bearer` | chat, models, tool-calling |
| `typhoon` ⚠ | Typhoon | `open-ai` | `https://api.opentyphoon.ai/v1` | `bearer` | chat, models, tool-calling |
| `uc-direct` ⚠ | UC Direct (uncensored.com) | `open-ai` | `https://api.uncensored.com/api/v1` | `bearer` | chat, models, tool-calling |
| `uncloseai` ⚠ | UncloseAI | `open-ai` | `https://hermes.ai.unturf.com/v1` | `bearer` | chat, models |
| `unorouter` ⚠ | UnoRouter | `open-ai` | `https://api.unorouter.com/v1` | `bearer` | chat, models, tool-calling |
| `upstage` ⚠ | Upstage | `open-ai` | `https://api.upstage.ai/v1` | `bearer` | chat, models, tool-calling |
| `venice` ⚠ | Venice.ai | `open-ai` | `https://api.venice.ai/api/v1` | `bearer` | chat, models, tool-calling |
| `vercel-ai-gateway` | Vercel AI Gateway | `open-ai` | `https://ai-gateway.vercel.sh/v1` | `bearer` | chat, responses, embeddings, models, tool-calling, reasoning, vision |
| `void-ai` ⚠ | Void AI | `open-ai` | `https://api.voidai.app/v1` | `bearer` | chat, models, tool-calling |
| `volcengine-agent-plan` ⚠ | Volcengine Ark Agent Plan | `open-ai` | `https://ark.cn-beijing.volces.com/api/plan/v3` | `bearer` | chat, models, tool-calling |
| `volcengine-coding-plan` ⚠ | Volcengine Ark Coding Plan | `open-ai` | `https://ark.cn-beijing.volces.com/api/coding/v3` | `bearer` | chat, models, tool-calling |
| `volcengine` ⚠ | Volcengine | `open-ai` | `https://ark.cn-beijing.volces.com/api/v3` | `bearer` | chat, models, tool-calling |
| `voyage` | Voyage AI | `open-ai` | `https://api.voyageai.com/v1` | `bearer` | embeddings |
| `wafer` ⚠ | Wafer AI | `anthropic` | `https://pass.wafer.ai/v1` | `api-key-header` (x-api-key) | messages, models, tool-calling |
| `wandb` ⚠ | Weights & Biases Inference | `open-ai` | `https://api.inference.wandb.ai/v1` | `bearer` | chat, models, tool-calling |
| `writer` ⚠ | Writer | `open-ai` | `https://api.writer.com/v1` | `bearer` | chat, models, tool-calling |
| `x5lab` ⚠ | X5Lab | `open-ai` | `https://api.x5lab.dev/v1` | `bearer` | chat, models, tool-calling |
| `xai` | xAI (Grok) | `open-ai` | `https://api.x.ai/v1` | `bearer` | chat, responses, models, tool-calling, reasoning, vision |
| `xiaomi-mimo` | Xiaomi MiMo | `open-ai` | `https://api.xiaomimimo.com/v1` | `bearer` | chat, models, reasoning |
| `xiaomi-mimo-token-plan` ⚠ | Xiaomi MiMo Token Plan | `open-ai` | `https://token-plan-sgp.xiaomimimo.com/v1` | `bearer` | chat, models, tool-calling |
| `xkiro` ⚠ | xKiro | `open-ai` | `https://api.xkiro.com/v1` | `bearer` | chat, models, tool-calling |
| `yi` ⚠ | Yi (01.AI) | `open-ai` | `https://api.lingyiwanwu.com/v1` | `bearer` | chat, models, tool-calling |
| `yolo-auto` ⚠ | Yolo-Auto | `open-ai` | `https://yolo-auto.com/v1` | `bearer` | chat, models, tool-calling |
| `zai` ⚠ | Z.AI | `anthropic` | `https://api.z.ai/api/anthropic/v1` | `api-key-header` (x-api-key) | messages, models, tool-calling |
| `zcode` | Z Code (Z.ai plan) | `zcode` | `https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1` | `oauth` (zcode) | messages, tool-calling, reasoning |
| `zenmux` ⚠ | ZenMux | `open-ai` | `https://zenmux.ai/api/v1` | `bearer` | chat, models, tool-calling |
| `zerolimitai` ⚠ | ZeroLimitAI | `open-ai` | `https://www.zerolimitai.com/api/v1` | `bearer` | chat, models, tool-calling |
| `zylo-api` ⚠ | Zylo API | `open-ai` | `https://api.zyloai.net/v1` | `bearer` | chat, models, tool-calling |

## Uncertainty the catalogue records

These are marked in the data itself, with the reasoning, rather than being silently
presented as verified. A provider marked uncertain works today; what is uncertain is how
long its endpoint or credentials will keep working, or whether a header is still required.


### `antigravity`

UNCERTAIN (T2): the Antigravity client identity and the exact x-goog-api-client and User-Agent values are reverse-engineered, not primary-confirmed. Client-Metadata is a JSON object and must be sent verbatim. Every project-scoped call needs the project id obtained from the onboardUser envelope.


### `openai-codex`

OAuth only. The relay injects chatgpt-account-id from the JWT claim at https://api.openai.com/auth -> chatgpt_account_id; sending a literal value there is a hard failure. originator must be codex_cli_rs for the official endpoint. UNCERTAIN: OpenAI-Beta responses=experimental is only needed by older servers, so it is sent for compatibility. UNCERTAIN: the two api.connectors.* scopes are version-dependent; older clients request only the first four, and a server that rejects an unknown scope would fail the whole request, so drop them if authorization is refused with an invalid_scope error.


### `bytez`

UNCERTAIN: the registry base URL is model-scoped (/models/v2/openai/v1), so the chat path depends on how Bytez resolves the model id.


### `cheaperinference`

UNCERTAIN: the OmniRoute executor selects the chat or responses URL per model.


### `cloudflare-ai`

UNCERTAIN: the registry base stops at /client/v4/accounts; the account id is a path segment, recorded in route_overrides. The Workers AI binding also exposes embeddings and images.


### `free-ai`

UNCERTAIN: the registry lists the chat URL as https://api.free.ai/v1/chat/ with zero models, so the prefix used here is inferred.


### `huggingface`

UNCERTAIN: OmniRoute lists chat at router.huggingface.co/v1, while 9Router classifies HuggingFace as an image and STT provider.


### `kimi`

UNCERTAIN which host a given key reaches: OmniRoute's kimi entry points at the legacy Moonshot host api.moonshot.ai/v1, while 9Router's newer kimi entry uses api.kimi.com/coding/v1 with OpenAI, Anthropic and Responses transports.


### `nebius`

UNCERTAIN: 9Router puts chat on api.studio.nebius.ai/v1 and embeddings on api.tokenfactory.nebius.com/v1; OmniRoute lists chat on the tokenfactory host.


### `uncloseai`

UNCERTAIN: a free public endpoint that needs no credential; availability and rate limits are not guaranteed.


## See also

- [Catalogue reference](README.md) — how to add an entry and what each field means.
- [OpenAI-compatible providers](openai-compatible.md)
- [Anthropic-compatible providers](anthropic-compatible.md)
- [Command Code](commandcode.md)
- [Codex](codex.md) · [Copilot](copilot.md) · [Antigravity](antigravity.md)

