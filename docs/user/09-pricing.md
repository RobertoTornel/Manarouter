# Pricing

Use this guide to understand what Manarouter currently does—and does not—show about model prices and request costs; always confirm current rates with your provider before budgeting or sending paid traffic.

## Current status

**Partial support — model rates can be stored, displayed, and synced from the bundled table, but live provider pricing and cost estimates are not available.** Model details and the Models view can show input/output rates already stored on a model. `POST /api/pricing/sync` and the Settings **Sync bundled prices** control apply matching entries from the bundled LiteLLM-derived price table to configured models and preserve manual prices. They do not query provider `/models` endpoints. Newly discovered models start with no price, and model-list sync does not populate prices. Do not interpret a missing price as free.

Manarouter does not show estimated request or quota costs. Do not use it as a billing or cost-control system.

## What to rely on

Check your provider's current public price page or account billing tools for the model and usage type you intend to use. Rates can vary by input versus output tokens, cache use, modality, region, and provider plan. The displayed per-million-token rates are model metadata only—not a calculation of what a request will cost or a statement of actual provider charges.

The price model distinguishes unknown rates from a known zero rate. Requests may incur upstream charges even when Manarouter shows no rate. For safe diagnostics and account protection, see [Troubleshooting](10-troubleshooting.md) and [Security and credentials](11-security.md).
