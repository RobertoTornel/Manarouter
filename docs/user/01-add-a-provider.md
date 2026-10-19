This guide helps you choose an upstream provider setup, store its credential safely, and confirm that Manarouter can reach it.

# Add a provider

A provider is the upstream API Manarouter calls. When possible, start from a catalogue entry: it supplies the provider's known base URL, dialect, and authentication style. The dialect describes the API shape, not the company: OpenAI-compatible, Anthropic Messages, Google, Codex, Copilot, Antigravity, and Command Code Go are different dialects supported by this build.

## Add a catalogue provider

For example, to use the OpenAI catalogue entry:

```bash
mr provider add openai
```

Expect a new provider entry configured from the catalogue. To see supported catalogue names, use the names shown in the catalogue (`openai`, `anthropic`, `google`, `openrouter`, `deepseek`, `groq`, and others), or inspect `mr provider add --help` for the syntax. Do not pick a provider solely by brand name: make sure its dialect and authentication flow match the upstream endpoint you intend to use.

For a custom compatible endpoint, give Manarouter an ID, dialect, and base URL. For example:

```bash
mr provider add my-endpoint --id my-endpoint --dialect open-ai --base-url https://api.example.test/v1
```

Expect a provider named `my-endpoint`. The base URL must start with `https://` or `http://`; use loopback HTTP only for a local service. The dialect spellings include `open-ai`, `anthropic`, `google`, `codex`, `copilot`, `antigravity`, and `command-code-go`. Use `mr provider add --help` for the complete options, including `--auth` and `--header`, when configuring a custom endpoint.

The panel's provider form lets you choose a dialect and enter a base URL, but it does not currently auto-fill URLs when you choose an OAuth or Command Code Go dialect. The CLI does copy those values from a catalogue entry. For example, `command-code` is the catalogue ID for the Command Code Go dialect, and OAuth catalogue entries include `openai-codex`, `github-copilot`, and `antigravity`:

```bash
mr provider add command-code
mr provider add openai-codex
```

Expect the provider dialect and base URL to come from each catalogue template. For OAuth providers, complete the supported OAuth flow in the panel; entering a token as a normal API key is not the same as completing that flow. The OAuth flows currently documented for this build are Codex, GitHub Copilot, and Antigravity.

## Add the provider credential

For a provider that uses a manually entered API credential, add an account with `--secret-stdin`:

```bash
mr account add openai --secret-stdin
```

Expect the terminal to wait for standard input. Paste the upstream credential there and press Ctrl-D (EOF). Manarouter reports a masked fingerprint and stores the credential in the encrypted vault; the config stores only a reference.

Do not use `--secret <value>` for a real credential. Shell command arguments can be visible to other local processes and may persist in shell history. The CLI does implement that option, but explicitly warns that the value may be visible to `ps`. Use `--secret-stdin` instead. The `--secret-env VAR` option is also implemented, but avoid placing credentials in shell startup files or logs.

Your local `vault.key` is needed to decrypt stored provider credentials. Protect your local account and backups; losing that key while retaining encrypted vault data makes the stored credentials unavailable.

OAuth-managed providers use an authorization flow rather than `mr account add --secret-stdin`. Add the appropriate catalogue entry and complete the provider's flow in the panel if that platform is enabled in your build.

## Sync and list models

Sync contacts the upstream provider's `/models` endpoint. It is a network request, so it may count against provider request limits even though it does not generate a completion.

```bash
mr models sync openai
```

Expect the CLI to fetch and save the models it can parse. If the provider does not need credentials for `/models` (such as some local endpoints), sync can try without one; otherwise it uses the stored credential. Current limitations: this tree has no implemented sync branch for Copilot, Antigravity, or Command Code Go dialects. Google and Codex model sync currently use built-in model entries rather than fetching `/models`. Check the CLI output for the behavior of your configured dialect.

```bash
mr models list openai
```

Expect the stored model IDs for `openai`. You can also run `mr models list` to see qualified IDs across configured providers. Use an ID that appears in this list when making a request.

## Prewarm connections

The provider configuration default for prewarm is off. When honored, prewarm is a best-effort connection to the provider's base URL at startup, intended to leave a warm connection for the first real request; a prewarm failure should not stop startup. It does not send a model prompt. There is no `mr provider add` flag for this setting.

Do not rely on prewarm behavior in this build: although the provider model contains the setting and the relay has a prewarm method, I could not find a relay call site that invokes it. Also, the panel's new-provider form currently checks the prewarm toggle by default and sends that value; turn it off explicitly if you create a provider there and want the stored setting off.

## Test reachability carefully

You can test provider reachability and authentication without sending a completion by using the default provider test, which makes one authenticated `GET /models` request:

```bash
mr provider test openai
```

Expect the upstream's actual HTTP status and a redacted response summary. This is still a network request and may count against rate limits. **Do not add `--model` unless you intend to make a real, one-token conversation:** that option sends a prompt and can consume quota or incur charges. The default probe needs the provider's `/models` endpoint to be available; if it is not, the CLI has no dry-run test mode to substitute.

## Common errors

- **401 or 403:** the upstream rejected the credential or access. Confirm that you chose the right provider/account and that the credential is current. Do not paste it into chat or logs; enter a replacement with `mr account add … --secret-stdin` after following the provider's rotation/revocation guidance.
- **404:** confirm the base URL and its path prefix (for example whether it already includes `/v1`), the dialect, and the model ID. A wrong URL path often reaches a valid host but not the API route.
- **DNS error:** check that the hostname is spelled correctly and that your machine can resolve and reach it. For a local endpoint, confirm that it is running at the URL you configured.
- **TLS/certificate error:** check that the hostname matches the certificate, your system time is correct, and any required CA certificates are trusted. Do not disable TLS verification as a workaround.

See [getting started](./00-getting-started.md) to make your first request and [client keys](./02-client-keys.md) to manage your app-facing credential.
