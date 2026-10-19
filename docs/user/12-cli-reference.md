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
mr account add <provider> --secret-stdin [--label <label>]
mr account list <provider>
mr account enable <provider> <account>
mr account disable <provider> <account>
mr account set-priority <provider> <account> <priority>
mr account remove <provider> <account> [--force]
```

Use `--secret-stdin` and paste the upstream credential into standard input in your own terminal, then send EOF. Do not pass real credentials through `--secret <value>` or put them in a command, shell history, or support message. The CLI also accepts `--secret-env <VAR>`, but environment-variable exposure is environment-dependent. See [Security](11-security.md).

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

A newly created or rotated key is printed once; save it securely and do not share it. Rotation invalidates the old key. `mr key create --help` shows optional `--allow-models`, `--allow-routes`, `--limit-requests`, and `--note` restrictions.

## Start and diagnose

```bash
mr serve
mr serve --relay-port <port> --control-port <port>
mr doctor
mr version
```

`mr serve` runs relay and conductor together in the foreground; Ctrl-C stops the foreground session. Port options override the configured ports for that invocation. The default relay API port is 20900 and panel/control port is 20901. For unattended operation, use the deployment instructions in the project README rather than treating this foreground command as a service manager.

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

Global options: `--help`, `--version`, `--json`, and `--config-dir <DIR>`. For exact positional arguments, accepted values, and flags, the installed binary's own `--help` is authoritative.

## Related guides

- [Troubleshooting](10-troubleshooting.md)
- [Security and credentials](11-security.md)
