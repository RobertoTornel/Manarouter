This guide explains how to create and protect the client keys your apps use to call Manarouter.

# Client keys

A **client key** is for an app that calls Manarouter's relay. A **provider credential** is for Manarouter to authenticate to an AI provider. They are different secrets: give an app only its client key, never the upstream provider credential. Client keys are not used to sign in to the web panel; the panel has its own local access controls.

## Create a client key

Create a key for one app or deployment and use `--print` to make its one-time value easy to capture:

```bash
mr key create --label my-app --print
```

Expect the bare key on standard output and confirmation on standard error. Save it in your app's secret manager or another secure store immediately. The plaintext is shown once; Manarouter stores a hash and a short display prefix, so `mr key list` cannot recover it. Do not put the key in source control, a shared terminal transcript, or a support message.

Use the key from your app as a bearer token:

```bash
curl http://127.0.0.1:20900/v1/models \
  -H 'Authorization: Bearer YOUR_CLIENT_KEY'
```

Replace `YOUR_CLIENT_KEY` with the client key saved from your terminal. Expect a JSON list of models the relay can route. Keep the key on the app side: the provider credential stays inside Manarouter.

## Disable or rotate a key

To stop a key from being accepted without deleting its record, disable it by the key ID shown in `mr key list`:

```bash
mr key disable KEY_ID
```

Expect the key to be rejected at authentication. To restore it, run `mr key enable KEY_ID`.

If the secret may have leaked, rotate it:

```bash
mr key rotate KEY_ID --print
```

Expect a replacement plaintext once, and the old key to become invalid immediately. Save the replacement in your app's secret store and update the app; any callers still using the old value will fail. `mr key remove KEY_ID` deletes the key record instead. Review which apps use it before disabling, rotating, or removing a shared key.

## Limits and current controls

The data format has optional rolling limits for **5 hours**, **1 week**, and **30 days**. Each window can cap tokens, requests, or both; zero means that dimension is uncapped. These rolling limits are not exposed in `mr key create` or the panel key form, and the relay does not currently enforce or report them. Do not rely on them as active quota controls.

The CLI and panel do expose a separate `--limit-requests` request-count field, but the relay does not currently enforce that field either. The panel key view shows the configured request limit, allowed models/routes, and enabled state; it does not show rolling-window usage. The relay currently uses client keys only to authenticate requests; it does not enforce the configured model or route allow-lists either. Do not treat these fields as an access-control boundary.

## Understand 401 and 429 responses

- **401 Unauthorized:** check that the app is sending the correct client key in `Authorization: Bearer …`, that the key is enabled, and that it was not rotated. A 401 from the upstream may also indicate the provider credential was rejected; distinguish it from the relay's invalid-client-key error before changing credentials.
- **429 Too Many Requests:** this is not currently a per-key rolling-limit response. If the response body has error code `no_usable_accounts`, the relay could not find an available account for the requested model/route (for example, accounts are cooling down). Otherwise, the status may be from the upstream provider. Check the response body and provider/account status; avoid repeatedly retrying a provider that is rate-limiting you.

For a first end-to-end request, see [getting started](./00-getting-started.md). To add or replace a provider credential safely, see [add a provider](./01-add-a-provider.md).
