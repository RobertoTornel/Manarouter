This guide takes you from a fresh install to your first request through Manarouter.

# Getting started

Manarouter is a self-hosted gateway between your app and one or more AI providers. A **provider credential** lets Manarouter call the upstream provider; a separate **client key** lets your app call Manarouter.

## Before you begin

The documented installer is for Linux and macOS and needs Git, `curl`, and Rust installed with `rustup`. It downloads the source and builds Manarouter on your computer. Install Rust from <https://rustup.rs> if needed, then run the installer from a terminal:

```bash
curl -fsSL https://raw.githubusercontent.com/RobertoTornel/Manarouter/main/installer.sh | bash
```

Expect `mr --help` to work in a new terminal. If the installer says `~/.local/bin` is not on your `PATH`, follow its printed instructions first.

## Start Manarouter

Run both local services together in the foreground:

```bash
mr serve
```

Expect the relay and panel/control service to start. Leave this terminal open and use a second terminal for the setup commands below. Press Ctrl-C in the `mr serve` terminal when you want to stop this foreground session.

- OpenAI-compatible relay: <http://127.0.0.1:20900/v1>
- Web panel: <http://127.0.0.1:20901>

The panel is bound to loopback by default. Keep it local; it can manage providers, credentials, and client keys. Do not expose it to an untrusted network.

## First successful request

Run these steps in your second terminal. `openai` is an example catalogue provider; choose a provider you have an account with.

1. Add the provider:

   ```bash
   mr provider add openai
   ```

   Expect a provider entry using the catalogue's dialect and base URL.

2. Add its credential through standard input:

   ```bash
   mr account add openai --secret-stdin
   ```

   Paste the provider credential into the terminal and press Ctrl-D (EOF). Expect a confirmation with a masked fingerprint. The credential is stored in the encrypted vault, not in `config.json`. Avoid `--secret`: command-line arguments can be visible to other local processes.

3. Fetch and inspect the provider's model list:

   ```bash
   mr models sync openai
   mr models list openai
   ```

   Expect the sync to report models it found and the list to show their IDs. Sync contacts the provider's `/models` endpoint; it does not generate a completion, but it may still be subject to provider request limits.

4. Create an app-facing client key:

   ```bash
   mr key create --label my-app --print
   ```

   Expect a one-time key on standard output. Save it securely; do not paste it into chat or commit it to source control.

5. Make one request, replacing `PROVIDER/MODEL_FROM_LIST` with the provider and a model ID you just listed, and `YOUR_CLIENT_KEY` with the one-time key:

   ```bash
   curl http://127.0.0.1:20900/v1/chat/completions \
     -H 'Authorization: Bearer YOUR_CLIENT_KEY' \
     -H 'Content-Type: application/json' \
     -d '{"model":"PROVIDER/MODEL_FROM_LIST","messages":[{"role":"user","content":"Hello"}]}'
   ```

   Substituting a real client key writes it into your shell history and exposes it in `ps`. If that matters, keep the header in a 0600 curl config file and pass it with `curl -K <file>`.

   Expect a JSON completion if the model and credentials are accepted. This calls the upstream provider and may incur charges.

## Where to go next

- [Add a provider](./01-add-a-provider.md) — choose a dialect, add credentials, and troubleshoot model sync.
- [Client keys](./02-client-keys.md) — create, rotate, and disable app credentials.
- [Calling the API](./03-calling-the-api.md) — API request details.
- [Models and reasoning levels](./04-models-and-reasoning-levels.md) — model names and options.
- [Combos](./05-combos.md) — route a name through multiple models.
- [Quota](./06-quota.md) — understand usage and limits.
- [Images, audio, and video](./07-images-audio-video.md) — modality-specific requests.
- [Proxies](./08-proxies.md) — provider connection settings.
- [Pricing](./09-pricing.md) — review model price information.
- [Troubleshooting](./10-troubleshooting.md) and [security](./11-security.md) — diagnose issues and protect access.
- [CLI reference](./12-cli-reference.md) — command index.
