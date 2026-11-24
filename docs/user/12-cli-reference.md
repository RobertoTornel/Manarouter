# CLI reference

This is a compact operator reference for the implemented `mr` commands, with safe credential handling and clear notes about command groups that are not yet implemented.

Run `mr <command> --help` for the installed version's full syntax and options. Read-only commands that print lists or status can use the global `--json` option for JSON output; `--config-dir <DIR>` points the CLI at a different configuration directory, and takes precedence over `MANAROUTER_CONFIG_DIR`.

## Provider setup and checks

```bash
mr provider add <catalogue-id>
mr provider list
mr provider show <provider>
mr provider enable <provider>
mr provider disable <provider>
mr provider test <provider> [--model <model>]
mr provider remove <provider> [--force]
```

`provider add` can use a catalogue entry or a custom endpoint; inspect `mr provider add --help` for `--dialect`, `--base-url`, auth, headers, and timeout options. `provider test` makes one real authenticated network request, which may consume quota or incur a charge. `provider remove --force` can also remove that provider's vault entries and combo steps; check the impact before using it.

## Provider accounts and credentials

```bash
mr account add <provider> --secret-stdin [--label <label>] [--credit-usd <USD>] [--tag <TAG>] [--model-allow <MODEL>] [--model-deny <MODEL>]
mr account list <provider>
mr account enable <provider> <account>
mr account disable <provider> <account>
mr account set-priority <provider> <account> <priority>
mr account set-credit <provider> <account> --usd <USD>
mr account set-credit <provider> <account> --clear
mr account remove <provider> <account> [--force]
```

Use `--secret-stdin` and paste the upstream credential into standard input in your own terminal, then send EOF. Do not pass real credentials through `--secret <value>` or put them in a command, shell history, or support message. The CLI also accepts `--secret-env <VAR>`, but environment-variable exposure is environment-dependent. See [Security](11-security.md).

`--credit-usd` and `set-credit` declare a balance the router cannot read for itself. The figure is a floor for one decision — starting a fine-tuning job, which requires more than $15 of known credit — not a spend meter, and `--clear` returns the account to "unknown" rather than zero. See [key selection](../05-key-selection.md#the-declared-balance-and-the-one-route-ranked-by-it).

`--model-allow` and `--model-deny` (repeatable) restrict which models this credential may serve. A pattern is a model id or a prefix ending in `*` (for example `claude-*`); a deny entry always wins over an allow entry, and an account with no allow patterns serves every model. `mr account list` shows the result in its `MODELS` column. The relay enforces these lists per request; see [key selection §5](../05-key-selection.md#5-models_allow--models_deny-and-where-they-apply).

## Models and combos

```bash
mr models list
mr models list <provider>
mr models list --source synced
mr models sync <provider>
mr models sync --all
mr combo add <name> --step <provider/model>
mr combo list
mr combo show <name>
mr combo disable <name>
mr combo enable <name>
mr combo remove <name> [--force]
```

`mr models list` reads configured data. `mr models sync` contacts the upstream and saves its model list. A combo can contain multiple repeated `--step <provider/model>` options; `mr combo add --help` lists strategy and other options. `combo remove --force` is needed when a client key or alias references the combo by name.

Aliases are also available:

```bash
mr alias list
mr alias add <from> <to>
mr alias remove <from>
```

## Relay client keys

```bash
mr key create --label <label> --print
mr key list
mr key disable <id>
mr key enable <id>
mr key rotate <id> --print
mr key remove <id>
```

A newly created or rotated key is printed once; save it securely and do not share it. Rotation invalidates the old key. `mr key create --help` shows optional `--allow-models`, `--allow-routes`, `--allow-combos`, `--limit-requests`, and `--note` restrictions. `mr key remove` deletes a key immediately and any client still using it starts receiving 401 responses, so retire the client first.

## Egress people

```bash
mr person add <tag> --proxy <proxy>
mr person list
mr person remove <tag>
```

A *person* is a tag plus the proxy their accounts egress through (for example a Tailscale peer's
SOCKS5 endpoint). Declare a person once, then tag accounts with `--tag` in `mr account add`; the
relay routes those accounts through that proxy. Removing a person keeps the tag on the accounts —
they simply stop being routed through a proxy.

## Jobs — custom GPU work

```bash
mr jobs submit --backend demo --image python:3.12-slim --command python --command render.py --env STEPS=50 --max-seconds 3600
mr jobs list [--limit 20] [--after <id>]
mr jobs show <id>
mr jobs cancel <id>
mr jobs wait <id> --timeout 10m
```

Submits to the relay's job lane (`/v1/jobs`) — a rented card (`--backend vast`) or the free local
simulator (`--backend demo`). `--gpu`, `--gpu-count` (1–8), `--disk-gb` and `--env NAME=VALUE`
(repeatable) shape the request; `--body <FILE>` takes a raw JSON submission instead of flags.
Jobs authenticate with the **client key** (the same key that calls a model), read from
`MR_CLIENT_KEY` or piped on stdin — never a flag, because a flag lands in shell history.

`wait` polls until the job reaches a terminal state and exits with it: `0` on `succeeded`, `1` on
`failed`/`cancelled`, so `&&` chains in an agent script behave. Artifacts are fetched by name from
`GET /v1/jobs/<id>/artifacts/<name>`; see [34-agent-gpu-jobs.md](../34-agent-gpu-jobs.md) for the
agent's guide, and [27-gpu-jobs.md](../27-gpu-jobs.md) for the lane itself.

A real rental bills for the whole `max_seconds` window whether the command is working or not;
develop the command on `demo` first.

## Start and diagnose

```bash
mr serve
mr serve --relay-port <port> --control-port <port>
mr doctor
mr version
```

`mr serve` runs relay and conductor together in the foreground; Ctrl-C stops the foreground session. Port options override the configured ports for that invocation. The default relay API port is 20900 and panel/control port is 20901. Both listeners bind the host configured in `config.json` (loopback by default); do not move them to a public interface without authentication, TLS, and a firewall. For unattended operation, use the deployment instructions in the project README rather than treating this foreground command as a service manager.

`mr doctor` is a read-only diagnostic: it checks config validation, vault metadata, snapshot status, client keys, and listener addresses. It does not probe/bind ports, contact providers, open the vault, or repair anything. `mr version` prints the version and configuration locations.

## Configuration and vault command status

The CLI lists the following command names, but their implementations currently return “not implemented yet”:

```text
mr config path|validate|show|edit|set-port
mr vault list|rekey
```

Do not rely on them to inspect, edit, validate, or rekey configuration/vault files. Use `mr doctor` for supported diagnostics and `mr version` to find the configuration path. `mr doctor` is not a substitute for vault repair.

## Other commands

`mr migrate omniroute` previews an import by default. The implementation reads the source database without modifying the original and only applies changes to Manarouter when explicitly passed `--apply`; inspect `mr migrate --help` before using it. This is a state-changing import and should be planned with a backup.

Global options: `--help`, `--json`, and `--config-dir <DIR>`; `mr --version` prints the CLI version but is only accepted before the subcommand. For exact positional arguments, accepted values, and flags, the installed binary's own `--help` is authoritative.

## Related guides

- [Troubleshooting](10-troubleshooting.md)
- [Security and credentials](11-security.md)
