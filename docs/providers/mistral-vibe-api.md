# Mistral Vibe API

Mistral's public platform surface at `https://api.mistral.ai/v1`, authenticated with a standard
bearer API key (the same credential the Mistral Vibe CLI reads from `MISTRAL_API_KEY`).

This is the API-backed sibling of `mistral-vibe` (the LeChat consumer gateway at
`chat.mistral.ai`). The wire format is OpenAI-shaped and is forwarded byte-for-byte from an OpenAI
client: no Vibe create envelope, no website chat shapes. The dialect exists so the surface can be
named explicitly in configuration, gets the standard `/v1/*` path rules, and is documented against
the Vibe CLI's own backend (`api.mistral.ai`) rather than the app's website.

## Configuration

```toml
[[providers]]
id = "mistral-vibe-api"
label = "Mistral Vibe API"
dialect = "mistral-vibe-api"
base_url = "https://api.mistral.ai/v1"
auth = "bearer"
capabilities = ["chat", "models", "tool-calling", "reasoning"]

[[providers.models]]
id = "mistral-large-latest"   # use the ids your key can actually serve
input_modalities = ["text"]

[[providers.accounts]]
id = "default"
```

Attach the API key with:

```bash
mr account add mistral-vibe-api --secret-stdin
```

(all paste the key and send EOF; it is stored in the encrypted vault, never on the command line).

## Behavior

- Requests pass through with only the model-id resolution (`provider/model` → the step's model id)
  applied. `reasoning_effort`, `temperature`, `max_tokens`, tool calls, vision and streaming all
  go through as the OpenAI client wrote them; responses come back unchanged, including `usage`.
- Streaming is the client's choice (unlike the LeChat gateway, which always streams and whose
  frames the relay folds).
- The same accounts, quota windows, combos, sticky success and failover machinery as every other
  provider apply.
