// gpu-setup-model.js — pure logic for the GPU rental setup flow (platform → account →
// model → card → sizing → review).
//
// Nothing here touches the DOM, the network, `document` or `window`: every export is a
// plain function over plain objects, so it runs and is tested under bare `node --test`.
// The view layer (`panel/view-gpu-rentals.js` and whatever renders the steps) owns the
// pixels; this file owns the arithmetic, the legality filtering and the generated
// deployment bundle.
//
// The rules encoded here come from `docs/research/ARCHITECTURE-gpu-rentals.md`, which is
// the authoritative contract, and from the research files it cites:
//
//   * §5  — `(Paid)` is shown only when the plan is *known* and the catalog says the card
//           is `free_plan: "unavailable"`. `"unknown"` never renders as free or paid.
//   * §6  — weight quant and KV dtype are separate axes with separate legality; the
//           catalog carries the numbers, the panel never hardcodes a GPU, price or flag.
//   * §7  — the KV pool is an *estimate* with its formula, assumptions and a measured
//           delta beside it. vLLM's own startup line is authoritative.
//   * §8  — a quant the repo does not ship is a recorded *recipe*, never an action.
//   * §11 — no invented Modal REST call, no live `modal deploy`, no claim that the
//           estimate equals a measurement, no `(Paid)` on an unknown free tier.

/** The Modal GPU ids that the generated bundle may name, per `modal-platform.md` §Q1. */
export const MODAL_GPU_IDS = Object.freeze([
  'T4', 'L4', 'A10', 'L40S', 'A100', 'A100-40GB', 'A100-80GB',
  'RTX-PRO-6000', 'H100', 'H200', 'B200', 'B300',
]);

/**
 * The GPU id the scaffold used that Modal no longer publishes.
 *
 * Kept as data rather than a hardcoded filter so the generated bundle can *say* what is
 * wrong with the id instead of silently rewriting the operator's choice.
 */
export const RENAMED_MODAL_GPU_IDS = Object.freeze({ A10G: 'A10' });

/** Modal `scaledown_window` bounds, verified in `modal-platform.md` (default 60, range 2–1200). */
export const SCALEDOWN_WINDOW_LIMITS = Object.freeze({ min: 2, max: 1200, default: 60 });

/** vLLM `--block-size` default (`vllm-sizing-and-quant.md` A.2). Pool tokens round down to it. */
export const BLOCK_SIZE = 64;

/** vLLM `DEFAULT_GPU_MEMORY_UTILIZATION` (`vllm-sizing-and-quant.md` A.2, corrected from 0.9). */
export const DEFAULT_GPU_MEMORY_UTILIZATION = 0.92;

/**
 * The activation / CUDA-graph / block-padding overhead subtracted from the VRAM budget, §7.1.
 *
 * 8 GiB is a **calibrated default, not a physical constant**: vLLM documents no separate figure,
 * and the operator's two measurements imply 7.27 GiB (RTX PRO 6000) and 7.85 GiB (L40S)
 * independently — a spread of 0.58 GiB. The sizing result says so in `assumptions`, and the code
 * never tunes the value silently to match a measurement. The earlier 4 GiB / 10.7 GiB figures in
 * the research drafts were both wrong; the canonical example's arithmetic (58 461 515 351 bytes of
 * budget on 96 GiB) only reproduces with 2**33.
 */
export const OVERHEAD_BYTES = 8 * 1024 ** 3; // 8 589 934 592

/** Deprecated alias kept so an existing import does not silently pick up a stale figure. */
export const ACTIVATION_RESERVE_BYTES = OVERHEAD_BYTES;

/** Names the pinned form of an H100 (`gpu="H100!"`) when the operator means 80 GiB, §2.1. */
export const H100_PIN_ID = 'H100!';

/**
 * The H100 auto-upgrade, §2.1: `gpu="H100"` may be served on an H200 at no extra cost, so a KV
 * budget computed for 80 GiB would be wrong. The bundle therefore pins H100 and says why.
 */
export const H100_AUTO_UPGRADE = Object.freeze({
  id: 'H100',
  pinned: H100_PIN_ID,
  mayUpgradeTo: 'H200',
  note: 'Modal may serve gpu="H100" on an H200 at no extra cost, which changes the VRAM (and therefore the KV budget) without warning. The bundle pins gpu="H100!" so the sizing stays true to the card it was computed for; drop the "!" to accept the upgrade and re-run the sizing step.',
});

/** B300 needs CUDA 13.1+; a CUDA-12 image cannot drive the card (§2.1). */
export const B300_CUDA_MIN_VERSION = '13.1';

/** Catalog GPU ids whose per-container card limit is *not* the default, §2.1 (`A10` is 4, not 8). */
export const MODAL_GPU_COUNT_LIMITS = Object.freeze({ A10: 4 });

/** Cards per container when neither the catalog nor `MODAL_GPU_COUNT_LIMITS` says otherwise. */
export const GPU_COUNT_LIMIT_DEFAULT = 8;

/**
 * Requantisation planning constants (§8, `requantize-recipes.md` §5).
 *
 * `K_GPU` is a CHOSEN margin, not a published figure: llm-compressor's published GPU term is
 * `mem(1 layer)` for RTN and twice that for GPTQ, with no allowance for calibration activations,
 * traced intermediates, the quantised copy of a layer, or the whole vision tower loading at once.
 * The plan names it as an assumption wherever it is reported.
 */
export const REQUANTIZE_K_GPU = 4;
/** Vision towers load onto the GPU whole and are never quantised; 1–4 GiB is [unverified], so use the top. */
export const REQUANTIZE_VISION_TOWER_BYTES = 4 * 1024 ** 3;
/** CUDA context, kernels and allocator fragmentation. [reasoned], not published. */
export const REQUANTIZE_FIXED_GPU_BYTES = 2 * 1024 ** 3;
/** Host-RAM margin over the BF16 model: `params x 2 x 1.25`. */
export const REQUANTIZE_K_RAM = 1.25;
/** Output checkpoint slack over `params x bytes/param`, absorbing unquantised residue. */
export const REQUANTIZE_OUTPUT_SLACK = 1.2;
/** 4-bit assumption when the catalog prices no `bytes_per_param` for the requested target. */
export const REQUANTIZE_TARGET_BPP_FALLBACK = 0.5;
/** The tool vLLM's own docs recommend. AutoAWQ is deprecated and is deliberately not named (§8.4). */
export const REQUANTIZE_TOOL = 'llm-compressor 0.15.0';
/** Dense round-trip: the measured anchor is 3.9 h for GPTQ on a Qwen3-30B-A3B, reasoned to 450–515 min. */
export const REQUANTIZE_DENSE_MINUTES = 480;
export const REQUANTIZE_DENSE_MINUTES_RANGE = Object.freeze([450, 515]);
/** One-pass `model_free_ptq`: 20–45 min, at the price of RTN instead of AWQ. */
export const REQUANTIZE_MODEL_FREE_MINUTES = 32;
export const REQUANTIZE_MODEL_FREE_MINUTES_RANGE = Object.freeze([20, 45]);

/**
 * vLLM version the generated image pins.
 *
 * Provenance: `docs/research/issue-4-spec.md` [observed] — the operator's own deployment of
 * this model ran vLLM 0.24.0. It is a *pin*, not a claim about the latest release; a catalog
 * that carries `vllm_version` overrides it.
 */
export const DEFAULT_VLLM_VERSION = '0.24.0';

/** Context lengths the sizing step reports concurrency for, before `max_model_len` is applied. */
export const SIZING_CONTEXT_LADDER = Object.freeze([2048, 8192, 32768]);

/** Step order. Requirement, not preference: an account exists before a model is chosen. */
export const STEP_ORDER = Object.freeze(['platform', 'account', 'model', 'card', 'sizing', 'review']);

/** Plan ids, matching `RentalPlan` in `crates/mr-core/src/rental.rs`. */
export const PLAN_IDS = Object.freeze(['free', 'paid', 'unknown']);

/** Bytes per element for the model dtypes a HF `config.json` can name. */
const MODEL_DTYPE_BYTES = Object.freeze({
  bfloat16: 2, bf16: 2, float16: 2, fp16: 2, half: 2,
  float32: 4, fp32: 4, float: 4,
  float8_e4m3fn: 1, float8_e5m2: 1, fp8: 1,
});

/** KV dtypes whose byte width is implied by the name when the catalog omits it. */
const KV_DTYPE_IMPLIED_BYTES = Object.freeze({
  fp8: 1, fp8_e4m3: 1, fp8_e5m2: 1, fp8_ds_mla: 1,
  bfloat16: 2, float16: 2, fp16: 2, bf16: 2, float32: 4,
});

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

function hasOwn(object, key) {
  return object != null && Object.prototype.hasOwnProperty.call(object, key);
}

function asArray(value) {
  return Array.isArray(value) ? value : [];
}

function asObject(value) {
  return value != null && typeof value === 'object' && !Array.isArray(value) ? value : {};
}

function text(value) {
  return typeof value === 'string' ? value : '';
}

/** Coerce an input value (the panel's inputs are strings) to a finite number or `null`. */
export function toNumber(value) {
  if (value == null) return null;
  if (typeof value === 'number') return Number.isFinite(value) ? value : null;
  // A field the operator has not typed into yet arrives as '' or as whitespace; both mean "no
  // value", not 0. `Number('')` is 0, which would silently size a deployment at zero cards.
  const trimmed = String(value).trim();
  if (trimmed === '') return null;
  const number = Number(trimmed);
  return Number.isFinite(number) ? number : null;
}

function toInteger(value) {
  const number = toNumber(value);
  return number == null || !Number.isInteger(number) ? number == null ? null : Math.trunc(number) : number;
}

function round1(value) {
  return Math.round(value * 10) / 10;
}

/** `delta_vs_measurement_pct` is reported at two decimals: the canonical example's 0.36. */
function round2(value) {
  return Math.round(value * 100) / 100;
}

function gib(bytes) {
  const n = toNumber(bytes);
  return n == null ? null : round2(n / 1024 ** 3);
}

function firstDefined(...values) {
  for (const value of values) {
    if (value !== undefined && value !== null) return value;
  }
  return null;
}

/** Parse a compute capability out of `8.9`, `"8.9"`, `"sm_89"`, `89` or `{major,minor}`. */
function parseComputeCap(value) {
  if (value == null) return null;
  if (typeof value === 'object') {
    const major = toNumber(value.major);
    const minor = toNumber(value.minor);
    return major == null || minor == null ? null : major + minor / 10;
  }
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) return null;
    // 89 / 86 style integers are major*10+minor. The threshold is 20, not 10, because a card
    // declared as `compute_capability: 10.0` arrives here as the integer 10 and must read as
    // capability 10.0 — not as sm_10 (capability 1.0). No real card sits between 20 and 100 in
    // the major*10 form below capability 2.0.
    if (Number.isInteger(value) && value >= 20) return Math.floor(value / 10) + (value % 10) / 10;
    return value;
  }
  const raw = String(value);
  const decimal = raw.match(/(\d+)\.(\d+)/);
  if (decimal) return Number(decimal[1]) + Number(`0.${decimal[2]}`);
  const sm = raw.match(/sm[_-]?(\d)(\d)/i);
  if (sm) return Number(sm[1]) + Number(sm[2]) / 10;
  const integer = raw.match(/^\d+$/);
  if (integer) return parseComputeCap(Number(raw));
  return null;
}

function compareCc(value, minimum) {
  return value + 1e-9 >= minimum;
}

function describeCc(value) {
  return value == null ? 'unknown' : value.toFixed(1);
}

function slugify(value) {
  return String(value ?? '')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '');
}

/**
 * The trailing `-<mode>-<card>[-x<n>]` half of every derived name.
 *
 * Mirrors `DeploymentShape::suffix` in `crates/mr-core/src/rental.rs`. It exists because one
 * repository may be deployed several times under one account — stream on one card, batch that
 * ships a half-full batch on another, batch that waits for a full one on a third — and two of
 * those must never share a provider id or a Modal app.
 *
 * @param {string} mode
 * @param {string} gpu
 * @param {number} gpuCount
 * @returns {string}
 */
export function deploymentSuffix(mode, gpu, gpuCount) {
  const count = Math.max(1, Math.trunc(toNumber(gpuCount) ?? 1));
  let suffix = `-${text(mode) === 'batch' ? 'batch' : 'streaming'}`;
  const card = slugify(gpu);
  if (card) suffix += `-${card}`;
  if (count > 1) suffix += `-x${count}`;
  return suffix;
}

/**
 * The stable part of the app name: `rental-<account>-<repo-name>-<mode>-<card>`, at most 64
 * characters.
 *
 * Mirrors `mr_core::rental::app_slug`. One string identifies both the provider id (built from it
 * in the conductor) and the Modal app, so the preview and the deployed name cannot drift.
 * Truncation eats the account/repository half and never the suffix: the suffix is the only part
 * that tells two deployments of one checkpoint apart.
 *
 * @param {string} accountId
 * @param {string} repo
 * @param {string} mode
 * @param {string} gpu
 * @param {number} gpuCount
 * @returns {string}
 */
export function appSlug(accountId, repo, mode, gpu, gpuCount) {
  const name = String(repo ?? '').split('/').filter(Boolean).pop() ?? '';
  const suffix = deploymentSuffix(mode, gpu, gpuCount);
  const room = Math.max(0, 64 - suffix.length);
  let head = `rental-${slugify(accountId)}-${slugify(name)}`;
  if (head.length > room) head = head.slice(0, room);
  head = head.replace(/-+$/, '');
  return `${head}${suffix}`;
}

/**
 * The Modal app name the bundle deploys, `manarouter-<app-slug>`.
 *
 * @param {string} accountId
 * @param {string} repo
 * @param {string} mode
 * @param {string} gpu
 * @param {number} gpuCount
 * @returns {string}
 */
export function modalAppName(accountId, repo, mode, gpu, gpuCount) {
  return `manarouter-${appSlug(accountId, repo, mode, gpu, gpuCount)}`;
}

/**
 * The endpoint URL Modal is expected to publish for this draft.
 *
 * `https://<workspace>--<app-name>-<function>.modal.run`, dots in the app name becoming dashes,
 * with the generated bundle's single web function named `serve`. `null` when a part is missing:
 * the caller must ask for an explicit URL rather than invent one. A card is required for the same
 * reason: without it the name would describe every deployment of the repository at once.
 *
 * The same derivation is `mr_core::rental::modal_base_url`; the two are kept in step by a golden
 * value on each side.
 *
 * @param {object} draft
 * @returns {string|null}
 */
export function modalBaseUrl(draft) {
  const d = normalizeDraft(draft);
  const workspace = slugify(d.workspace);
  const account = slugify(d.accountId);
  const name = slugify((d.repo.split('/').filter(Boolean).pop()) ?? '');
  if (!workspace || !account || !name || !slugify(d.gpu)) return null;
  // UNCERTAIN: the URL shape and the `serve` function half are reverse-engineered from Modal's
  // documented app naming. `modal deploy` prints the real URL, which is authoritative if it differs.
  return `https://${workspace}--${modalAppName(d.accountId, d.repo, d.mode, d.gpu, d.gpuCount).replace(/\./g, '-')}-serve.modal.run`;
}

/** Quote a JS string as a Python string literal without inventing escapes. */
function pyString(value) {
  return JSON.stringify(String(value));
}

// ---------------------------------------------------------------------------
// draft
// ---------------------------------------------------------------------------

/**
 * A blank draft for one platform.
 *
 * Defaults are the documented ones, not guesses: plan `"unknown"` (an unrecorded plan must
 * never read as free), `scaledown_window` 60 and `gpu_memory_utilization` 0.92 are the
 * vendors' own defaults, `mode` is the Rust default, `kv_cache_dtype` is vLLM's `auto`.
 *
 * @param {string} [platformId]
 * @returns {object} Draft
 */
export function emptyDraft(platformId) {
  return {
    platform: typeof platformId === 'string' ? platformId.trim() : '',
    // `new` creates the account this rental bills through; `existing` reuses one already recorded,
    // which is the normal case once an account has been created: one account may host several
    // rentals, and the account's credential is written once.
    accountMode: 'new',
    accountId: '',
    // Shown on the list instead of the id when the operator gives one. Empty falls back to the id,
    // which is what a flow that never asked for a label has always recorded.
    accountLabel: '',
    // The Modal workspace the account's apps deploy into. Not a secret: it is the half of the
    // endpoint URL the panel can know without deploying anything.
    workspace: '',
    plan: 'unknown',
    tokenId: '',
    tokenSecret: '',
    // A Hugging Face token is named, never pasted here: `hfTokenId` is an id from the recorded
    // tokens (`hf_tokens` in `GET /api/gpu/rentals`). `hfTokenNew`/`hfTokenNewLabel` hold a token
    // being added in the same step, so a gated repository does not force a second visit.
    hfTokenId: '',
    hfTokenNew: '',
    hfTokenNewLabel: '',
    hfTokenNeeded: false,
    repo: '',
    revision: '',
    gpu: '',
    gpuCount: 1,
    weightQuant: '',
    kvCacheDtype: 'auto',
    maxModelLen: null,
    maxNumSeqs: null,
    shutdownCooldownSecs: SCALEDOWN_WINDOW_LIMITS.default,
    mode: 'streaming',
    // How many sessions this deployment is asked to hold at once. Streaming is not "one request
    // at a time": one vLLM process serves several, so both modes carry a concurrency figure and
    // the fit check judges the card against it.
    sessions: 1,
    // A batch deployment's fill policy. `full` waits for `batchSize` requests however long that
    // takes, which is the operator's informed choice; `timeout` ships a half-full batch after
    // `batchTimeoutSecs`.
    batchSize: 8,
    batchFill: 'full',
    batchTimeoutSecs: 60,
    // A quant the repository does not ship, recorded as a future GPU job. The bundle still serves
    // what the repository ships; this field is never executed by the wizard.
    requantizeTo: '',
    gpuMemoryUtilization: DEFAULT_GPU_MEMORY_UTILIZATION,
    // Empty by default: the URL is derived from the workspace and the app name. An explicit value
    // here is an override for when `modal deploy` prints something different.
    baseUrl: '',
    sizing: null,
    probe: null,
  };
}

/** Fill a partial draft with the documented defaults, without mutating the input. */
export function normalizeDraft(draft) {
  const given = asObject(draft);
  const merged = { ...emptyDraft(given.platform), ...given };
  merged.platform = text(merged.platform);
  merged.accountMode = text(merged.accountMode) === 'existing' ? 'existing' : 'new';
  merged.accountId = text(merged.accountId);
  merged.accountLabel = text(merged.accountLabel);
  merged.hfTokenId = text(merged.hfTokenId);
  merged.hfTokenNew = text(merged.hfTokenNew);
  merged.hfTokenNewLabel = text(merged.hfTokenNewLabel);
  // A pre-fix draft pasted the token value onto the model step. It is dropped rather than carried
  // into a payload: a value is written once, by the token action, and never round-trips.
  delete merged.hfToken;
  merged.workspace = text(merged.workspace);
  merged.plan = text(merged.plan) || 'unknown';
  merged.repo = text(merged.repo);
  merged.revision = text(merged.revision);
  merged.gpu = text(merged.gpu);
  merged.weightQuant = text(merged.weightQuant);
  merged.kvCacheDtype = text(merged.kvCacheDtype) || 'auto';
  // `interactive` is the pre-fix spelling of `streaming`; an old draft keeps loading, but nothing
  // new is ever written with it.
  merged.mode = text(merged.mode) === 'batch' ? 'batch' : 'streaming';
  merged.sessions = Math.max(1, Math.trunc(toNumber(merged.sessions) ?? 1));
  merged.batchSize = Math.max(1, Math.trunc(toNumber(merged.batchSize) ?? 8));
  merged.batchFill = text(merged.batchFill) === 'timeout' ? 'timeout' : 'full';
  merged.batchTimeoutSecs = Math.max(1, Math.trunc(toNumber(merged.batchTimeoutSecs) ?? 60));
  merged.requantizeTo = text(merged.requantizeTo);
  merged.baseUrl = text(merged.baseUrl);
  // A pre-fix draft could name a second card for the batch lane. A rental is one model on one card,
  // so the stale key is dropped rather than carried around and echoed by some later payload.
  delete merged.batchGpu;
  if (merged.sizing === undefined) merged.sizing = null;
  if (merged.probe === undefined) merged.probe = null;
  if (merged.maxModelLen === undefined) merged.maxModelLen = null;
  if (merged.maxNumSeqs === undefined) merged.maxNumSeqs = null;
  return merged;
}

// ---------------------------------------------------------------------------
// catalog
// ---------------------------------------------------------------------------

/**
 * Index a `GET /api/gpu/catalog` body for lookup.
 *
 * Tolerant on purpose: the catalog is produced by another workstream and a missing key must
 * degrade to "unknown" rather than throw. `platformById`/`gpuById` are plain objects, and
 * every lookup goes through `hasOwn`, so a GPU literally named `constructor` cannot be
 * confused with `Object.prototype`.
 *
 * @param {object} catalog
 * @returns {{platformById: object, gpuById: object, quantById: object, kvDtypeById: object}}
 */
export function catalogIndex(catalog) {
  const body = asObject(catalog);
  const platformById = {};
  const gpuById = {};
  const gpuPlatformById = {};
  const platforms = asArray(body.platforms);

  for (const platform of platforms) {
    const entry = asObject(platform);
    const id = text(entry.id);
    if (!id) continue;
    platformById[id] = entry;
    for (const gpu of asArray(entry.gpus)) {
      const card = asObject(gpu);
      const gpuId = text(card.id);
      if (!gpuId) continue;
      // First platform wins for a shared card; the per-platform entry stays reachable via
      // `platformById[platform].gpus`.
      if (!hasOwn(gpuById, gpuId)) {
        gpuById[gpuId] = card;
        gpuPlatformById[gpuId] = id;
      }
    }
  }

  const quantById = {};
  for (const quant of asArray(body.quant_methods ?? body.quantMethods ?? body.quants)) {
    const entry = asObject(quant);
    const id = text(entry.id);
    if (id) quantById[id] = entry;
  }

  const kvDtypeById = {};
  for (const dtype of asArray(body.kv_cache_dtypes ?? body.kvCacheDtypes ?? body.kv_dtypes)) {
    const entry = asObject(dtype);
    const id = text(entry.id);
    if (id) kvDtypeById[id] = entry;
  }

  return {
    platformById,
    gpuById,
    quantById,
    kvDtypeById,
    // extra, non-frozen conveniences for the view layer
    gpuPlatformById,
    platforms,
    quants: asArray(body.quant_methods ?? body.quantMethods ?? body.quants),
    kvDtypes: asArray(body.kv_cache_dtypes ?? body.kvCacheDtypes ?? body.kv_dtypes),
    measuredReferences: asArray(body.measured_references ?? body.measuredReferences),
    catalog: body,
  };
}

function gpuEntry(index, gpuId) {
  const id = text(gpuId);
  return id && hasOwn(index.gpuById, id) ? index.gpuById[id] : null;
}

/**
 * Cards per container for one GPU id, §2.1.
 *
 * The catalog's own figure wins when it states a positive one; otherwise the known Modal limit;
 * otherwise the default. A catalog that is silent leaves the default, which is what `H100:8` is —
 * and `A10` is the one card whose published limit is lower (4), which the *card* decides, not the
 * catalog's absence.
 *
 * @param {string|null} gpuId
 * @param {object|null} catalog
 * @returns {number}
 */
export function maxGpuCountFor(gpuId, catalog) {
  const index = catalogIndex(catalog);
  const gpu = gpuEntry(index, gpuId);
  const declared = toNumber(firstDefined(
    gpu?.max_gpu_count, gpu?.maxGpuCount, gpu?.max_per_container, gpu?.maxPerContainer,
  ));
  if (declared != null && declared > 0) return Math.trunc(declared);
  const id = text(gpuId);
  if (id && hasOwn(MODAL_GPU_COUNT_LIMITS, id)) return MODAL_GPU_COUNT_LIMITS[id];
  return GPU_COUNT_LIMIT_DEFAULT;
}

/** Resolve a GPU's compute capability from whichever of the catalog's spellings carries it. */
function gpuComputeCap(index, gpuId, gpu) {
  const shared = asObject(index.catalog.compute_capabilities ?? index.catalog.computeCapabilities);
  const fromGpu = firstDefined(
    gpu?.compute_capability, gpu?.compute_cap, gpu?.cc, gpu?.sm, gpu?.capability,
  );
  const fromCatalog = hasOwn(shared, text(gpuId)) ? shared[text(gpuId)] : null;
  return parseComputeCap(firstDefined(fromGpu, fromCatalog));
}

function quantMinCc(quant) {
  return parseComputeCap(firstDefined(
    quant?.min_compute_cap, quant?.minComputeCap, quant?.min_compute_capability, quant?.min_cc,
  ));
}

function dtypeMinCc(dtype) {
  return parseComputeCap(firstDefined(
    dtype?.min_compute_cap, dtype?.minComputeCap, dtype?.min_compute_capability, dtype?.min_cc,
  ));
}

/**
 * Verdict for one catalog entry against one card.
 *
 * `legal` is tri-state. `null` means "the catalog does not carry enough to decide" and the UI
 * must say so; it must never be rounded to `true`.
 */
function verdictFor({ index, gpuId, entry, minCc, kind }) {
  const gpu = gpuEntry(index, gpuId);
  const label = text(entry?.label) || text(entry?.id) || 'this option';
  const id = text(entry?.id);
  if (!gpu) {
    return {
      legal: null,
      reason: `The catalog has no GPU "${text(gpuId) || '(none)'}", so the ${kind} "${id}" cannot be checked against it.`,
    };
  }
  const platformId = hasOwn(index.gpuPlatformById, gpu.id) ? index.gpuPlatformById[gpu.id] : '';

  // Explicit per-platform support, when the catalog carries it.
  const platformList = asArray(entry?.platforms ?? entry?.platform_ids);
  if (platformList.length > 0 && !platformList.map((p) => text(p)).includes(platformId)) {
    return {
      legal: false,
      reason: `${label} is only offered on ${platformList.join(', ')}, not on ${platformId || 'this platform'}.`,
    };
  }

  // Explicit per-GPU evidence beats a derived compute-capability comparison.
  const hardware = asObject(entry?.hardware ?? entry?.gpus);
  if (hasOwn(hardware, gpu.id)) {
    const value = hardware[gpu.id];
    if (typeof value === 'boolean') {
      return {
        legal: value,
        reason: value
          ? `The catalog marks ${label} as supported on ${gpu.id}.`
          : `The catalog marks ${label} as unsupported on ${gpu.id}.`,
      };
    }
    const verdict = text(value).toLowerCase();
    if (['yes', 'supported', 'true', 'ok'].includes(verdict)) {
      return { legal: true, reason: `The catalog marks ${label} as "${verdict}" on ${gpu.id}.` };
    }
    if (['no', 'unsupported', 'false'].includes(verdict)) {
      return { legal: false, reason: `The catalog marks ${label} as "${verdict}" on ${gpu.id}.` };
    }
    if (['limited', 'partial', 'unverified'].includes(verdict)) {
      return { legal: null, reason: `The catalog marks ${label} as "${verdict}" on ${gpu.id}; treat it as unverified.` };
    }
  }

  const supportedGpus = asArray(entry?.gpus);
  if (supportedGpus.map((g) => text(g)).includes(gpu.id)) {
    return { legal: true, reason: `The catalog lists ${gpu.id} as a supported card for ${label}.` };
  }
  const unsupportedGpus = asArray(entry?.unsupported_gpus ?? entry?.excluded_gpus);
  if (unsupportedGpus.map((g) => text(g)).includes(gpu.id)) {
    return { legal: false, reason: `The catalog excludes ${gpu.id} for ${label}.` };
  }

  if (minCc == null) {
    return {
      legal: null,
      reason: `The catalog does not record a compute capability for ${label}, so ${gpu.id} cannot be confirmed.`,
    };
  }
  const cc = gpuComputeCap(index, gpu.id, gpu);
  if (cc == null) {
    return {
      legal: null,
      reason: `The catalog does not record ${gpu.id}'s compute capability, so ${label}'s requirement (≥ ${describeCc(minCc)}) cannot be checked.`,
    };
  }
  const ok = compareCc(cc, minCc);
  return {
    legal: ok,
    reason: ok
      ? `${gpu.id} has compute capability ${describeCc(cc)}, which meets ${label}'s ${describeCc(minCc)}.`
      : `${label} needs compute capability ≥ ${describeCc(minCc)}; ${gpu.id} has ${describeCc(cc)}.`,
  };
}

function confidenceOf(entry, verdict) {
  const declared = text(entry?.confidence).toLowerCase();
  if (declared === 'documented' || declared === 'observed' || declared === 'unverified') return declared;
  return verdict === null ? 'unverified' : 'documented';
}

/**
 * Every catalog weight-quantisation method, with a verdict for one card.
 *
 * @param {object} catalog
 * @param {string} gpuId
 * @returns {Array<{id: string, label: string, legal: boolean|null, reason: string}>}
 */
export function quantsForGpu(catalog, gpuId) {
  const index = catalogIndex(catalog);
  return index.quants.map((quant) => {
    const entry = asObject(quant);
    const verdict = verdictFor({ index, gpuId, entry, minCc: quantMinCc(entry), kind: 'quantisation' });
    return {
      id: text(entry.id),
      label: text(entry.label) || text(entry.id),
      legal: verdict.legal,
      reason: verdict.reason,
      confidence: confidenceOf(entry, verdict.legal),
      bytesPerParam: toNumber(entry.bytes_per_param ?? entry.bytesPerParam),
      minComputeCap: quantMinCc(entry),
    };
  });
}

/**
 * Every catalog KV-cache dtype, with a verdict for one card.
 *
 * Weight quant and KV dtype are separate axes: FP8 *KV storage* needs Ampere-class CUDA
 * (11.8+) while FP8 *GEMM* needs Ada/Hopper tensor cores. The catalog carries the number;
 * this function only compares it.
 *
 * @param {object} catalog
 * @param {string} gpuId
 * @returns {Array<{id: string, label: string, legal: boolean|null, reason: string}>}
 */
export function kvDtypesForGpu(catalog, gpuId) {
  const index = catalogIndex(catalog);
  return index.kvDtypes.map((dtype) => {
    const entry = asObject(dtype);
    const verdict = verdictFor({ index, gpuId, entry, minCc: dtypeMinCc(entry), kind: 'KV-cache dtype' });
    const declaredBytes = toNumber(entry.bytes_per_element ?? entry.bytesPerElement);
    return {
      id: text(entry.id),
      label: text(entry.label) || text(entry.id),
      legal: verdict.legal,
      reason: verdict.reason,
      confidence: confidenceOf(entry, verdict.legal),
      bytesPerElement: declaredBytes ?? (hasOwn(KV_DTYPE_IMPLIED_BYTES, text(entry.id)) ? KV_DTYPE_IMPLIED_BYTES[text(entry.id)] : null),
    };
  });
}

/**
 * The `(Paid)` marker for an account's plan, and for one card when the caller names it.
 *
 * §5's rule, in full: a marker is truthful only when the plan is **known** *and* the GPU is
 * documented as `free_plan: "unavailable"`. A `"unknown"` card renders no marker, because
 * Modal documents no per-GPU free-tier gating and a badge there would be invented.
 *
 * The frozen signature has no GPU argument, so the third parameter accepts either a plan id
 * (`"paid"`) or `{ plan, gpu }`. Without a GPU there is nothing to gate, so the marker is
 * `null` and the provenance says why.
 *
 * @param {object} catalog
 * @param {string} platformId
 * @param {string|{plan?: string, gpu?: string, gpuId?: string}} plan
 * @returns {{paid: boolean, text: string|null, provenance: string}}
 */
export function planMarker(catalog, platformId, plan) {
  const index = catalogIndex(catalog);
  const spec = typeof plan === 'string' ? { plan, gpu: null } : asObject(plan);
  const planId = text(spec.plan).toLowerCase();
  const gpuId = text(firstDefined(spec.gpu, spec.gpuId));
  const platform = hasOwn(index.platformById, text(platformId)) ? index.platformById[text(platformId)] : null;

  if (!platform) {
    return {
      paid: false,
      text: null,
      provenance: `Platform "${text(platformId) || '(none)'}" is not in the catalog, so no plan marker is shown.`,
    };
  }
  if (!PLAN_IDS.includes(planId) || planId === 'unknown') {
    return {
      paid: false,
      text: null,
      provenance: 'The account plan is unknown, so no plan marker is shown: an unrecorded plan is never rendered as free or as paid.',
    };
  }
  if (!gpuId) {
    // No card chosen yet: fall back to a platform-level free-tier fact when the catalog states
    // one, and say nothing when it does not. `unknown` is never rendered as paid.
    const platformFreePlan = text(platform.free_plan ?? platform.freePlan).toLowerCase();
    if (platformFreePlan === 'unavailable') {
      return {
        paid: true,
        text: '(Paid)',
        provenance: `The planned platform records free_plan: "unavailable" at the platform level, so the plan is marked paid before a card is chosen.`,
      };
    }
    return {
      paid: false,
      text: null,
      provenance: platformFreePlan === 'available'
        ? `The platform records free_plan: "available", so no paid marker is shown.`
        : `The plan is recorded as "${planId}", but no card is selected and the catalog states no platform-level free_plan, so there is no free-tier fact to show.`,
    };
  }

  const gpu = gpuEntry(index, gpuId);
  if (!gpu) {
    return {
      paid: false,
      text: null,
      provenance: `The catalog has no GPU "${gpuId}", so no plan marker is shown.`,
    };
  }

  const freePlan = text(gpu.free_plan ?? gpu.freePlan).toLowerCase() || 'unknown';
  const source = text(gpu.source) || 'unknown';
  const provenanceBase = `${gpuId} free_plan: "${freePlan}" (source: ${source})`;

  if (freePlan === 'unavailable') {
    return {
      paid: true,
      text: '(Paid)',
      provenance: planId === 'free'
        ? `${provenanceBase}; the recorded plan is "free", which contradicts a card documented as unavailable on the free tier — the marker stands because the card requires a paid plan.`
        : `${provenanceBase}; the plan is recorded as "${planId}".`,
    };
  }
  if (freePlan === 'available') {
    return {
      paid: false,
      text: null,
      provenance: `${provenanceBase}; the card is documented as usable on the free plan.`,
    };
  }
  return {
    paid: false,
    text: null,
    provenance: `${provenanceBase}; Modal documents no per-GPU free-tier gating, so showing "(Paid)" here would be a fabrication.`,
  };
}

/**
 * `planMarker` addressed by GPU id, for callers that already know the card.
 *
 * @param {object} catalog
 * @param {string} platformId
 * @param {string} plan
 * @param {string} gpuId
 */
export function planMarkerForGpu(catalog, platformId, plan, gpuId) {
  return planMarker(catalog, platformId, { plan, gpu: gpuId });
}

// ---------------------------------------------------------------------------
// probe
// ---------------------------------------------------------------------------

/**
 * Whether the repo's files need a HuggingFace token.
 *
 * A gated repo answers 401 to an unauthenticated file fetch (`huggingface-discovery.md`), so
 * `gated: false` and `gated: "auto"|"manual"` are the only two states that matter here.
 *
 * @param {object|null} probe
 * @returns {boolean}
 */
export function requiresHfToken(probe) {
  if (probe == null || typeof probe !== 'object') return false;
  const gated = probe.gated;
  if (typeof gated === 'boolean') return gated;
  if (typeof gated === 'string') {
    const value = gated.toLowerCase();
    if (value === 'false' || value === '') return false;
    if (value === 'true' || value === 'auto' || value === 'manual') return true;
    return false;
  }
  const state = text(asObject(gated).state).toLowerCase();
  if (state === 'auto' || state === 'manual' || state === 'true') return true;
  if (asObject(gated).needs_token === true) return true;
  return false;
}

/**
 * The quant choices for a probed repo on a chosen card.
 *
 * `available` is the repo's own quants that the card can execute. Everything else is
 * `unavailable`, and a quant the repo does not ship carries a `requantize` recipe (§8):
 * recorded, shown, never run.
 *
 * @param {object|null} probe
 * @param {string} gpuId
 * @param {object} catalog
 */
export function quantOptions(probe, gpuId, catalog) {
  const index = catalogIndex(catalog);
  const verdicts = new Map(quantsForGpu(catalog, gpuId).map((entry) => [entry.id, entry]));
  const probed = asArray(probe?.quants).map(asObject);
  const inRepo = new Map();

  for (const quant of probed) {
    const id = text(quant.id);
    if (!id) continue;
    if (quant.in_repo === false) continue;
    inRepo.set(id, quant);
  }
  // A plain bf16/fp16 repo ships no quantised tensors, but it *is* servable without
  // `--quantization`: that is the `none` option, and it needs no probe entry to be legal.
  if (inRepo.size === 0 && probe != null && (probe.arch != null || probed.length === 0)) {
    inRepo.set('none', { id: 'none', label: 'None (model dtype)', in_repo: true, evidence: 'no quantisation config in config.json' });
  }

  const available = [];
  const unavailable = [];

  for (const [id, quant] of inRepo) {
    const catalogVerdict = verdicts.get(id) ?? null;
    const legal = catalogVerdict ? catalogVerdict.legal : (id === 'none' ? true : null);
    const reason = catalogVerdict
      ? catalogVerdict.reason
      : id === 'none'
        ? 'Unquantised weights: no --quantization flag is needed.'
        : `The repository ships ${id}, but the catalog has no "${id}" entry to check against ${text(gpuId) || 'this card'}.`;
    if (legal === false) {
      unavailable.push({
        id,
        label: text(quant.label) || catalogVerdict?.label || id,
        reason: `The repository ships ${id}, but this card cannot execute it: ${reason}`,
        requantize: null,
      });
      continue;
    }
    available.push({
      id,
      label: text(quant.label) || catalogVerdict?.label || id,
      legal,
      legalReason: reason,
      confidence: legal === null ? 'unverified' : (catalogVerdict?.confidence ?? 'documented'),
      bytesPerParam: toNumber(quant.bytes_per_param ?? quant.bytesPerParam) ?? catalogVerdict?.bytesPerParam ?? null,
      evidence: text(quant.evidence) || null,
      inRepo: true,
    });
  }

  for (const catalogQuant of index.quants) {
    const entry = asObject(catalogQuant);
    const id = text(entry.id);
    if (!id || inRepo.has(id)) continue;
    const catalogVerdict = verdicts.get(id);
    unavailable.push({
      id,
      label: text(entry.label) || id,
      reason: probed.length > 0
        ? `The repository ships ${[...inRepo.keys()].join(', ') || 'no quantised tensors'}; it does not ship ${id}.`
        : `No probe has resolved the repository's quants yet, so ${id} cannot be offered.`,
      requantize: requantizePlan({ probe, gpuId, catalog, targetId: id }),
    });
  }

  // Stable, readable order: repo order for available, catalog order for unavailable.
  return { available, unavailable };
}

/**
 * The quantisation plan for a quant the repo does not ship (§8).
 *
 * The measured constants of the job, each one either published or explicitly chosen:
 *
 * - `K_GPU = 4`: a **chosen** margin on llm-compressor's published `mem(1 layer)` term. The
 *   published value is 1 for RTN and 2 for GPTQ with nothing left over for calibration
 *   activations, the tracer's intermediates or the whole-model vision-tower onload (§8.5).
 * - the vision tower loads onto the GPU all at once and is not quantised; `1–4 GiB` is the
 *   research's own `[unverified]` range, so the plan uses the 4 GiB end of it.
 * - `fixed_gpu_bytes ≈ 2 GiB` is [reasoned], not published: CUDA context, kernels, fragmentation.
 * - `K_RAM = 1.25` and the `1.2` output slack are the research's derived constants.
 *
 * `feasible_on_gpu` is computed from the corrected predicate, and `false` is a legitimate
 * answer — but it is *not* the answer for either catalog card on the operator's model, and the
 * old claim that a 27B requantisation "does not fit on the card" was wrong: llm-compressor keeps
 * the model in host RAM and onloads one layer at a time, so ~12 GiB of VRAM fits a 46 GiB L40S.
 * The terms that actually bite are host RAM, scratch disk and wall clock, and they are reported.
 *
 * Two paths are offered, labelled by **quality, never by speed**: the dense round-trip (the
 * official recipe for this architecture) produces AWQ+GPTQ, and the one-pass `model_free_ptq`
 * route is faster but yields RTN — llm-compressor refuses activation calibration on it — so it
 * is presented as a lower-quality attempt, not as an equivalent button.
 *
 * No serve command is ever built with `--quantization` from `target.id`: llm-compressor writes
 * `quant_method: "compressed-tensors"`, and vLLM's `_verify_quantization` *raises* rather than
 * coerces when the flag disagrees with the checkpoint (§8.4).
 */
function requantizePlan({ probe, gpuId, catalog, targetId }) {
  const index = catalogIndex(catalog);
  const target = hasOwn(index.quantById, targetId) ? index.quantById[targetId] : { id: targetId };
  const probed = asArray(probe?.quants).map(asObject).filter((q) => q.in_repo !== false);
  const hardware = asObject(probe?.requantize);
  const params = toNumber(probe?.params?.total);
  const layers = toNumber(probe?.arch?.num_hidden_layers ?? probe?.arch?.num_layers);
  const requestedBpp = toNumber(target.bytes_per_param) ?? toNumber(asObject(hardware.target).bytes_per_param);
  const targetBpp = requestedBpp ?? REQUANTIZE_TARGET_BPP_FALLBACK;
  const targetBppAssumed = requestedBpp == null;
  const checkoutId = text(probed[0]?.id) || 'none';
  const checkoutProbe = probed[0] ?? null;
  const checkout = {
    id: checkoutId,
    label: text(asObject(index.quantById[checkoutId]).label) || text(checkoutProbe?.label) || checkoutId,
    bytes_per_param: toNumber(checkoutProbe?.bytes_per_param)
      ?? toNumber(asObject(index.quantById[checkoutId]).bytes_per_param)
      ?? 2,
    repo: text(probe?.repo) || null,
    gated: ['true', 'auto', 'manual'].includes(String(probe?.gated?.state ?? probe?.gated?.gated ?? '')),
  };
  const gpu = gpuEntry(index, gpuId);
  const vram = toNumber(gpu?.vram_bytes ?? gpu?.vramBytes)
    ?? (toNumber(gpu?.vram_gib) != null ? toNumber(gpu.vram_gib) * 1024 ** 3 : null);

  const baseModel = text(probe?.base_model) || text(asObject(hardware.source).repo) || null;

  const dense = requantizeCost({ params, layers, gptq: true, checkoutBpp: checkout.bytes_per_param, targetBpp });
  const modelFree = requantizeCost({ params, layers, gptq: false, checkoutBpp: checkout.bytes_per_param, targetBpp });

  const term = (cost) => (cost.requires_vram_bytes == null ? null : cost.requires_vram_bytes);
  const selected = dense;
  const failedOn = requantizeFailure({ vram, requiresVram: term(selected), probe, cost: selected });
  const feasible = failedOn === null ? true : (selected.requires_vram_bytes == null && vram == null ? null : false);

  const paths = [
    {
      path: 'dense',
      label: 'Higher quality · ~8 h',
      quality: 'AWQ+GPTQ over a dense BF16 round-trip — the recipe with an official example for this architecture',
      produces: 'compressed-tensors (pack-quantized, W4A16)',
      produces_note: 'AWQ is the requested label; the artefact\'s own quant_method is compressed-tensors',
      verified_for_this_architecture: true,
      estimated_minutes: REQUANTIZE_DENSE_MINUTES,
      estimated_minutes_range: [...REQUANTIZE_DENSE_MINUTES_RANGE],
      requires_vram_bytes: dense.requires_vram_bytes,
      requires_vram_bytes_exact: dense.requires_vram_bytes_exact,
      requires_disk_bytes: dense.requires_disk_bytes,
      requires_disk_bytes_exact: dense.requires_disk_bytes_exact,
      requires_host_ram_bytes: dense.requires_host_ram_bytes,
      requires_extra_vram_bytes: 0,
      steps: [
        `pip install "${text(hardware.tool) || REQUANTIZE_TOOL}"`,
        checkoutId === 'none'
          // Nothing is quantised: the checkout already is the dense BF16 directory, so there is
          // no dequantisation step and naming one would invent work.
          ? 'stage the unquantised BF16 checkpoint as the dense working directory (it is already dense; no dequantiser is involved)'
          : `dequantise the ${checkoutId} checkout to a dense BF16 directory (llm-compressor\'s converter; the FP8 source carries weight_block_size/weight_scale_inv, so the dequantiser must be FP8BlockDequantizer and not the generic CompressedTensorsDequantizer)`,
        `run the llm-compressor one-shot AWQ+GPTQ W4A16 recipe on that directory, with a calibration dataset and the offload settings written out explicitly`,
        'stop the deployment first, or use a scratch instance: this job wants the card to itself',
        `push the resulting W4A16 directory and point this deployment at it, then probe again`,
      ],
    },
    {
      path: 'model-free',
      label: 'Faster · lower quality',
      quality: 'one-pass RTN (model_free_ptq) — data-free, so llm-compressor refuses activation calibration on this path: this is RTN, NOT AWQ',
      produces: 'compressed-tensors (pack-quantized, RTN weights)',
      verified_for_this_architecture: false,
      estimated_minutes: REQUANTIZE_MODEL_FREE_MINUTES,
      estimated_minutes_range: [...REQUANTIZE_MODEL_FREE_MINUTES_RANGE],
      requires_vram_bytes: modelFree.requires_vram_bytes,
      requires_vram_bytes_exact: modelFree.requires_vram_bytes_exact,
      requires_disk_bytes: modelFree.requires_disk_bytes,
      requires_disk_bytes_exact: modelFree.requires_disk_bytes_exact,
      requires_host_ram_bytes: modelFree.requires_host_ram_bytes,
      requires_extra_vram_bytes: 0,
      steps: [
        `pip install "${text(hardware.tool) || REQUANTIZE_TOOL}"`,
        'call model_free_ptq on the source repo with scheme W4A16 and FP8BlockDequantizer, which chains dequantise→requantise without writing a dense checkpoint',
        'treat this as attempt-and-report-failure: it is proven on a different model (MiniMax-M2.7) and is not confirmed to survive this architecture\'s vision tower and linear_attn layers',
        'accept that the weights are RTN: no activation calibration happens on this path at all',
      ],
    },
  ];

  const maxLen = toNumber(probe?.arch?.max_position_embeddings);
  const kvDtype = text(probe?.kv_cache_dtype) || text(asObject(hardware.kv_cache_dtype))
    || text(asObject(probe?.sizing).kv_cache_dtype);

  return {
    requested: targetId,
    unavailable_reason: probed.length > 0
      ? `the repository ships ${probed.map((q) => text(q.id)).filter(Boolean).join(', ') || 'nothing quantised'}; no ${targetId} sibling and no ${targetId} tensors were found`
      : 'no probe has resolved the repository, so the shipped quants are unknown',
    source: {
      id: 'bf16',
      label: 'BF16 (base model)',
      bytes_per_param: 2.0,
      repo: baseModel,
      gated: false,
      recommended: true,
      reason: 'quantising from the BF16 base is both better on quality (no lossy FP8 round-trip) and easier operationally (ungated, Apache-2.0, no credential); the FP8 repo is the fallback for an operator who cannot pull the full BF16 checkpoint',
      token_field_hidden: true,
    },
    checkout,
    fallback_source: {
      id: checkoutId,
      label: checkout.label,
      repo: checkout.repo,
      gated: checkout.gated,
      reason: 'the FP8 checkout is half the download, at the cost of a gate, an HF token and a lossy FP8 round-trip before quantisation',
    },
    target: {
      id: targetId,
      label: text(target.label) || targetId,
      bytes_per_param: targetBpp,
      produced_quant_method: 'compressed-tensors',
      produces: 'compressed-tensors (pack-quantized, W4A16)',
      requested_not_produced: true,
      produced_quant_method_note: `${targetId} is the requested label, not a description of the artefact: llm-compressor writes quant_method "compressed-tensors" with format "pack-quantized" and no top-level bits/group_size, so it is not an AutoAWQ-format repository`,
    },
    path: 'dense',
    paths,
    feasible_on_gpu: feasible,
    failed_on: failedOn,
    requires_vram_bytes: dense.requires_vram_bytes,
    requires_vram_bytes_exact: dense.requires_vram_bytes_exact,
    requires_extra_vram_bytes: 0,
    requires_extra_vram_note: 'Compression does not support tensor parallelism; additional GPUs only shorten the job (measured 2.0x on 2 GPUs, 3.8x on 4). It is a single-card job that wants exclusive use of that card.',
    requires_host_ram_bytes: dense.requires_host_ram_bytes,
    requires_disk_bytes: dense.requires_disk_bytes,
    estimated_minutes: REQUANTIZE_DENSE_MINUTES,
    estimated_minutes_range: [...REQUANTIZE_DENSE_MINUTES_RANGE],
    estimated_minutes_reason: 'anchored on a measured GPTQ run on a Qwen3-30B-A3B (3.9 h on one GPU), reasoned up for 27B AWQ+GPTQ plus the dense round-trip; it is an estimate, not a measurement of this job',
    gpu_hours_at_k: gpuHoursAtK(index, REQUANTIZE_DENSE_MINUTES),
    k_gpu: REQUANTIZE_K_GPU,
    tool: text(hardware.tool) || REQUANTIZE_TOOL,
    serve_command: serveCommandForRequantized({ probe, hardware, maxLen, kvDtype }),
    serve_command_note: 'Serve the artefact with no --quantization flag derived from the request: the checkpoint declares quant_method "compressed-tensors" and vLLM\'s _verify_quantization raises ValueError when the flag disagrees with it, which is a paid boot that serves nothing.',
    note: `This is a real offline job on a real GPU: roughly 7.5–8.6 h, ~100 GB of scratch disk, ~69 GB of host RAM, and a quality regression relative to a publisher-native AWQ. Manarouter shows the recipe and the command; it does not run it, and it does not claim to have run it.`,
    assumptions: [
      `k_gpu = ${REQUANTIZE_K_GPU} is a CHOSEN margin on llm-compressor's published mem(1 layer) figure (1.0 published for RTN, 2.0 for GPTQ) to leave room for calibration activations, traced intermediates and the vision-tower onload: it has no published equivalent, the same category as the sizing overhead term`,
      `the vision tower (${gib(REQUANTIZE_VISION_TOWER_BYTES)} GiB) is onloaded by the GPU all at once and is not quantised; the research range is 1–4 GiB and is itself [unverified], so the top of that range is used`,
      `fixed_gpu_bytes = ${gib(REQUANTIZE_FIXED_GPU_BYTES)} GiB for the CUDA context, kernels and allocator fragmentation is [reasoned], not published`,
      `requires_vram_bytes is the byte-exact sum rounded to the nearest whole GiB; requires_host_ram_bytes and requires_disk_bytes are rounded to the nearest whole GB`,
      checkoutId === 'none'
        ? 'requires_disk_bytes counts the unquantised checkpoint being pulled (2 bytes/param); it is already dense, so there is no separate expansion step'
        : `requires_disk_bytes counts the ${checkoutId} checkout being pulled (${checkout.bytes_per_param} byte(s)/param)${checkout.bytes_per_param >= 2 ? '' : ' plus the dense BF16 intermediate it is expanded into'}`,
      ...(targetBppAssumed ? [`the catalog carries no bytes_per_param for ${targetId}, so ${REQUANTIZE_TARGET_BPP_FALLBACK} bytes/param was assumed for a 4-bit target`] : []),
      ...(dense.requires_disk_bytes == null ? ['the parameter count is unknown, so the host RAM and disk terms could not be computed'] : []),
    ],
    warnings: [
      'Only the dense path has an official example for this architecture; the model-free path is proven on a different model and is an attempt-and-report-failure route.',
      'vLLM\'s own startup line (`GPU KV cache size: N tokens`) is authoritative once the requantised artefact serves; these numbers are planning estimates.',
      'Requantisation here produces a derivative checkpoint that the operator hosts, with its own licensing question; Manarouter does not redistribute it.',
    ],
  };
}

/** Round to a whole GiB / GB, the convention the research notes use for these terms. */
function roundToGiB(bytes) {
  return bytes == null ? null : Math.round(bytes / 1024 ** 3) * 1024 ** 3;
}

function roundToGB(bytes) {
  return bytes == null ? null : Math.round(bytes / 1e9) * 1e9;
}

/**
 * The memory terms of one requantisation path, from the published model plus the stated margin.
 * Paths differ in `gptq` (the Hessian is weight-sized) and in whether a dense intermediate is
 * written, which is the difference between ~100 GB and ~45 GB of scratch disk.
 */
function requantizeCost({ params, layers, gptq, checkoutBpp, targetBpp }) {
  const bDequant = 2;
  const layerParams = params != null && layers != null && layers > 0 ? Math.ceil(params / layers) : null;
  const gpuTerm = layerParams == null
    ? null
    : layerParams * bDequant * (1 + (gptq ? 1 : 0)) * REQUANTIZE_K_GPU;
  const vramExact = gpuTerm == null
    ? null
    : gpuTerm + REQUANTIZE_VISION_TOWER_BYTES + REQUANTIZE_FIXED_GPU_BYTES;
  const hostExact = params == null ? null : params * bDequant * REQUANTIZE_K_RAM;
  const diskExact = params == null
    ? null
    : params * (checkoutBpp ?? 2)
      + (gptq ? params * bDequant : 0)
      + params * targetBpp * REQUANTIZE_OUTPUT_SLACK;

  return {
    requires_vram_bytes: roundToGiB(vramExact),
    requires_vram_bytes_exact: vramExact == null ? null : Math.round(vramExact),
    requires_host_ram_bytes: roundToGB(hostExact),
    requires_host_ram_bytes_exact: hostExact == null ? null : Math.round(hostExact),
    requires_disk_bytes: roundToGB(diskExact),
    requires_disk_bytes_exact: diskExact == null ? null : Math.round(diskExact),
  };
}

/**
 * The §8.5 predicate, in order: VRAM against the card at a 0.9 safety factor, then scratch disk,
 * then host RAM. `failed_on` names the term that ran out, so a `false` says which resource to fix
 * rather than implying another GPU would rescue it.
 */
function requantizeFailure({ vram, requiresVram, probe, cost }) {
  if (requiresVram != null && vram != null && requiresVram > vram * 0.9) return 'vram';
  const disk = toNumber(probe?.provisioned_disk_bytes ?? probe?.disk_bytes);
  if (cost.requires_disk_bytes != null && disk != null && cost.requires_disk_bytes > disk) return 'disk';
  const hostRam = toNumber(probe?.host_ram_bytes);
  if (cost.requires_host_ram_bytes != null && hostRam != null && cost.requires_host_ram_bytes > hostRam) return 'host_ram';
  return null;
}

/** Wall-clock hours and their cost on every card the catalog prices, at the plan's card count. */
function gpuHoursAtK(index, minutes) {
  const hours = round2(minutes / 60);
  const rows = [];
  for (const entry of Object.values(index.gpuById)) {
    const gpuObj = asObject(entry);
    const id = text(gpuObj.id);
    if (!id) continue;
    const perSecond = toNumber(gpuObj.price_usd_per_second);
    rows.push({
      gpu: id,
      hours,
      cost_usd: perSecond == null ? null : round1(hours * perSecond * 3600),
      price_usd_per_second: perSecond,
    });
  }
  return rows;
}

/** `vllm serve <dir>` with the flags the plan can justify — and never a `--quantization`. */
function serveCommandForRequantized({ probe, hardware, maxLen, kvDtype }) {
  const supplied = text(hardware.serve_command);
  if (supplied) return supplied;
  const args = ['vllm serve <output-dir>'];
  if (maxLen != null) args.push(`--max-model-len ${Math.trunc(maxLen)}`);
  if (kvDtype) args.push(`--kv-cache-dtype ${kvDtype}`);
  void probe;
  return args.join(' ');
}

/**
 * Fold a probe result into the draft.
 *
 * Only *empty* fields are filled: the operator's typed value always wins, because this runs
 * on every probe and a probe must never silently undo an edit.
 *
 * @param {object} draft
 * @param {object|null} probe
 * @param {object} catalog
 * @returns {object} a new Draft
 */
export function applyProbe(draft, probe, catalog) {
  const next = { ...normalizeDraft(draft) };
  const result = asObject(probe);
  const arch = asObject(result.arch);
  const hasProbe = probe != null && typeof probe === 'object' && Object.keys(result).length > 0;

  if (!hasProbe) {
    next.probe = null;
    next.hfTokenNeeded = false;
    return next;
  }

  next.probe = result;
  next.hfTokenNeeded = requiresHfToken(result);

  if (!next.repo && text(result.repo)) next.repo = text(result.repo);
  if (!next.revision && text(result.revision)) next.revision = text(result.revision);

  if (next.maxModelLen == null) {
    const maxLen = toNumber(arch.max_position_embeddings ?? arch.maxModelLen);
    if (maxLen != null) next.maxModelLen = Math.trunc(maxLen);
  }

  if (!next.weightQuant) {
    const probedQuants = asArray(result.quants).map(asObject).filter((q) => q.in_repo !== false && text(q.id));
    if (probedQuants.length === 1) next.weightQuant = text(probedQuants[0].id);
  }

  if (!next.kvCacheDtype) next.kvCacheDtype = 'auto';
  if (next.sizing == null && result.sizing != null) next.sizing = result.sizing;
  // Touch the catalog only to keep the signature honest: nothing here may depend on it, and a
  // missing catalog must not stop a probe from filling arch fields.
  void catalog;
  return next;
}

// ---------------------------------------------------------------------------
// sizing
// ---------------------------------------------------------------------------

function weightQuantBytes({ draft, probe, index }) {
  const total = toNumber(probe?.params?.total);
  const quantId = text(draft.weightQuant) || (asArray(probe?.quants).length === 1 ? text(probe.quants[0].id) : '');
  const probed = asArray(probe?.quants).map(asObject).find((q) => text(q.id) === quantId) ?? null;
  const entry = quantId && hasOwn(index.quantById, quantId) ? index.quantById[quantId] : null;
  const bpp = toNumber(probed?.bytes_per_param) ?? toNumber(entry?.bytes_per_param);

  let bytesPerParam = bpp;
  let source = bpp == null
    ? null
    : `bytes_per_param ${bpp} from ${probed?.bytes_per_param != null ? 'the probe' : `catalog quant_methods["${quantId}"]`}`;
  const assumption = [];

  if (bytesPerParam == null && (quantId === '' || quantId === 'none')) {
    const dtype = text(probe?.arch?.torch_dtype).toLowerCase();
    const implied = hasOwn(MODEL_DTYPE_BYTES, dtype) ? MODEL_DTYPE_BYTES[dtype] : null;
    if (implied != null) {
      bytesPerParam = implied;
      source = `unquantised weights follow the model dtype (${dtype} = ${implied} bytes/param)`;
      assumption.push(`assumed bf16/fp16 weights from config.json torch_dtype "${dtype}"; the catalog has no bytes_per_param for "${quantId || 'none'}"`);
    }
  }

  if (total == null) {
    return { bytes: null, bytesPerParam, source, assumption, reason: 'the probe carries no parameter count (probe.params.total)' };
  }
  if (bytesPerParam == null) {
    return {
      bytes: null,
      bytesPerParam,
      source,
      assumption,
      reason: `the catalog records no bytes_per_param for the weight quant "${quantId || '(none)'}"`,
    };
  }
  return { bytes: Math.round(total * bytesPerParam), bytesPerParam, source, assumption, reason: null };
}

function kvDtypeWidth({ draft, probe, index }) {
  const id = text(draft.kvCacheDtype) || 'auto';
  const entry = hasOwn(index.kvDtypeById, id) ? index.kvDtypeById[id] : null;
  const declared = toNumber(entry?.bytes_per_element ?? entry?.bytesPerElement);
  const assumption = [];
  if (declared != null) {
    return { bytes: declared, id, source: `catalog kv_cache_dtypes["${id}"].bytes_per_element`, assumption };
  }
  if (id === 'auto') {
    const dtype = text(probe?.arch?.torch_dtype).toLowerCase();
    const implied = hasOwn(MODEL_DTYPE_BYTES, dtype) ? MODEL_DTYPE_BYTES[dtype] : null;
    if (implied != null) {
      assumption.push(`assumed --kv-cache-dtype auto stores the model dtype (${dtype} = ${implied} bytes/element); the catalog has no kv_cache_dtypes["auto"] entry`);
      return { bytes: implied, id, source: `model dtype "${dtype}" (vLLM auto)`, assumption };
    }
  }
  const elem = toNumber(probe?.arch?.kv_bytes_per_token_elem);
  if (elem != null) return { bytes: elem, id, source: 'probe.arch.kv_bytes_per_token_elem', assumption };
  const implied = hasOwn(KV_DTYPE_IMPLIED_BYTES, id) ? KV_DTYPE_IMPLIED_BYTES[id] : null;
  if (implied != null) {
    assumption.push(`derived ${implied} byte(s)/element from the dtype name "${id}"; the catalog does not state it`);
    return { bytes: implied, id, source: `dtype name "${id}"`, assumption };
  }
  return { bytes: null, id, source: null, assumption };
}

function measuredReferenceFor({ index, draft, gpuId }) {
  const candidates = index.measuredReferences.map(asObject).filter((entry) => {
    const repo = text(entry.repo);
    const gpu = text(entry.gpu ?? entry.card ?? entry.gpu_id);
    if (repo && repo !== text(draft.repo)) return false;
    if (gpu && gpu !== text(gpuId)) return false;
    if (!repo && !gpu) return false;
    return true;
  });
  if (candidates.length === 0) return null;

  const wantDtype = text(draft.kvCacheDtype);
  const wantLen = toNumber(draft.maxModelLen);
  const scored = candidates.map((entry) => {
    let score = 0;
    if (text(entry.kv_cache_dtype) && text(entry.kv_cache_dtype) === wantDtype) score += 2;
    if (toNumber(entry.max_model_len) != null && toNumber(entry.max_model_len) === wantLen) score += 1;
    return { entry, score };
  });
  scored.sort((a, b) => b.score - a.score);
  return scored[0].entry;
}

/**
 * Compute the analytic KV-pool estimate for a draft, and say how much to trust it.
 *
 * `kv_bytes_per_token = 2 * full_attention_layers * num_key_value_heads * head_dim *
 * kv_dtype_bytes` (vLLM's own accounting, `vllm-sizing-and-quant.md` A.1) and
 * `kv_pool_tokens = floor((vram*util) − weights − overhead) / kv_bytes_per_token`.
 *
 * Two facts from §7.1 are load-bearing and neither is optional:
 *
 * 1. The target model is a **hybrid linear-attention** model. Only the `full_attention` entries of
 *    `layer_types` hold a per-token KV cache; the `linear_attention` layers hold a fixed-size
 *    recurrent state. Counting all 64 layers of `Qwen/Qwen3.8-27B` gives 131 072 bytes/token where
 *    the truth is 32 768 — a 4x overestimate — so `layer_types` decides the layer count whenever
 *    the config carries it.
 * 2. `overhead_bytes` is a **calibrated default (8 GiB), not a physical constant**. The estimate is
 *    therefore accompanied by its assumptions, by a calibration table of the recorded
 *    measurements, and by the warning that agreement with those same measurements is not
 *    independent confirmation.
 *
 * Refusing to guess stays a valid outcome: an unreadable architecture with no matching measurement
 * comes back `confidence: "unavailable"` with the reason instead of a number. An unreadable
 * architecture *with* a matching measurement comes back `confidence: "calibrated"`, inverted from
 * that measurement and labelled as circular.
 *
 * @param {object} draft
 * @param {object|null} probe
 * @param {object} catalog
 * @returns {{draft: object, sizing: object, warnings: string[]}}
 */
export function applySizing(draft, probe, catalog) {
  const index = catalogIndex(catalog);
  const base = normalizeDraft(draft);
  const probeResult = probe != null && typeof probe === 'object' ? probe : (base.probe ?? null);
  const result = asObject(probeResult);
  const arch = asObject(result.arch);
  const gpuId = text(base.gpu);
  const gpu = gpuEntry(index, gpuId);
  const gpuCount = Math.max(1, Math.trunc(toNumber(base.gpuCount) ?? 1));
  const utilGiven = toNumber(base.gpuMemoryUtilization);
  const utilization = utilGiven ?? DEFAULT_GPU_MEMORY_UTILIZATION;
  const assumptions = [];
  const warnings = [];

  const vramPerCard = toNumber(gpu?.vram_bytes ?? gpu?.vramBytes)
    ?? (toNumber(gpu?.vram_gib) != null ? toNumber(gpu.vram_gib) * 1024 ** 3 : null);
  const vramBytes = vramPerCard == null ? null : Math.round(vramPerCard * gpuCount);
  // Usable capacity, when the catalog records what the operator measured rather than the datasheet
  // (95 GiB of 96 GiB on the RTX PRO 6000). Used for the calibration table only, never silently in
  // place of the catalog's own figure.
  const usablePerCard = toNumber(gpu?.usable_vram_bytes ?? gpu?.usableVramBytes)
    ?? (toNumber(gpu?.usable_vram_gib ?? gpu?.usableVramGib) != null
      ? toNumber(gpu.usable_vram_gib ?? gpu.usableVramGib) * 1024 ** 3
      : null);
  const usableVramBytes = usablePerCard == null ? null : Math.round(usablePerCard * gpuCount);

  const weights = weightQuantBytes({ draft: base, probe: result, index });
  const kvWidth = kvDtypeWidth({ draft: base, probe: result, index });

  // --- architecture: which layers actually cache per token -------------------------------
  const layerTypes = asArray(arch.layer_types ?? arch.layerTypes).map((entry) => text(entry)).filter(Boolean);
  const declaredLayers = toInteger(arch.num_hidden_layers ?? arch.numLayers);
  const fullAttentionLayers = layerTypes.filter((entry) => /full[-_ ]?attention/i.test(entry)).length;
  let layers = declaredLayers;
  let layerCountSource = declaredLayers == null ? null : 'num_hidden_layers';
  let hybrid = false;
  let layerTypesIgnored = false;

  if (layerTypes.length > 0) {
    if (fullAttentionLayers > 0) {
      layers = fullAttentionLayers;
      layerCountSource = `layer_types: ${fullAttentionLayers} full_attention of ${layerTypes.length}`;
      hybrid = fullAttentionLayers !== layerTypes.length;
    } else {
      layerTypesIgnored = true;
      layerCountSource = 'num_hidden_layers (layer_types carried no full_attention entry)';
    }
  }

  const attentionHeads = toInteger(arch.num_attention_heads ?? arch.numAttentionHeads);
  const kvHeads = toInteger(firstDefined(arch.num_key_value_heads, arch.numKeyValueHeads, arch.num_kv_heads))
    ?? attentionHeads;
  let headDim = toInteger(firstDefined(arch.head_dim, arch.headDim, arch.head_size, arch.headSize));
  const hidden = toInteger(arch.hidden_size ?? arch.hiddenSize);
  if (headDim == null && hidden != null && attentionHeads != null && attentionHeads > 0 && hidden % attentionHeads === 0) {
    headDim = hidden / attentionHeads;
    assumptions.push(`head_dim was absent, so it is derived as hidden_size / num_attention_heads (${hidden} / ${attentionHeads} = ${headDim}); an explicit head_dim is preferred because that fallback is wrong for models that declare one`);
  }
  if (arch.num_key_value_heads == null && arch.numKeyValueHeads == null && arch.num_kv_heads == null && attentionHeads != null) {
    assumptions.push(`num_key_value_heads absent, so it defaults to num_attention_heads (${attentionHeads}): plain MHA, no grouped-query reduction`);
  }

  const archSource = text(arch.source ?? arch.arch_source ?? result.arch_source) || 'huggingface config.json';
  const missing = [];
  if (gpuId === '') missing.push('a GPU');
  else if (gpu == null) missing.push(`a catalog entry for GPU "${gpuId}"`);
  else if (vramBytes == null) missing.push(`vram_bytes for GPU "${gpuId}"`);
  if (layers == null) missing.push('num_hidden_layers (or layer_types) from the probed config.json');
  if (kvHeads == null) missing.push('num_key_value_heads');
  if (headDim == null) missing.push('head_dim');
  if (kvWidth.bytes == null) missing.push(`a KV element width for --kv-cache-dtype "${kvWidth.id}"`);
  if (weights.bytes == null) missing.push(weights.reason ?? 'a weight-quant byte width');

  const measured = measuredReferenceFor({ index, draft: base, gpuId });
  const measuredTokens = measured == null ? null : toNumber(measured.kv_pool_tokens ?? measured.kvPoolTokens);
  const hasMeasurement = measuredTokens != null && measuredTokens > 0;

  /** The recorded measurements, turned into a table of predicted-vs-measured points (§7.3). */
  const calibrationTable = () => asArray(index.measuredReferences).map(asObject).map((entry) => {
    const entryRepo = text(entry.repo);
    const card = text(entry.gpu ?? entry.card ?? entry.gpu_id);
    const tokens = toNumber(entry.kv_pool_tokens ?? entry.kvPoolTokens);
    const cardEntry = gpuEntry(index, card);
    const cardUsable = toNumber(cardEntry?.usable_vram_bytes ?? cardEntry?.usableVramBytes)
      ?? (toNumber(cardEntry?.usable_vram_gib ?? cardEntry?.usableVramGib) != null
        ? toNumber(cardEntry.usable_vram_gib ?? cardEntry.usableVramGib) * 1024 ** 3
        : null);
    const cardB = 2 * (layers ?? 0) * (kvHeads ?? 0) * (headDim ?? 0) * (kvWidth.bytes ?? 0);
    const predicted = cardUsable == null || cardB <= 0 || tokens == null || weights.bytes == null
      ? null
      : Math.max(0, Math.floor(Math.floor(cardUsable * utilization - weights.bytes - OVERHEAD_BYTES) / cardB));
    return {
      card: card || null,
      repo: entryRepo || null,
      measured_kv_pool_tokens: tokens,
      predicted_kv_pool_tokens: predicted,
      delta_pct: predicted == null || tokens == null || tokens <= 0 ? null : round2(((predicted - tokens) / tokens) * 100),
      vram_basis: cardUsable == null ? 'datasheet' : 'usable',
      usable_vram_gib: cardUsable == null ? null : gib(cardUsable),
      source: text(entry.source) || null,
      measured_at: text(entry.measured_at ?? entry.checked_at) || null,
    };
  });

  const measuredReference = measured == null ? null : {
    kv_pool_tokens: measuredTokens,
    card: text(measured.gpu ?? measured.card ?? measured.gpu_id) || gpuId || null,
    note: text(measured.note) || null,
    source: text(measured.source) || null,
    measured_at: text(measured.measured_at ?? measured.checked_at) || null,
    kv_cache_dtype: text(measured.kv_cache_dtype) || null,
    max_model_len: toNumber(measured.max_model_len),
    cold_start_seconds: measured.cold_start_seconds ?? null,
    tokens_per_second: toNumber(measured.tokens_per_second),
  };

  /**
   * The measurement inverted to a per-token KV width. Used only when the architecture is
   * unreadable: it is real data, it is labelled `calibrated`, and it is circular by construction.
   */
  const calibratedWidth = () => {
    if (!hasMeasurement || vramBytes == null || weights.bytes == null) return null;
    const usable = usableVramBytes ?? vramBytes;
    const budget = Math.floor(usable * utilization) - weights.bytes - OVERHEAD_BYTES;
    const perToken = Math.floor(budget / measuredTokens);
    return perToken > 0 ? perToken : null;
  };

  const baseFields = () => ({
    gpu: gpuId || null,
    gpu_count: gpuCount,
    vram_bytes: vramBytes,
    usable_vram_bytes: usableVramBytes,
    gpu_memory_utilization: utilization,
    block_size: BLOCK_SIZE,
    arch_source: archSource,
    layer_count_source: layerCountSource,
    num_full_attention_layers: fullAttentionLayers > 0 ? fullAttentionLayers : null,
    kv_bytes_per_token: null,
    kv_bytes_per_token_basis: null,
    weights_bytes: weights.bytes,
    weights_bytes_source: weights.source,
    overhead_bytes: OVERHEAD_BYTES,
    overhead_implied_bytes: null,
    architecture_consistent: null,
    kv_budget_bytes: null,
    kv_pool_tokens: null,
    kv_pool_tokens_source: null,
    max_model_len: toInteger(base.maxModelLen),
    simultaneous_sessions_at: [],
    max_num_seqs: toInteger(base.maxNumSeqs),
    max_num_seqs_effective: null,
    max_num_seqs_effective_basis: null,
    measured_reference: measuredReference,
    calibration: calibrationTable(),
    delta_vs_measurement_pct: null,
    delta_at_usable_vram_pct: null,
    missing: [],
    reason: null,
    assumptions,
    warnings,
    formula: 'kv_bytes_per_token = 2 * full_attention_layers * num_key_value_heads * head_dim * kv_dtype_bytes',
    source: 'docs/research/vllm-sizing-and-quant.md',
  });

  const finish = (sizing) => {
    // Forward the two record-keeping facts under the names `SizingResult` uses, so a stored
    // deployment's sizing describes itself. `vram_source` is the catalog's provenance for the VRAM
    // figure — a datasheet number must never be read back as if it had been measured — and
    // `kv_bytes_per_element` is the cache element width. The panel's own names stay alongside;
    // the conductor ignores keys it does not know.
    sizing.vram_source = sizing.vram_source ?? probeResult?.sizing?.vram_source ?? null;
    sizing.kv_bytes_per_element = toNumber(sizing.kv_bytes_per_element) ?? toNumber(sizing.kv_dtype_bytes);
    return { draft: { ...base, probe: probeResult, sizing }, sizing, warnings: sizing.warnings };
  };

  // A measured, unreadable architecture is calibrated rather than refused (§7.3).
  if (missing.length > 0) {
    const perToken = calibratedWidth();
    if (perToken != null) {
      const sizing = {
        ...baseFields(),
        method: 'calibrated',
        confidence: 'calibrated',
        missing,
        kv_bytes_per_token: perToken,
        kv_bytes_per_token_basis: `inverted from the recorded measurement on ${measuredReference.card}: (${gib(usableVramBytes ?? vramBytes)} GiB × ${utilization} − ${weights.bytes} bytes of weights − ${OVERHEAD_BYTES} bytes of overhead) / ${measuredTokens} measured tokens`,
        kv_pool_tokens: measuredTokens,
        kv_pool_tokens_source: 'the recorded measurement',
        delta_vs_measurement_pct: 0,
        kv_budget_bytes: Math.floor((usableVramBytes ?? vramBytes) * utilization) - weights.bytes - OVERHEAD_BYTES,
      };
      finalizeSizing(sizing, sizing.kv_pool_tokens);
      const seqs = effectiveSequences(sizing, sizing.kv_pool_tokens);
      sizing.max_num_seqs_effective = seqs.value;
      sizing.max_num_seqs_effective_basis = seqs.basis;
      assumptions.push(
        `the architecture could not be read (${missing.join('; ')}), so kv_bytes_per_token is calibrated by inverting the ${measuredReference.card} measurement of this model`,
        `gpu_memory_utilization ${utilization}${utilGiven == null ? ' (vLLM default)' : ' (operator override)'}`,
        'delta_vs_measurement_pct is 0 by construction here: the width was derived from that same measurement, so it carries no independent information',
      );
      warnings.push(
        'vLLM\'s own startup line (`GPU KV cache size: N tokens`) is authoritative; the calibrated figure below is a per-card conversion of one measurement, not a general estimate.',
        `The calibration is circular: kv_bytes_per_token was computed from the ${measuredReference.card} measurement, so the 0 % agreement in delta_vs_measurement_pct proves nothing.`,
      );
      if (layerTypesIgnored) {
        warnings.push('The config.json lists layer_types but none of them is full_attention; a hybrid model with no per-token KV layer cannot be sized from this config at all.');
      }
      return finish(sizing);
    }

    const reason = `The KV pool is not estimated because the probe and catalog do not supply: ${missing.join('; ')}${hasMeasurement ? '' : ', and no measurement of this model on this card is recorded either'}.`;
    warnings.push(reason);
    const sizing = { ...baseFields(), method: 'unavailable', confidence: 'unavailable', reason, missing };
    return finish(sizing);
  }

  const kvBytesPerToken = 2 * layers * kvHeads * headDim * kvWidth.bytes;
  const totalBudget = vramBytes * utilization;
  const kvBudget = Math.floor(totalBudget) - weights.bytes - OVERHEAD_BYTES;
  const kvPoolTokens = kvBudget <= 0 ? 0 : Math.floor(kvBudget / kvBytesPerToken);

  assumptions.push(
    `gpu_memory_utilization ${utilization}${utilGiven == null ? ' (vLLM default)' : ' (operator override)'}`,
    `overhead_bytes ${OVERHEAD_BYTES} (8 GiB) for activations, CUDA-graph capture, block padding and the fixed linear-attention state, excluding weights; vLLM documents no separate figure, so this default is calibrated rather than physical`,
    `--block-size ${BLOCK_SIZE}; the pool is not rounded to whole blocks here because vLLM's reported figure already absorbs block padding`,
    'max_model_len is the per-session context used for the session count',
    'the candidate architecture is rejected if it implies a negative overhead',
  );
  if (hybrid) {
    assumptions.push(`only full_attention layers are counted: ${fullAttentionLayers} of ${layerTypes.length} layer_types entries hold a per-token KV cache, and the other ${layerTypes.length - fullAttentionLayers} hold a fixed-size recurrent state. Counting all ${declaredLayers ?? layerTypes.length} layers would overestimate the KV cache ${round2((declaredLayers ?? layerTypes.length) / Math.max(1, fullAttentionLayers))}x`);
  } else {
    assumptions.push('every counted layer is assumed to cache per token; a sliding-window layer would cache less and this is therefore an overestimate');
  }
  if (kvWidth.assumption.length > 0) assumptions.push(...kvWidth.assumption);
  if (weights.assumption.length > 0) assumptions.push(...weights.assumption);
  if (gpuCount > 1) {
    assumptions.push(`gpu_count ${gpuCount}: the weights and the KV pool are assumed to be sharded evenly by tensor parallelism, so the pool scales with the card count and every sharded head count must divide by ${gpuCount}`);
  }

  if (kvBudget <= 0) {
    warnings.push(`The weights (${weights.bytes} bytes) plus the ${gib(OVERHEAD_BYTES)} GiB overhead exceed ${Math.floor(totalBudget)} bytes of budget on ${gpuId}${gpuCount > 1 ? ` ×${gpuCount}` : ''}: this card cannot host the checkpoint, so the pool below is 0 and no session fits.`);
  }
  if (layerTypesIgnored) {
    warnings.push(`The config.json lists layer_types but none of them is full_attention, so all ${declaredLayers ?? 0} layers were counted as caching per token — a hybrid model with no per-token KV layer cannot be sized from this config at all, and this number is not to be trusted.`);
  }
  if (layerTypes.length === 0) {
    warnings.push(`config.json carries no layer_types, so all ${declaredLayers ?? 0} layers are counted as caching per token. For a hybrid linear-attention model that overestimates the pool (4x on the model in this workstream), and the public base model's config.json is the documented fallback when a quantised repo hides the topology.`);
  }

  // --- calibration: what the recorded measurements imply, and whether they fit -----------
  const impliedOverhead = !hasMeasurement || vramBytes == null
    ? null
    : Math.floor((usableVramBytes ?? vramBytes) * utilization) - weights.bytes - measuredTokens * kvBytesPerToken;
  const measuredCard = measuredReference?.card ?? gpuId;
  let architectureConsistent = null;
  if (impliedOverhead != null) {
    architectureConsistent = impliedOverhead >= 0;
    if (impliedOverhead >= 0) {
      assumptions.push(`the recorded ${measuredCard} measurement (${measuredTokens} tokens) is explained by this architecture only with an overhead of ${gib(impliedOverhead)} GiB, against the ${gib(OVERHEAD_BYTES)} GiB default used here`);
    } else {
      warnings.push(`The recorded ${measuredCard} measurement cannot be explained by this architecture: it implies an overhead of ${impliedOverhead} bytes (${gib(impliedOverhead)} GiB), which is negative. The candidate architecture is rejected — check num_key_value_heads, head_dim and layer_types before trusting the pool below.`);
    }
  }

  const sizing = {
    ...baseFields(),
    method: 'analytic',
    confidence: 'analytic',
    num_hidden_layers: layers,
    num_declared_layers: declaredLayers,
    num_key_value_heads: kvHeads,
    head_dim: headDim,
    kv_cache_dtype: kvWidth.id,
    kv_dtype_bytes: kvWidth.bytes,
    kv_bytes_per_token: kvBytesPerToken,
    kv_bytes_per_token_basis: hybrid
      ? `full_attention_layers=${layers} of ${layerTypes.length}${declaredLayers != null && declaredLayers !== layerTypes.length ? ` (num_hidden_layers=${declaredLayers})` : ''} (hybrid linear attention; the other ${layerTypes.length - layers} hold a fixed-size recurrent state and no per-token KV) x num_kv_heads=${kvHeads} x head_dim=${headDim} x dtype_bytes=${kvWidth.bytes}; x2 for K and V`
      : `num_layers=${layers} x num_kv_heads=${kvHeads} x head_dim=${headDim} x dtype_bytes=${kvWidth.bytes}; x2 for K and V`,
    overhead_implied_bytes: impliedOverhead,
    architecture_consistent: architectureConsistent,
    kv_budget_bytes: kvBudget,
    kv_pool_tokens: kvPoolTokens,
    kv_pool_tokens_source: 'analytic estimate',
  };

  finalizeSizing(sizing, kvPoolTokens);
  const seqs = effectiveSequences(sizing, kvPoolTokens);
  sizing.max_num_seqs_effective = seqs.value;
  sizing.max_num_seqs_effective_basis = seqs.basis;

  const delta = hasMeasurement ? round2(((kvPoolTokens - measuredTokens) / measuredTokens) * 100) : null;
  sizing.delta_vs_measurement_pct = delta;
  const usableBudget = usableVramBytes == null ? null : Math.floor(usableVramBytes * utilization) - weights.bytes - OVERHEAD_BYTES;
  const usablePool = usableBudget == null || usableBudget <= 0 ? null : Math.floor(usableBudget / kvBytesPerToken);
  // Guarded by `hasMeasurement`, not just by `usablePool`: without a recorded figure this is a
  // division by null, which yields Infinity and would render as "Infinity %" beside the estimate.
  const deltaUsable = !hasMeasurement || usablePool == null
    ? null
    : round2(((usablePool - measuredTokens) / measuredTokens) * 100);
  sizing.delta_at_usable_vram_pct = deltaUsable;

  if (!hasMeasurement) {
    warnings.push('No measurement of this model on this card is recorded, so there is no delta to check the estimate against. vLLM\'s own startup line reports the authoritative KV size.');
  } else {
    warnings.push(
      `A ${delta} % agreement with the ${measuredCard} measurement is NOT independent confirmation: overhead_bytes is itself calibrated against that measurement. ${deltaUsable == null ? 'Treat it as a sanity bound, not as a validation.' : `Recomputing with the ${gib(usableVramBytes)} GiB the operator actually measured gives ${deltaUsable} %, so treat this as an accuracy bound of roughly ±2 %, not as validation.`}`,
      `vLLM's own startup line (\`GPU KV cache size: N tokens\`) is authoritative: this step estimates from the architecture, and only that line reports what the container actually allocated.`,
    );
  }
  const measuredCards = [...new Set(calibrationTable().map((row) => row.card).filter(Boolean))];
  if (measuredCards.length > 0) {
    warnings.push(`Only ${measuredCards.join(' and ')} ${measuredCards.length === 1 ? 'has' : 'have'} been measured for this model; every other card is extrapolated, and its VRAM is a datasheet figure rather than the usable capacity the operator measured.`);
  }

  sizing.calibration = calibrationTable();
  sizing.assumptions = assumptions;
  sizing.warnings = warnings;
  return finish(sizing);
}

/**
 * The contexts the sizing step reports concurrency for, ladder plus the operator's own maximum.
 *
 * The rows are `SessionsAtContext` (`crates/mr-core/src/gpu/sizing.rs`), not a display-only
 * shape: this object travels to `POST /api/gpu/apply`, which re-parses it into the server's own
 * `SizingResult`. A row missing `context_len`, `blocks_per_session` or `bytes_per_session` is
 * rejected there with HTTP 400, which is exactly what a display shape named `{context, sessions}`
 * did. `sessions` is `None` server-side when there is no pool to divide.
 */
function sessionLadder(sizing, kvPoolTokens) {
  const maxLen = sizing?.max_model_len ?? null;
  const bytesPerToken = toNumber(sizing?.kv_bytes_per_token);
  const blockSize = toNumber(sizing?.block_size) ?? BLOCK_SIZE;
  const contexts = [...new Set([...SIZING_CONTEXT_LADDER, ...(maxLen != null && maxLen > 0 ? [maxLen] : [])])]
    .filter((context) => context > 0 && (maxLen == null || context <= maxLen))
    .sort((a, b) => a - b);
  return contexts.map((contextLen) => ({
    context_len: contextLen,
    blocks_per_session: Math.ceil(contextLen / blockSize),
    bytes_per_session: bytesPerToken == null ? null : bytesPerToken * contextLen,
    sessions: kvPoolTokens == null ? null : Math.floor(kvPoolTokens / contextLen),
  }));
}

/**
 * Fill in every derived figure `SizingResult` defines, so the object forwarded to
 * `POST /api/gpu/apply` is the same shape the probe returned rather than a display subset.
 *
 * This is not cosmetic: the conductor reports a deployment's fit from these fields
 * (`gpu_api.rs::fit_of` reads `kv_pool_tokens_aligned` and `simultaneous_sessions_at_max_len`), so
 * omitting them made a deployment that had sized perfectly report `known: true` with a null pool
 * in the rentals view. `kv_blocks` and `kv_bytes_per_session_at_max_len` are filled for the same
 * reason: the wire object should lose nothing when it round-trips.
 */
function finalizeSizing(sizing, kvPoolTokens) {
  const bytesPerToken = toNumber(sizing?.kv_bytes_per_token);
  const maxLen = toNumber(sizing?.max_model_len);
  const blockSize = toNumber(sizing?.block_size) ?? BLOCK_SIZE;
  const pool = toNumber(kvPoolTokens);

  sizing.simultaneous_sessions_at = sessionLadder(sizing, pool);

  const blocks = pool == null ? null : Math.floor(pool / blockSize);
  const aligned = blocks == null ? null : blocks * blockSize;
  sizing.kv_blocks = blocks;
  sizing.kv_pool_tokens_aligned = aligned;
  sizing.kv_bytes_per_session_at_max_len = bytesPerToken == null || maxLen == null
    ? null
    : bytesPerToken * maxLen;
  sizing.simultaneous_sessions_at_max_len = aligned == null || maxLen == null || maxLen === 0
    ? null
    : Math.floor(aligned / maxLen);
  return sizing;
}

/**
 * `max_num_seqs_effective` is the pool's concurrency at the coarsest *reference* context at or
 * below `max_model_len` (32 768), capped by the operator's `max_num_seqs` — 54 in the canonical
 * example. It is deliberately not `pool / max_model_len`, which would be 6 there and is reported
 * as its own point in `simultaneous_sessions_at`.
 */
function effectiveSequences(sizing, kvPoolTokens) {
  const maxLen = sizing.max_model_len;
  const contexts = SIZING_CONTEXT_LADDER.filter((context) => maxLen == null || context <= maxLen);
  const context = contexts.length > 0 ? contexts[contexts.length - 1] : maxLen;
  if (context == null || !(context > 0) || kvPoolTokens == null) {
    return { value: sizing.max_num_seqs ?? null, basis: 'no context length is set, so the operator\'s max_num_seqs stands unchanged' };
  }
  const atContext = Math.floor(kvPoolTokens / context);
  const value = sizing.max_num_seqs == null ? atContext : Math.min(sizing.max_num_seqs, atContext);
  return {
    value,
    basis: `min(max_num_seqs=${sizing.max_num_seqs ?? 'unset'}, kv_pool_tokens / ${context}) to stay inside the pool at the coarse reference context; a session at the full ${maxLen ?? 'unset'} tokens would fit ${Math.floor(kvPoolTokens / (maxLen || 1))} time(s)`,
  };
}

// ---------------------------------------------------------------------------
// steps
// ---------------------------------------------------------------------------

/** A select whose options the view binds from the catalog (or from a resolved function). */
function selectField({ name, label, hint, optionsSource, required = true, visible = true, enabled = true, value = null }) {
  return {
    name, label, kind: 'select', hint, required, visible, enabled,
    options: null, optionsSource, value, placeholder: null, min: null, max: null, step: null,
    unit: null, readOnly: false, notes: [],
  };
}

/** A single-choice control: the view renders one radio card per option, never a select. */
function choiceField({ name, label, hint, optionsSource, required = true, visible = true, enabled = true, value = null }) {
  return {
    name, label, kind: 'choice', hint, required, visible, enabled,
    options: null, optionsSource, value, placeholder: null, min: null, max: null, step: null,
    unit: null, readOnly: false, notes: [],
  };
}

function textField({ name, label, hint = null, kind = 'text', required = false, visible = true, enabled = true, value = '', placeholder = null }) {
  return {
    name, label, kind, hint, required, visible, enabled,
    options: null, optionsSource: null, value, placeholder, min: null, max: null, step: null,
    unit: null, readOnly: false, notes: [],
  };
}

function numberField({ name, label, hint = null, value = null, min = null, max = null, step = null, required = false, visible = true, enabled = true, unit = null }) {
  return {
    name, label, kind: 'number', hint, required, visible, enabled,
    options: null, optionsSource: null, value, placeholder: null, min, max, step, unit,
    readOnly: false, notes: [],
  };
}

/** A derived, read-only line. `value` is what the model already knows; the view adds catalog bits. */
function readout({ name, label, hint = null, value = null, optionsSource = null, notes = [], visible = true }) {
  return {
    name, label, kind: 'readout', hint, required: false, visible, enabled: false,
    options: null, optionsSource, value, placeholder: null, min: null, max: null, step: null,
    unit: null, readOnly: true, notes,
  };
}

function fieldValue(draft, name) {
  const value = draft[name];
  return value === undefined ? null : value;
}

/**
 * The wizard's steps, for the current draft.
 *
 * `stepList` receives no catalog, so option-bearing fields carry `optionsSource` instead of
 * values: `catalog.platforms`, `catalog.gpus`, `quantOptions`, `kvDtypesForGpu`, `plans`,
 * `modes`, `planMarker`. That keeps the catalog the single source of GPU ids, prices and
 * flags (§5: "the panel never hardcodes a GPU, a price or a flag").
 *
 * @param {object} draft
 * @param {object|null} [catalog] Optional catalog, so the card-count field can honour a
 *   catalog-declared per-container limit instead of the built-in Modal one.
 * @returns {Array<{id: string, title: string, hint: string, fields: object[]}>}
 */
export function stepList(draft, catalog = null) {
  const d = normalizeDraft(draft);
  const probe = asObject(d.probe);
  const hasRepo = d.repo.trim().length > 0;
  const gated = requiresHfToken(probe);
  // Visibility follows the same rule `validateStep` enforces: the draft's own `hfTokenNeeded`, or a
  // probe that says the repository is gated. Reading only the probe put a "choose a token" problem
  // on a field the form was not drawing.
  const tokenNeeded = d.hfTokenNeeded || gated;
  const probedQuants = asArray(probe.quants).map(asObject);
  const sizing = asObject(d.sizing);
  const measured = asObject(sizing.measured_reference);
  const planLabel = d.plan || 'unknown';

  return [
    {
      id: 'platform',
      title: 'Platform',
      hint: 'Which rental platform this deployment runs on. The catalog decides what the rest of the flow may offer.',
      fields: [
        selectField({ name: 'platform', label: 'Platform', optionsSource: 'catalog.platforms', hint: 'Only the platforms the catalogue lists. Nothing beyond it is assumed.', value: fieldValue(d, 'platform') }),
        readout({ name: 'platformDeployment', label: 'How deployment works here', optionsSource: 'catalog.deployment', hint: 'Modal publishes no deployment REST API: the setup flow emits a bundle and a command, it does not deploy.' }),
      ],
    },
    {
      id: 'account',
      title: 'Account',
      hint: 'The account this rental bills through. One account can host several rentals, and one model on one card can be recorded under several accounts, so this step either creates an account or names one that already exists.',
      fields: [
        selectField({
          name: 'accountMode', label: 'Account', optionsSource: 'accountModes', required: true,
          value: d.accountMode,
          hint: 'Create a new account, or reuse one already recorded. Reusing does not touch its credential: the vault entry is written once, when the account is created.',
        }),
        textField({
          name: 'accountId', label: 'New account id', required: true,
          visible: d.accountMode !== 'existing',
          value: fieldValue(d, 'accountId'), placeholder: 'mana-modal',
          hint: 'Local identifier for the account, and the name of its vault entry. Several rentals may name the same account.',
        }),
        selectField({
          name: 'accountId', label: 'Account', optionsSource: 'accounts', required: true,
          visible: d.accountMode === 'existing',
          value: fieldValue(d, 'accountId'),
          hint: 'A recorded account. Its credential stays where it is; this rental only names it.',
        }),
        textField({ name: 'workspace', label: 'Modal workspace', required: true, visible: d.accountMode !== 'existing', value: fieldValue(d, 'workspace'), placeholder: 'mana', hint: 'The workspace this account\'s apps deploy into, for example `mana`. Not a secret: it is the half of the endpoint URL Manarouter can know without deploying.' }),
        selectField({ name: 'plan', label: 'Plan', optionsSource: 'plans', required: true, visible: d.accountMode !== 'existing', value: planLabel, hint: 'Unknown is a real answer and is never rendered as free or as paid.' }),
        textField({ name: 'tokenId', label: 'Modal proxy token id', kind: 'password', required: true, visible: d.accountMode !== 'existing', value: fieldValue(d, 'tokenId'), placeholder: 'wk-…', hint: 'Half of the Modal proxy token pair: the `wk-…` id the relay sends as `Modal-Key`. Stored in the vault; never rendered back.' }),
        textField({ name: 'tokenSecret', label: 'Modal proxy token secret', kind: 'password', required: true, visible: d.accountMode !== 'existing', value: fieldValue(d, 'tokenSecret'), placeholder: 'ws-…', hint: 'The other half: the `ws-…` secret sent as `Modal-Secret`. Both halves are stored as one vault entry, so an account cannot exist half-configured.' }),
        readout({
          name: 'planMarker', label: 'Plan marker', optionsSource: 'planMarker',
          hint: 'Shown only for a card the catalog marks `free_plan: "unavailable"`.',
          notes: ['Resolved by the view from the catalog with `planMarker(catalog, platform, { plan, gpu })`.'],
        }),
        readout({
          name: 'accountCredentialNote', label: 'Credential handling', value: 'vault',
          notes: [
            'A HuggingFace token is never collected here: tokens are independent of accounts and are named on the model step, only when the repository is gated.',
            'No secret is ever echoed back by `POST /api/gpu/apply`: the response carries a vault id and a fingerprint.',
          ],
        }),
      ],
    },
    {
      id: 'model',
      title: 'Model',
      hint: 'Paste a HuggingFace repo. Nothing is fetched until it is non-empty, and the name is inspected before the repo is queried.',
      fields: [
        textField({ name: 'repo', label: 'HuggingFace repo', required: true, value: fieldValue(d, 'repo'), placeholder: 'orcarouter/Qwen3.8-27B-Uncensored-FP8', hint: '`owner/name`, as on huggingface.co.' }),
        textField({ name: 'revision', label: 'Revision', value: fieldValue(d, 'revision'), placeholder: 'main', hint: 'Branch, tag or commit. Empty means the repo default.' }),
        selectField({
          name: 'hfTokenId', label: 'HuggingFace token', optionsSource: 'hfTokens',
          required: false, visible: tokenNeeded,
          value: fieldValue(d, 'hfTokenId'),
          hint: 'Only needed because this repository is gated: unauthenticated file fetches answer 401. A token is shared by any number of accounts, so this list is independent of the account step.',
        }),
        textField({
          name: 'hfTokenNew', label: 'Add a HuggingFace token', kind: 'password', visible: tokenNeeded,
          value: fieldValue(d, 'hfTokenNew'), placeholder: 'hf_…',
          hint: 'Paste one to add it now; it is written to the vault under the id below and never rendered back. Leave empty to use a recorded token.',
        }),
        textField({
          name: 'hfTokenNewLabel', label: 'New token id', visible: tokenNeeded && Boolean(d.hfTokenNew),
          value: fieldValue(d, 'hfTokenNewLabel'), placeholder: 'hub-main',
          hint: 'The id this token is recorded under, so a later rental can name it instead of pasting it again.',
        }),
        readout({
          name: 'probeGated', label: 'Gated', value: d.probe == null ? null : (gated ? 'token required' : 'public'),
          visible: d.probe != null,
          hint: 'From `gated` in the probe response.',
        }),
        readout({
          name: 'probeParams', label: 'Parameters', value: toNumber(probe.params?.total),
          visible: probe.params != null,
          notes: [
            probe.params?.from_name != null
              ? `Name says ${probe.params.from_name.token ?? '?'} (${probe.params.from_name.value ?? '?'} params).`
              : 'The name carries no parameter count.',
            probe.params?.source != null ? `Counted from ${probe.params.source}.` : '',
            probe.params?.agrees_with_name === false ? 'The name and the checkpoint disagree; the checkpoint wins.' : '',
          ].filter(Boolean),
        }),
        readout({
          name: 'probeArch', label: 'Architecture', value: probe.arch?.model_type ?? null,
          visible: probe.arch != null,
          notes: [
            `layers ${probe.arch?.num_hidden_layers ?? '?'}, KV heads ${probe.arch?.num_key_value_heads ?? probe.arch?.num_attention_heads ?? '?'}, head_dim ${probe.arch?.head_dim ?? '?'}`,
            `torch_dtype ${probe.arch?.torch_dtype ?? '?'}`,
          ],
        }),
        readout({
          name: 'probeQuants', label: 'Quants in the repository', value: probedQuants.length,
          visible: d.probe != null,
          notes: probedQuants.length > 0
            ? probedQuants.map((q) => `${q.id}: ${q.evidence ?? 'shipped'}`)
            : ['No quantisation config was found; the checkpoint serves unquantised.'],
        }),
        readout({
          name: 'probeWarnings', label: 'Probe warnings', value: asArray(probe.warnings).length,
          visible: asArray(probe.warnings).length > 0,
          notes: asArray(probe.warnings).map(String),
        }),
      ],
    },
    {
      id: 'card',
      title: 'Card',
      hint: 'The card this rental runs on. Its plan marker, its price and which quantisations are legal all come from it.',
      fields: [
        selectField({ name: 'gpu', label: 'GPU', optionsSource: 'catalog.gpus', required: true, value: fieldValue(d, 'gpu'), hint: 'From the catalogue. The price, the VRAM ceiling and the legal quantisations on this step all follow from this choice.' }),
        numberField({ name: 'gpuCount', label: 'Cards', value: fieldValue(d, 'gpuCount'), min: 1, max: maxGpuCountFor(d.gpu, catalog), step: 1, required: true, hint: `Cards per container: up to ${GPU_COUNT_LIMIT_DEFAULT}, except A10 at ${MODAL_GPU_COUNT_LIMITS.A10}. More than one card needs tensor parallelism, and the KV pool is then assumed to be sharded evenly.` }),
        choiceField({
          name: 'mode', label: 'One card, one job', optionsSource: 'modes', required: true, value: fieldValue(d, 'mode'),
          hint: 'A deployment is one model on one card doing one thing. Streaming answers requests as they arrive; batch drains a queue in bulk. Exactly one applies here.',
        }),
        numberField({ name: 'gpuMemoryUtilization', label: 'GPU memory utilization', value: fieldValue(d, 'gpuMemoryUtilization'), min: 0.05, max: 1, step: 0.01, required: true, hint: 'vLLM’s own budget knob (default 0.92). It must cover weights, activations and CUDA graphs, not only the KV cache.' }),
        selectField({ name: 'weightQuant', label: 'Weight quantisation', optionsSource: 'quantOptions', enabled: hasRepo, required: hasRepo, value: fieldValue(d, 'weightQuant'), hint: hasRepo ? 'Only quants this repository ships, filtered by this card.' : 'Paste a model repo first: the quant list is resolved from the probe, not guessed.' }),
        selectField({ name: 'kvCacheDtype', label: 'KV-cache dtype', optionsSource: 'kvDtypesForGpu', enabled: true, required: true, value: fieldValue(d, 'kvCacheDtype'), hint: 'Independent of the weight quantisation: each card has its own legal dtypes.' }),
        selectField({ name: 'requantizeTo', label: 'Requantise to (optional)', optionsSource: 'requantizeOptions', required: false, enabled: hasRepo, value: fieldValue(d, 'requantizeTo'), hint: 'A quant the repository does not ship. Choosing one plans a future GPU job for this setup; it is not done by the wizard and the bundle still serves what the repository ships.' }),
        readout({
          name: 'gpuPrice', label: 'Price', optionsSource: 'catalog.gpus.price_usd_per_second',
          hint: 'Per second of container life. Modal bills per second, including the cold boot.',
          visible: d.gpu !== '',
        }),
        readout({
          name: 'cardPlanMarker', label: 'Plan marker for this card', optionsSource: 'planMarker',
          visible: d.gpu !== '',
          notes: ['`(Paid)` only when the plan is known and the catalog says `free_plan: "unavailable"`.'],
        }),
      ],
    },
    {
      id: 'sizing',
      title: 'Sizing',
      hint: 'The probe’s architecture gives an analytic KV estimate. Overrides are yours; vLLM’s startup line is authoritative.',
      fields: [
        numberField({ name: 'maxModelLen', label: 'Max model length', value: fieldValue(d, 'maxModelLen'), min: 1, step: 1, hint: 'vLLM `--max-model-len`. Defaults to the config’s `max_position_embeddings`; lowering it raises concurrency.' }),
        numberField({ name: 'maxNumSeqs', label: 'Sequences per step', value: fieldValue(d, 'maxNumSeqs'), min: 1, step: 1, hint: 'vLLM `--max-num-seqs`: how many sequences one scheduling step admits. This is not the number of simultaneous sessions; it is the ceiling vLLM keeps in flight, and the sessions you ask for below are what the card is judged against.' }),
        numberField({
          name: 'sessions', label: 'Concurrent sessions to hold', value: fieldValue(d, 'sessions'), min: 1, step: 1,
          visible: d.mode !== 'batch',
          hint: 'One streaming card serves several sessions at once, not one request at a time. This is how many must fit in the KV pool together; the fit check below refuses the deployment if the card cannot hold them.',
        }),
        numberField({
          name: 'batchSize', label: 'Batch size', value: fieldValue(d, 'batchSize'), min: 1, step: 1,
          visible: d.mode === 'batch',
          hint: 'How many requests one batch collects before it is worked. This is the batch deployment’s concurrency, and the fit check judges the card against it.',
        }),
        choiceField({
          name: 'batchFill', label: 'Ship a batch when', value: fieldValue(d, 'batchFill'),
          optionsSource: 'batchFills',
          visible: d.mode === 'batch',
          hint: 'A full batch is the cheapest work per request; a timeout keeps a quiet model from holding a request forever.',
        }),
        numberField({
          name: 'batchTimeoutSecs', label: 'Batch timeout (seconds)', value: fieldValue(d, 'batchTimeoutSecs'),
          min: 1, max: 3600, step: 1,
          visible: d.mode === 'batch' && d.batchFill === 'timeout',
          hint: 'After this long, whatever has arrived ships as a half-full batch rather than waiting for more.',
        }),
        readout({
          name: 'batchWaitNote', label: 'Waiting for a full batch', value: null,
          visible: d.mode === 'batch' && d.batchFill !== 'timeout',
          notes: ['With no timeout, a request waits until the batch is full, however long that takes. That is a choice, not an oversight: nothing here adds a hidden cap.'],
        }),
        numberField({ name: 'shutdownCooldownSecs', label: 'Shutdown cooldown (seconds)', value: fieldValue(d, 'shutdownCooldownSecs'), min: SCALEDOWN_WINDOW_LIMITS.min, max: SCALEDOWN_WINDOW_LIMITS.max, step: 1, hint: 'Modal `scaledown_window`: default 60, range 2–1200.' }),
        readout({
          name: 'sizingKvBasis', label: 'KV bytes per token', value: sizing.kv_bytes_per_token ?? null,
          visible: sizing.kv_bytes_per_token != null,
          notes: [
            sizing.kv_bytes_per_token_basis != null ? String(sizing.kv_bytes_per_token_basis) : '',
            sizing.arch_source != null ? `Architecture read from ${sizing.arch_source}.` : '',
            sizing.layer_count_source != null ? `Layer count from ${sizing.layer_count_source}.` : '',
            sizing.confidence != null ? `confidence: ${sizing.confidence}` : '',
          ].filter(Boolean),
        }),
        readout({
          name: 'sizingKvPool', label: sizing.kv_pool_tokens_source === 'the recorded measurement' ? 'KV pool (calibrated)' : 'KV pool (estimate)',
          value: toNumber(sizing.kv_pool_tokens),
          visible: sizing.kv_pool_tokens != null,
          notes: [
            `weights ${sizing.weights_bytes ?? '?'} bytes, overhead ${sizing.overhead_bytes ?? '?'} bytes, budget ${sizing.kv_budget_bytes ?? '?'} bytes`,
            sizing.overhead_implied_bytes != null ? `the recorded measurement implies ${sizing.overhead_implied_bytes} bytes of overhead` : '',
            sizing.architecture_consistent === false ? 'The candidate architecture is rejected: it implies a negative overhead.' : '',
            ...asArray(sizing.simultaneous_sessions_at).map((point) => `${point.sessions} session(s) at ${point.context_len} tokens`),
            sizing.max_num_seqs_effective != null ? `max_num_seqs_effective ${sizing.max_num_seqs_effective}: ${sizing.max_num_seqs_effective_basis ?? ''}` : 'Set a max model length to see sessions.',
          ].filter(Boolean),
        }),
        readout({
          name: 'sizingMeasured', label: 'Measured reference', value: measured.kv_pool_tokens ?? null,
          visible: sizing.measured_reference != null,
          notes: [
            measured.card != null ? `Measured on ${measured.card}.` : '',
            measured.measured_at != null ? String(measured.measured_at) : '',
            measured.note != null ? String(measured.note) : '',
            sizing.delta_vs_measurement_pct != null ? `This estimate is ${sizing.delta_vs_measurement_pct > 0 ? '+' : ''}${sizing.delta_vs_measurement_pct}% against it.` : '',
            sizing.delta_at_usable_vram_pct != null ? `Against the usable capacity the operator measured: ${sizing.delta_at_usable_vram_pct}%.` : '',
            'Read this as an accuracy bound of roughly ±2 %, not as a validation of the estimate.',
          ].filter(Boolean),
        }),
        readout({
          name: 'sizingCalibration', label: 'Calibration points', value: asArray(sizing.calibration).length,
          visible: asArray(sizing.calibration).length > 0,
          notes: asArray(sizing.calibration).map((row) => `${row.card ?? '?'}: measured ${row.measured_kv_pool_tokens ?? '?'}, predicted ${row.predicted_kv_pool_tokens ?? '?'} (${row.delta_pct ?? '?'} %), VRAM basis ${row.vram_basis}`),
        }),
        readout({
          name: 'sizingAssumptions', label: 'Assumptions', value: asArray(sizing.assumptions).length,
          visible: asArray(sizing.assumptions).length > 0,
          notes: asArray(sizing.assumptions).map(String),
        }),
        readout({
          name: 'sizingUnavailable', label: 'Why there is no estimate', value: sizing.reason ?? null,
          visible: sizing.method === 'unavailable',
          notes: asArray(sizing.missing).map((item) => `missing: ${item}`),
        }),
        readout({
          name: 'sizingWarnings', label: 'Warnings', value: asArray(sizing.warnings).length,
          visible: asArray(sizing.warnings).length > 0,
          notes: asArray(sizing.warnings).map(String),
        }),
      ],
    },
    {
      id: 'review',
      title: 'Review',
      hint: 'The bundle is the deployment: Modal publishes no deployment REST API, so this file and its command are what you run.',
      fields: [
        readout({
          name: 'deploymentShape', label: 'What this deployment is', value: deploymentShapeNote(d),
          notes: ['The mode and the fill policy are recorded with the deployment. A batch that waits for a full batch holds a request until the batch fills; the timeout variant is the one that ships half-full.'],
        }),
        readout({
          name: 'endpointUrl', label: 'Endpoint URL', value: modalBaseUrl(d),
          notes: ['Derived from the account\'s Modal workspace and the generated app name. Nothing is deployed yet: `modal deploy` prints the real URL, and if it differs, that one wins.'],
        }),
        textField({
          name: 'baseUrl', label: 'Override endpoint URL (optional)', required: false, value: fieldValue(d, 'baseUrl'),
          placeholder: 'https://workspace--manarouter-rental-….modal.run',
          hint: 'Leave empty normally. Fill it only when `modal deploy` prints a URL other than the derived one.',
        }),
        readout({
          name: 'bundleFilename', label: 'Bundle file', value: 'modal_app.py',
          notes: ['Generated from this draft; no credentials are ever embedded in it.'],
        }),
        readout({
          name: 'bundleCommand', label: 'Deploy command', value: 'modal deploy modal_app.py',
          notes: ['Run it yourself, in a shell where the Modal CLI is signed in.'],
        }),
        readout({
          name: 'bundleNotes', label: 'Caveats', optionsSource: 'deploymentBundle.notes',
          notes: ['Every note from `deploymentBundle()` is shown beside the file, not hidden behind it.'],
        }),
        readout({
          name: 'bundleWarnings', label: 'Bundle warnings', optionsSource: 'deploymentBundle.warnings',
          notes: [
            'A warning here means the generated file cannot serve the quantisation this draft selects: the quant is not in the checkout, or its recorded label is not its real `quant_method`. The file is still shown, because the recipe that produces it is the recorded one — but read this before running `modal deploy`.',
          ],
        }),
        readout({
          name: 'sizingDelta', label: 'Estimate vs measurement', value: sizing.delta_vs_measurement_pct ?? null,
          visible: sizing.delta_vs_measurement_pct != null,
          notes: [
            'An accuracy bound of roughly ±2 %, not a validation: the overhead term was calibrated against the same measurement, so agreement is not independent confirmation.',
            sizing.delta_at_usable_vram_pct != null ? `Against the usable capacity the operator measured: ${sizing.delta_at_usable_vram_pct}%.` : '',
            'The estimate is written into the provider as a default you can override once the container reports its own KV size.',
          ].filter(Boolean),
        }),
      ],
    },
  ];
}

/**
 * One step by id.
 *
 * @param {object} draft
 * @param {string} stepId
 * @returns {object|null}
 */
export function stepById(draft, stepId) {
  const id = text(stepId);
  if (!id) return null;
  return stepList(draft).find((step) => step.id === id) ?? null;
}

/**
 * The next step id in requirement order, or `null` at the end.
 *
 * Navigation is order only: whether the current step is valid is `validateStep`'s answer,
 * and conflating the two is how a wizard ends up silently skipping a required field.
 *
 * @param {object} draft
 * @param {string} stepId
 * @returns {string|null}
 */
export function nextStep(draft, stepId) {
  void draft;
  const index = STEP_ORDER.indexOf(text(stepId));
  if (index < 0) return null;
  return index + 1 < STEP_ORDER.length ? STEP_ORDER[index + 1] : null;
}

/**
 * The previous step id in requirement order, or `null` at the start.
 *
 * @param {object} draft
 * @param {string} stepId
 * @returns {string|null}
 */
export function prevStep(draft, stepId) {
  void draft;
  const index = STEP_ORDER.indexOf(text(stepId));
  if (index <= 0) return null;
  return STEP_ORDER[index - 1];
}

// ---------------------------------------------------------------------------
// validation
// ---------------------------------------------------------------------------

function problem(field, message) {
  return { field, message };
}

function slugLike(value) {
  return /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/.test(value);
}

/**
 * Validate one step of the draft.
 *
 * Field-level answers land in `problems` (mapped to inputs by leaf name, as the panel
 * conventions require); structural answers land in `errors` with an empty `details` array,
 * because this module has no server to quote. Values are never echoed, so a bad token cannot
 * end up in a toast.
 *
 * @param {object} draft
 * @param {string} stepId
 * @param {object|null} [catalog]
 * @param {Array<object>|null} [accounts] The recorded accounts, so an id another rental already
 *   pays from is refused here instead of by the conductor.
 * @returns {{ok: boolean, problems: Array<{field: string, message: string}>, errors: Array<{code: string, message: string, details: string[]}>}}
 */
export function validateStep(draft, stepId, catalog = null, accounts = null) {
  const d = normalizeDraft(draft);
  const id = text(stepId);
  const problems = [];
  const errors = [];

  if (!STEP_ORDER.includes(id)) {
    errors.push({
      code: 'unknown_step',
      message: `There is no setup step "${id || '(none)'}". Known steps: ${STEP_ORDER.join(', ')}.`,
      details: [],
    });
    return { ok: false, problems, errors };
  }

  if (id === 'platform') {
    if (!d.platform) {
      problems.push(problem('platform', 'Choose a platform. The catalog decides which ones exist.'));
    } else if (/\s/.test(d.platform)) {
      problems.push(problem('platform', `"${d.platform}" is not a catalog platform id.`));
    }
  }

  if (id === 'account') {
    const recorded = asArray(accounts).map(asObject);
    const reuse = d.accountMode === 'existing';
    if (reuse) {
      // Reusing an account names it and nothing else: its workspace, plan and credential are
      // already recorded, and re-sending them would be a second write of the same secret.
      if (!d.accountId) {
        problems.push(problem('accountId', 'Choose the recorded account this rental bills through.'));
      } else if (!recorded.some((account) => text(account.id) === d.accountId)) {
        problems.push(problem('accountId', `"${d.accountId}" is not a recorded account on this platform, so there is nothing to reuse.`));
      }
    } else {
      if (!d.accountId) {
        problems.push(problem('accountId', 'An account id is required: it names the vault entry and the deployment’s billing account.'));
      } else if (!slugLike(d.accountId)) {
        problems.push(problem('accountId', 'Use letters, digits, dot, dash or underscore, starting with a letter or digit.'));
      } else if (recorded.some((account) => text(account.id) === d.accountId)) {
        problems.push(problem('accountId', `Account "${d.accountId}" is already recorded. Reuse it instead of creating a second one with the same id, which would collide on its vault entry.`));
      }
      if (!d.workspace) {
        problems.push(problem('workspace', 'The Modal workspace is required: it is half of the endpoint URL and the app deploys into it.'));
      } else if (d.workspace.length > 63 || !/^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$/.test(d.workspace)) {
        problems.push(problem('workspace', 'A Modal workspace is up to 63 lowercase letters, digits or dashes, with no dash at either end (for example `mana`).'));
      }
      if (!PLAN_IDS.includes(d.plan)) {
        problems.push(problem('plan', `Plan must be one of ${PLAN_IDS.join(', ')}; leave it on unknown rather than guessing.`));
      }
      if (!d.tokenId) {
        problems.push(problem('tokenId', 'The Modal proxy token id is required (Modal-Key).'));
      } else if (/^ak-/.test(d.tokenId)) {
        problems.push(problem('tokenId', 'That is a Modal API token (ak-…), not a proxy token. The relay sends proxy tokens as Modal-Key (wk-…).'));
      }
      if (!d.tokenSecret) {
        problems.push(problem('tokenSecret', 'The Modal proxy token secret is required (Modal-Secret).'));
      } else if (/^as-/.test(d.tokenSecret)) {
        problems.push(problem('tokenSecret', 'That is a Modal API token secret (as-…), not a proxy token secret (ws-…).'));
      }
    }
  }

  if (id === 'model') {
    const repo = d.repo.trim();
    if (!repo) {
      problems.push(problem('repo', 'Paste a HuggingFace repo id (owner/name). Nothing is fetched until it is non-empty.'));
    } else if (!/^[\w.-]+\/[\w.-]+$/.test(repo)) {
      problems.push(problem('repo', 'A repo id looks like owner/name, e.g. orcarouter/Qwen3.8-27B-Uncensored-FP8.'));
    }
    const probe = asObject(d.probe);
    if (d.probe != null && probe.repo != null && text(probe.repo) !== repo) {
      problems.push(problem('repo', `The probe result is for "${text(probe.repo)}", not this repo. Probe again before continuing.`));
    }
    if (asArray(probe.errors).length > 0) {
      problems.push(problem('repo', `The last probe failed: ${asArray(probe.errors).map((e) => (typeof e === 'string' ? e : text(e?.message) || JSON.stringify(e))).join('; ')}`));
    }
    if (requiresHfToken(probe) && !d.hfTokenId && !d.hfTokenNew) {
      problems.push(problem('hfTokenId', 'This repository is gated, so a HuggingFace token is needed for the file fetches. Choose a recorded token or add one; it is stored in the vault, never on the panel.'));
    }
    if (d.hfTokenNew && !d.hfTokenNewLabel) {
      problems.push(problem('hfTokenNewLabel', 'Give the new token an id, so a later rental can name it instead of pasting it again.'));
    } else if (d.hfTokenNew && !slugLike(d.hfTokenNewLabel)) {
      problems.push(problem('hfTokenNewLabel', 'Use letters, digits, dot, dash or underscore for the token id.'));
    }
  }

  if (id === 'card') {
    if (!d.gpu) {
      problems.push(problem('gpu', 'Choose a GPU from the catalog.'));
    }
    const count = toNumber(d.gpuCount);
    if (count == null || !Number.isInteger(count) || count < 1) {
      problems.push(problem('gpuCount', 'Card count must be a whole number of at least 1.'));
    } else {
      // Modal's per-container limit is a property of the card, and A10 is the published exception
      // at 4. Asking for more is not a warning-level nicety: Modal rejects the deploy.
      const limit = maxGpuCountFor(d.gpu, catalog);
      if (count > limit) {
        problems.push(problem('gpuCount', `${d.gpu} runs at most ${limit} card${limit === 1 ? '' : 's'} per container, so ${count} is not a configuration Modal will accept.`));
      } else if (count > 1 && count > GPU_COUNT_LIMIT_DEFAULT) {
        problems.push(problem('gpuCount', `More than ${GPU_COUNT_LIMIT_DEFAULT} cards is not a configuration this flow can size honestly.`));
      }
    }
    const utilization = toNumber(d.gpuMemoryUtilization);
    if (utilization == null || !(utilization > 0) || utilization > 1) {
      problems.push(problem('gpuMemoryUtilization', 'GPU memory utilization must be greater than 0 and at most 1 (vLLM’s default is 0.92).'));
    }
    if (!d.repo.trim()) {
      problems.push(problem('weightQuant', 'Paste a model repo on the model step first: the quant list comes from the probe, not from a hardcoded list.'));
    } else if (!d.weightQuant) {
      problems.push(problem('weightQuant', 'Choose a weight quantisation (or "none" for unquantised weights).'));
    } else if (d.weightQuant !== 'none' && asObject(d.probe).quants != null) {
      const shipped = asArray(asObject(d.probe).quants).map((q) => text(asObject(q).id));
      if (shipped.length > 0 && !shipped.includes(d.weightQuant)) {
        problems.push(problem('weightQuant', `The probed repository ships ${shipped.join(', ')}; "${d.weightQuant}" is not one of them. Choose it under "Requantise to (optional)" as a future GPU job instead.`));
      }
    }
    if (d.mode !== 'streaming' && d.mode !== 'batch') {
      problems.push(problem('mode', 'Choose exactly one: streaming or batch.'));
    }
    if (!d.kvCacheDtype) {
      problems.push(problem('kvCacheDtype', 'Choose a KV-cache dtype; "auto" follows the model dtype.'));
    }
  }

  if (id === 'sizing') {
    const maxLen = d.maxModelLen;
    if (maxLen != null && maxLen !== '') {
      const value = toNumber(maxLen);
      if (value == null || !Number.isInteger(value) || value < 1) {
        problems.push(problem('maxModelLen', 'Max model length must be a whole number of tokens, or empty to use the config’s value.'));
      }
    }
    const seqs = d.maxNumSeqs;
    if (seqs != null && seqs !== '') {
      const value = toNumber(seqs);
      if (value == null || !Number.isInteger(value) || value < 1) {
        problems.push(problem('maxNumSeqs', 'Simultaneous sessions must be a whole number of at least 1, or empty.'));
      }
    }
    const sessions = d.sessions;
    if (sessions == null || sessions === '' || !Number.isInteger(toNumber(sessions)) || toNumber(sessions) < 1) {
      problems.push(problem('sessions', 'Concurrent sessions must be a whole number of at least 1.'));
    }
    if (d.mode === 'batch') {
      const size = toNumber(d.batchSize);
      if (size == null || !Number.isInteger(size) || size < 1) {
        problems.push(problem('batchSize', 'Batch size must be a whole number of at least 1.'));
      }
      if (d.batchFill !== 'full' && d.batchFill !== 'timeout') {
        problems.push(problem('batchFill', 'Choose one fill policy: wait until full, or ship after a timeout.'));
      }
      if (d.batchFill === 'timeout') {
        const secs = toNumber(d.batchTimeoutSecs);
        if (secs == null || !Number.isInteger(secs) || secs < 1 || secs > 3600) {
          problems.push(problem('batchTimeoutSecs', 'Batch timeout must be a whole number of seconds between 1 and 3600.'));
        }
      }
    }
    const cooldown = toNumber(d.shutdownCooldownSecs);
    if (cooldown == null || !Number.isInteger(cooldown)) {
      problems.push(problem('shutdownCooldownSecs', 'Shutdown cooldown must be a whole number of seconds.'));
    } else if (cooldown < SCALEDOWN_WINDOW_LIMITS.min || cooldown > SCALEDOWN_WINDOW_LIMITS.max) {
      problems.push(problem('shutdownCooldownSecs', `Modal accepts scaledown_window from ${SCALEDOWN_WINDOW_LIMITS.min} to ${SCALEDOWN_WINDOW_LIMITS.max} seconds (default ${SCALEDOWN_WINDOW_LIMITS.default}).`));
    }
  }

  if (id === 'review') {
    const url = d.baseUrl.trim();
    if (!url) {
      // Absent override: the conductor derives the URL from the account's workspace and is the
      // authority on whether that is possible; the panel must not invent one either.
      if (modalBaseUrl(d) == null) {
        problems.push(problem('baseUrl', 'No endpoint URL can be derived: record the Modal workspace on the account step, or paste the URL `modal deploy` prints.'));
      }
    } else {
      let parsed = null;
      try {
        parsed = new URL(url);
      } catch {
        problems.push(problem('baseUrl', 'That is not a URL. Modal web endpoints look like https://workspace--app-fn.modal.run.'));
      }
      if (parsed) {
        if (parsed.protocol !== 'https:') {
          problems.push(problem('baseUrl', 'Use https://: the relay sends proxy-token headers, and they must not travel in clear text.'));
        }
        if (d.platform === 'modal' && !/(^|\.)modal\.run$/.test(parsed.hostname)) {
          problems.push(problem('baseUrl', `"${parsed.hostname}" is not a *.modal.run host. \`modal deploy\` prints the app's real URL.`));
        }
      }
    }
    if (!d.gpu) problems.push(problem('gpu', 'No GPU is selected, so there is no deployment to review.'));
    if (!d.repo.trim()) problems.push(problem('repo', 'No model repo is selected, so there is no deployment to review.'));
  }

  return { ok: problems.length === 0 && errors.length === 0, problems, errors };
}

// ---------------------------------------------------------------------------
// deployment bundle
// ---------------------------------------------------------------------------

function clampScaledown(value) {
  const raw = toNumber(value);
  const requested = raw == null ? null : Math.trunc(raw);
  if (requested == null) return { seconds: SCALEDOWN_WINDOW_LIMITS.default, requested: null, clamped: false };
  const seconds = Math.min(SCALEDOWN_WINDOW_LIMITS.max, Math.max(SCALEDOWN_WINDOW_LIMITS.min, requested));
  return { seconds, requested, clamped: seconds !== requested };
}

function vllmVersion(catalog) {
  const index = catalogIndex(catalog);
  const body = index.catalog;
  return text(body.vllm_version ?? body.vllmVersion ?? asObject(body.vllm).version) || DEFAULT_VLLM_VERSION;
}

/**
 * The `modal_app.py` bundle: the deployment itself, because Modal publishes no deployment
 * REST API.
 *
 * The generated Python names a `modal.Secret` by **reference** (`from_name`) and never a
 * credential value; `notes` carries the `modal secret create` command the operator runs in
 * their own shell. Multi-card deployments get `gpu="<id>:<n>"` plus `--tensor-parallel-size`,
 * because a two-card container without tensor parallelism only uses one card.
 *
 * @param {object} draft
 * @param {object|null} sizing
 * @param {object} catalog
 * @returns {{filename: string, text: string, command: string, notes: string[]}}
 */
/**
 * One sentence naming what the recorded deployment is, in the same words the bundle uses: a mode,
 * a card (or several), and — for a batch — how a batch decides it is done collecting. The review
 * step shows this because the fill policy is part of the deployment, not a detail of the app.
 */
export function deploymentShapeNote(draft) {
  const d = normalizeDraft(draft);
  const card = d.gpu ? `${d.gpuCount > 1 ? `${d.gpuCount} × ` : ''}${d.gpu}` : 'a card not chosen yet';
  if (d.mode === 'batch') {
    const when = d.batchFill === 'timeout'
      ? `a batch ships after ${d.batchTimeoutSecs} s even if it is not full`
      : 'a batch waits until it is full, however long that takes';
    return `Batch of ${d.batchSize} on ${card}: ${when}.`;
  }
  return `Streaming on ${card}, holding ${d.sessions} simultaneous session${d.sessions === 1 ? '' : 's'}.`;
}

export function deploymentBundle(draft, sizing, catalog) {
  const d = normalizeDraft(draft);
  const index = catalogIndex(catalog);
  const notes = [];
  const warnings = [];
  const gpuId = d.gpu;
  const gpu = gpuEntry(index, gpuId);
  const cardLimit = maxGpuCountFor(gpuId, catalog);
  const requestedCount = Math.max(1, Math.trunc(toNumber(d.gpuCount) ?? 1));
  const gpuCount = Math.min(requestedCount, cardLimit);
  // `gpu="H100"` may be served on an H200 at no extra cost, which silently changes the VRAM the
  // KV budget was computed against. Pin the H100 and say why, rather than sizing one card and
  // requesting another (§2.1).
  const pinsH100 = gpuId === H100_AUTO_UPGRADE.id;
  const bundleGpuId = pinsH100 ? H100_AUTO_UPGRADE.pinned : gpuId;
  const cudaPinned = gpuId === 'B300';
  const version = vllmVersion(catalog);
  const clamp = clampScaledown(d.shutdownCooldownSecs);
  const gated = d.hfTokenNeeded || requiresHfToken(d.probe) || Boolean(d.hfTokenId || d.hfTokenNew);
  const utilization = toNumber(d.gpuMemoryUtilization) ?? DEFAULT_GPU_MEMORY_UTILIZATION;
  // A batch deployment is not a vLLM flag set: vLLM schedules whatever arrives. The mode's meaning
  // lives in the serving layer this file generates (the assembler below), so the batch shape has to
  // reach the text, not only the record.
  const mode = d.mode === 'batch' ? 'batch' : 'streaming';
  const batchSize = Math.max(1, Math.trunc(toNumber(d.batchSize) ?? 1));
  const batchFill = d.batchFill === 'timeout' ? 'timeout' : 'full';
  const batchTimeout = Math.max(1, Math.trunc(toNumber(d.batchTimeoutSecs) ?? 1));
  // At least one full batch has to be admissible, or "wait until it is full" can never complete:
  // Modal's default is one concurrent input per container, which would make the assembler deadlock
  // by design.
  const batchInputs = Math.max(1, batchSize);
  // vLLM owns its own port in batch mode so the assembler can own the public one.
  const servingPort = mode === 'batch' ? 8001 : 8000;
  // The same derivation the conductor records as the provider's base_url, so the app this file
  // deploys and the URL the panel shows cannot drift apart.
  const appName = modalAppName(d.accountId, d.repo, d.mode, d.gpu, d.gpuCount);
  const derivedUrl = modalBaseUrl(d);
  const secretName = 'manarouter-hf';
  const volumeName = 'manarouter-hf-cache';
  // What the probed repo actually ships. `null` ids are ignored: a probe that names no quant has
  // not answered the question, and "the repo ships nothing" is not a fact to assert from silence.
  const shippedQuants = asArray(asObject(d.probe).quants)
    .map((q) => (typeof q === 'string' ? q : text(asObject(q).id)))
    .filter(Boolean);
  const requantizeNeeded = shippedQuants.length > 0 && Boolean(d.weightQuant)
    && d.weightQuant !== 'none' && !shippedQuants.includes(d.weightQuant);
  // The frozen contract records the produced quant_method on `requantize.target` (§8), not on the
  // catalog's quant table — §6 documents only `min_compute_cap` and `bytes_per_param` there. A
  // catalog that omits the field must therefore not silently disable the disagreement warning: when
  // the draft asks for a quant the repo does not ship, the artefact is whatever the recorded recipe
  // writes, and §6.2 fixes that as `compressed-tensors`.
  const producedQuantMethod = text(asObject(index.quantById[d.weightQuant]).produced_quant_method)
    || (requantizeNeeded
      ? text(asObject(requantizePlan({ probe: d.probe, gpuId, catalog, targetId: d.weightQuant }).target).produced_quant_method)
      : '');

  if (!gpu && gpuId) {
    notes.push(`"${gpuId}" is not in the catalog, so its VRAM and price are unknown here. Modal will reject an id it does not publish.`);
  }
  if (hasOwn(RENAMED_MODAL_GPU_IDS, gpuId)) {
    notes.push(`"${gpuId}" is not a current Modal GPU id; the catalog spells this card "${RENAMED_MODAL_GPU_IDS[gpuId]}". The bundle keeps your value and Modal will reject it until it is changed.`);
  }
  if (clamp.clamped) {
    notes.push(`scaledown_window ${clamp.requested} s was outside Modal's ${SCALEDOWN_WINDOW_LIMITS.min}–${SCALEDOWN_WINDOW_LIMITS.max} s range, so the bundle uses ${clamp.seconds} s.`);
  }

  const args = [
    '--model', d.repo || '<repo>',
  ];
  if (d.revision) args.push('--revision', d.revision);
  // No `--quantization` flag is emitted, ever, and that is deliberate (§6.4 of
  // docs/research/requantize-recipes.md, §2.1 here). vLLM auto-detects the quantisation from the
  // checkpoint's own `quant_method`, and `_verify_quantization` *raises* rather than coercing when
  // a flag disagrees with the artefact — so a flag can only ever lose a paid container that boots
  // and then serves nothing. Two ways to disagree: the repo ships a different quant than the one
  // selected, and a requantised artefact whose recorded label ("awq") is not its `quant_method`
  // ("compressed-tensors"). The notes below say which case applies; the warning says so too.
  args.push('--kv-cache-dtype', d.kvCacheDtype || 'auto');
  if (d.maxModelLen != null) args.push('--max-model-len', String(Math.trunc(toNumber(d.maxModelLen) ?? 0)));
  if (d.maxNumSeqs != null) args.push('--max-num-seqs', String(Math.trunc(toNumber(d.maxNumSeqs) ?? 0)));
  args.push('--gpu-memory-utilization', String(utilization));
  if (gpuCount > 1) args.push('--tensor-parallel-size', String(gpuCount));
  args.push('--host', '0.0.0.0', '--port', String(servingPort));

  const argLines = [];
  for (let i = 0; i < args.length; i += 2) {
    argLines.push(`    ${pyString(args[i])}, ${pyString(args[i + 1])},`);
  }

  const decorator = [
    '@app.function(',
    `    image=vllm_image,`,
    `    gpu=${pyString(gpuCount > 1 ? `${bundleGpuId}:${gpuCount}` : bundleGpuId)},`,
    `    scaledown_window=${clamp.seconds},`,
    `    startup_timeout=900,`,
    `    volumes={"/root/.cache/huggingface": hf_cache},`,
    ...(gated ? [`    secrets=[modal.Secret.from_name(${pyString(secretName)})],`] : []),
    ')',
  ];

  // In batch mode the public port belongs to the assembler and vLLM keeps an internal one, so the
  // file says both: what the batch is, and who owns which port. The assembler is stdlib-only
  // (threads, queue, urllib, http.server) on purpose: it has to run in whatever image vLLM runs in,
  // and every extra import is one more way for a paid container to fail at boot.
  const batchConstants = mode === 'batch'
    ? [
      `BATCH_SIZE = ${batchSize}`,
      batchFill === 'timeout'
        ? `# A batch that has not filled after ${batchTimeout} seconds ships anyway; a half-full batch still goes out.`
        : '# This batch waits until it is full, however long that takes: that is the policy chosen for it.',
      `BATCH_TIMEOUT = ${batchFill === 'timeout' ? `${batchTimeout}.0` : 'None'}`,
      '',
      'VLLM_PORT = 8001',
      '',
      'import json',
      'import queue',
      'import threading',
      'import time',
      'import urllib.error',
      'import urllib.request',
      'from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer',
      '',
      'UPSTREAM = "http://127.0.0.1:%d" % VLLM_PORT',
      'PENDING = queue.Queue()',
      '',
      '',
      'def _post(body):',
      '    request = urllib.request.Request(',
      '        UPSTREAM + "/v1/chat/completions",',
      '        data=body,',
      '        headers={"Content-Type": "application/json"},',
      '        method="POST",',
      '    )',
      '    try:',
      '        with urllib.request.urlopen(request) as answer:',
      '            return answer.status, answer.headers.get("Content-Type"), answer.read()',
      '    except urllib.error.HTTPError as error:',
      '        return error.code, error.headers.get("Content-Type"), error.read()',
      '    except Exception as error:  # a dead upstream must still answer its waiters',
      '        return 502, "application/json", json.dumps({"error": str(error)}).encode()',
      '',
      '',
      'def _wait_until_serving():',
      '    # A cold container loads weights for minutes; dispatching before vLLM answers only turns',
      '    # that wait into a batch of failed requests.',
      '    while True:',
      '        try:',
      '            with urllib.request.urlopen(UPSTREAM + "/v1/models", timeout=5):',
      '                return',
      '        except Exception:',
      '            time.sleep(5.0)',
      '',
      '',
      'def _assemble():',
      '    _wait_until_serving()',
      '    while True:',
      '        batch = [PENDING.get()]',
      '        if BATCH_SIZE > 1:',
      '            if BATCH_TIMEOUT is None:',
      '                while len(batch) < BATCH_SIZE:',
      '                    batch.append(PENDING.get())',
      '            else:',
      '                deadline = time.monotonic() + BATCH_TIMEOUT',
      '                while len(batch) < BATCH_SIZE:',
      '                    left = deadline - time.monotonic()',
      '                    if left <= 0:',
      '                        break',
      '                    try:',
      '                        batch.append(PENDING.get(timeout=left))',
      '                    except queue.Empty:',
      '                        break',
      '        answers = [None] * len(batch)',
      '',
      '        def run(index, request_body):',
      '            answers[index] = _post(request_body)',
      '',
      '        threads = [',
      '            threading.Thread(target=run, args=(index, body))',
      '            for index, (body, _) in enumerate(batch)',
      '        ]',
      '        for thread in threads:',
      '            thread.start()',
      '        for thread in threads:',
      '            thread.join()',
      '        for (_, slot), answer in zip(batch, answers):',
      '            slot["answer"] = answer',
      '            slot["done"].set()',
      '',
      '',
      'class BatchHandler(BaseHTTPRequestHandler):',
      '    def log_message(self, *args):',
      '        pass',
      '',
      '    def _reply(self, status, content_type, payload):',
      '        self.send_response(status)',
      '        self.send_header("Content-Type", content_type or "application/json")',
      '        self.send_header("Content-Length", str(len(payload)))',
      '        self.end_headers()',
      '        self.wfile.write(payload)',
      '',
      '    def _proxy_get(self, path):',
      '        with urllib.request.urlopen(UPSTREAM + path) as answer:',
      '            self._reply(answer.status, answer.headers.get("Content-Type"), answer.read())',
      '',
      '    def do_GET(self):',
      '        if self.path == "/v1/models":',
      '            self._proxy_get(self.path)',
      '        else:',
      '            self._reply(404, "application/json", b\'{"error":"not found"}\')',
      '',
      '    def do_POST(self):',
      '        if self.path != "/v1/chat/completions":',
      '            self._reply(404, "application/json", b\'{"error":"not found"}\')',
      '            return',
      '        length = int(self.headers.get("Content-Length") or 0)',
      '        body = self.rfile.read(length)',
      '        slot = {"done": threading.Event(), "answer": None}',
      '        PENDING.put((body, slot))',
      '        slot["done"].wait()',
      '        status, content_type, payload = slot["answer"]',
      '        self._reply(status, content_type, payload)',
      '',
    ]
    : [];

  const serveSurface = mode === 'batch'
    ? [
      '@modal.concurrent(max_inputs=BATCH_SIZE)',
      '@modal.web_server(port=8000, requires_proxy_auth=True)',
      'def serve():',
      '    # vLLM owns VLLM_PORT and the assembler owns 8000; Modal proxies 8000 at the app URL.',
      '    subprocess.Popen(["vllm", "serve", *VLLM_ARGS])',
      '    threading.Thread(target=_assemble, daemon=True).start()',
      '    server = ThreadingHTTPServer(("0.0.0.0", 8000), BatchHandler)',
      '    server.daemon_threads = True',
      '    server.serve_forever()',
      '',
    ]
    : [
      '@modal.web_server(port=8000, requires_proxy_auth=True)',
      'def serve():',
      '    # vLLM owns port 8000; Modal proxies it at the app\'s *.modal.run URL.',
      '    process = subprocess.Popen(["vllm", "serve", *VLLM_ARGS])',
      '    process.wait()',
      '',
    ];

  const lines = [
    '"""modal_app.py — generated by Manarouter\'s GPU setup flow.',
    '',
    `Model:   ${d.repo || '<repo>'}${d.revision ? ` @ ${d.revision}` : ''}`,
    `Card:    ${bundleGpuId || '<gpu>'}${gpuCount > 1 ? ` × ${gpuCount}` : ''}${gpu?.price_usd_per_second != null ? ` (catalog $${gpu.price_usd_per_second}/s)` : ''}`,
    `Sizing:  ${asObject(sizing ?? d.sizing).kv_pool_tokens != null ? `Manarouter estimate ${asObject(sizing ?? d.sizing).kv_pool_tokens} KV tokens — an estimate, not vLLM's measurement` : 'not estimated'}`,
    '',
    ...(cudaPinned
      ? [`WARNING: B300 needs CUDA ${B300_CUDA_MIN_VERSION}+. This template pins no CUDA base image, so the vLLM wheel above must itself be a CUDA ${B300_CUDA_MIN_VERSION}+ build. Check that before deploying: a CUDA-12 image cannot drive this card.`, '']
      : []),
    ...(pinsH100
      ? [`Card pinned as "${H100_AUTO_UPGRADE.pinned}": ${H100_AUTO_UPGRADE.note}`, '']
      : []),
    'Run it with:  modal deploy modal_app.py',
    '"""',
    '',
    'import subprocess',
    '',
    'import modal',
    '',
    `app = modal.App(${pyString(appName)})`,
    '',
    'hf_cache = modal.Volume.from_name(' + pyString(volumeName) + ', create_if_missing=True)',
    '',
    'vllm_image = (',
    '    modal.Image.debian_slim(python_version="3.12")',
    `    .pip_install("vllm==${version}")`,
    '    .env({"HF_HOME": "/root/.cache/huggingface", "VLLM_USE_V1": "1"})',
    ')',
    '',
    ...batchConstants,
    'VLLM_ARGS = [',
    ...argLines,
    ']',
    '',
    '',
    ...decorator,
    ...serveSurface,
  ];

  notes.push(...[
    'Modal publishes no deployment REST API, so this file is the deployment: run `modal deploy modal_app.py` yourself (or `app.deploy()` from the SDK). Manarouter does not run it for you.',
    `The KV budget Manarouter derived (${asObject(sizing ?? d.sizing).kv_pool_tokens ?? 'not estimated'} tokens) is an analytic estimate. vLLM's own startup line "GPU KV cache size: N tokens" is authoritative — read it back and override the provider's kv_pool_tokens with it.`,
    `The image pins vllm==${version} (provenance: the operator's own deployment, docs/research/issue-4-spec.md). Change the pin deliberately, not silently.`,
    `\`modal deploy\` takes --strategy [rolling|recreate] (and -e/--env, --name, --tag, --stream-logs); rolling is the default and keeps the old container serving while the new one boots. There is no --detach: the command stays attached to the deploy, so run it in your own terminal.`,
  ]);

  if (requestedCount > gpuCount) {
    notes.push(`Modal allows at most ${cardLimit} of these cards in one container (you asked for ${requestedCount}), so the bundle requests ${bundleGpuId}:${cardLimit}. A10 is the published exception at ${MODAL_GPU_COUNT_LIMITS.A10}; every other card is capped at ${GPU_COUNT_LIMIT_DEFAULT} per container.`);
  }
  if (pinsH100) {
    notes.push(`Card pinned: ${H100_AUTO_UPGRADE.note}`);
  }
  if (cudaPinned) {
    notes.push(`B300 needs CUDA ${B300_CUDA_MIN_VERSION}+. This template pins no CUDA base image, so only a vLLM wheel built against CUDA ${B300_CUDA_MIN_VERSION}+ can drive the card; a CUDA-12 image fails at startup. Verify the wheel's CUDA build before deploying, and do not read the KV estimate as valid until the container actually boots.`);
  }

  if (gated) {
    notes.push(`The bundle references the Modal secret "${secretName}" by name and embeds no token. Create it in your own shell: modal secret create ${secretName} HF_TOKEN=... (the value never touches this panel).`);
  } else {
    notes.push('The repository probes as public, so no HuggingFace secret is attached. If the probe was wrong or the repo becomes gated, add a secret rather than pasting a token into the file.');
  }

  if (gpuCount > 1) {
    notes.push(`Multi-card: the bundle asks Modal for gpu="${gpuId}:${gpuCount}" and passes --tensor-parallel-size ${gpuCount}. A container that requests several cards without tensor parallelism uses one of them.`);
  }
  if (!d.weightQuant || d.weightQuant === 'none') {
    notes.push('No --quantization flag is emitted: the checkpoint serves at its own dtype. Asking for a quant this repo does not ship is a recorded recipe, not a runtime flag.');
  } else if (shippedQuants.length > 0 && !shippedQuants.includes(d.weightQuant)) {
    notes.push(`"${d.weightQuant}" is not what this repo ships (it ships ${shippedQuants.join(', ')}). No --quantization flag is emitted: vLLM would read the checkpoint's own quant_method anyway, and a flag that disagrees with the artefact raises ValueError at startup. Produce the quant with the recorded requantisation recipe first, then point --model at the output directory (or at a repo you push it to) instead of ${d.repo || 'this repo'}.`);
  } else if (shippedQuants.length > 0) {
    notes.push(`No --quantization flag is emitted: vLLM detects ${d.weightQuant} from the checkpoint's own quant_method. A --quantization flag that disagrees with the artefact makes vLLM raise ValueError and the container serves nothing, so auto-detection is the only safe rule.`);
  } else {
    notes.push(`No --quantization flag is emitted, even though ${d.weightQuant} is selected: the probe has not resolved what this repo ships. vLLM detects the quantisation from the checkpoint's own quant_method; a flag that disagrees raises ValueError at startup and the container serves nothing.`);
  }
  if (producedQuantMethod && d.weightQuant && producedQuantMethod !== d.weightQuant) {
    warnings.push(`The catalog records that "${d.weightQuant}" is produced as quant_method "${producedQuantMethod}", not "${d.weightQuant}". llm-compressor writes the label as the recipe's name, not as the artefact's dtype, so a --quantization ${d.weightQuant} flag against a ${producedQuantMethod} checkpoint raises ValueError at startup. This bundle emits no --quantization flag; point --model at the requantised directory and probe it again before trusting the KV estimate.`);
  }
  if (requantizeNeeded) {
    warnings.push(`The checkout at ${d.repo || 'this repo'} does not ship "${d.weightQuant}", so nothing in this bundle can serve the quantisation you selected. The bundle is only the *serving* half: produce the quant with the recorded recipe, then deploy again against the produced directory.`);
  }
  if (d.maxModelLen != null && asObject(d.probe).arch?.max_position_embeddings != null
    && toNumber(d.maxModelLen) < toNumber(asObject(d.probe).arch.max_position_embeddings)) {
    notes.push(`--max-model-len ${d.maxModelLen} is below the config's max_position_embeddings (${asObject(d.probe).arch.max_position_embeddings}); requests longer than this are rejected, which is the intended way to raise concurrency.`);
  }
  notes.push('Modal bills per second of container life, including the cold boot; the catalog carries the per-second rate, and an hour-based figure is not the same number.');
  if (d.requantizeTo) {
    notes.push(`A future requantisation to "${d.requantizeTo}" is planned in this setup, as a separate multi-hour GPU job. It is not part of this file, which serves what the repository ships.`);
  }
  if (d.baseUrl) {
    notes.push(`The endpoint URL is overridden to ${d.baseUrl}. That value wins only if it is what \`modal deploy\` printed; the derived URL for this app would be ${derivedUrl ?? 'unavailable without a workspace'}.`);
  } else if (derivedUrl) {
    notes.push(`Manarouter computes the endpoint URL as ${derivedUrl} from the workspace and the app name. \`modal deploy\` prints the authoritative URL; if it differs, override the provider's base_url with the printed one.`);
  }
  if (asObject(sizing ?? d.sizing).method === 'unavailable') {
    notes.push('No KV estimate was available when this bundle was generated, so the provider will need its kv_pool_tokens from vLLM\'s startup line.');
  }

  if (mode === 'batch') {
    notes.push(`Batch mode is carried by the assembler in this file: /v1/chat/completions holds requests until ${batchSize} have arrived${batchFill === 'timeout' ? `, or until ${batchTimeout} seconds have passed, whichever comes first` : ', however long that takes'}, then sends them to vLLM in one wave and returns the responses in the order they were queued.`);
    notes.push('The assembler is one FastAPI app in this same file, behind @modal.asgi_app(); vLLM listens on 8001 and the assembler owns 8000. @modal.concurrent(max_inputs=BATCH_SIZE) is required, not decorative: Modal admits one input per container by default, and a batch that can never be full would wait for a request that is not allowed to arrive.');
    notes.push('A request that asks for `stream: true` is answered with the batch\'s finished body in one piece rather than an incremental stream, because the batch is dispatched and awaited as a whole. Ask for a streaming deployment if token-by-token delivery matters.');
  }

  return { filename: 'modal_app.py', text: lines.join('\n'), command: 'modal deploy modal_app.py', notes, warnings };
}

// ---------------------------------------------------------------------------
// payloads for POST /api/gpu/apply
// ---------------------------------------------------------------------------

function optionalInt(value) {
  const number = toNumber(value);
  return number == null ? null : Math.trunc(number);
}

/**
 * The `create-account` body for `POST /api/gpu/apply` (§5), or `null` when this rental reuses an
 * account that is already recorded.
 *
 * This is the one payload that carries an account credential, and it exists because the vault
 * write is the point of the request. It must be posted and dropped, never rendered, logged or kept
 * in component state; the server answers with a vault id and a fingerprint, never the value. A
 * HuggingFace token is not part of it: tokens are their own entity, added by
 * {@link hfTokenApplyPayload} and named by the deployment.
 *
 * @param {object} draft
 * @returns {object|null}
 */
export function accountApplyPayload(draft) {
  const d = normalizeDraft(draft);
  if (d.accountMode === 'existing') return null;
  const payload = {
    action: 'create-account',
    account: {
      id: d.accountId,
      label: d.accountLabel || d.accountId,
      platform: d.platform,
      plan: d.plan,
      // Not a secret: the Modal workspace the apps deploy into, and the half of the endpoint URL
      // the conductor can know without deploying.
      workspace: d.workspace,
      token_id: d.tokenId,
      token_secret: d.tokenSecret,
    },
  };
  return payload;
}

/**
 * The `create-hf-token` body for `POST /api/gpu/apply`, or `null` when the draft adds no token.
 *
 * A token is written once and then named by id: the value must be posted and dropped exactly like
 * an account credential. The id is the draft's `hfTokenNewLabel`, which is what a later rental
 * selects from the recorded list.
 *
 * @param {object} draft
 * @returns {object|null}
 */
export function hfTokenApplyPayload(draft) {
  const d = normalizeDraft(draft);
  if (!d.hfTokenNew) return null;
  const id = d.hfTokenNewLabel;
  return {
    action: 'create-hf-token',
    token: { id, label: id, secret: d.hfTokenNew },
  };
}

/**
 * The `sizing` object travels to the conductor, which re-parses it into its own `SizingResult`.
 * Send it only when it would round-trip — a `gpu` and at least one complete `SessionsAtContext`
 * row — and omit it otherwise: the endpoint documents "pass the object the probe returned, or
 * omit it", and a half-shaped object is a 400 rather than a smaller answer.
 */
function wireSizing(draft, sizing) {
  const result = asObject(sizing ?? draft.sizing);
  if (Object.keys(result).length === 0) return null;
  if (!text(result.gpu)) return null;
  const rows = asArray(result.simultaneous_sessions_at).map(asObject);
  if (rows.length === 0) return null;
  const complete = rows.every((row) => toNumber(row.context_len) != null
    && toNumber(row.blocks_per_session) != null
    && toNumber(row.bytes_per_session) != null);
  return complete ? result : null;
}

/**
 * The `create-deployment` body for `POST /api/gpu/apply` (§5).
 *
 * No secret is included: the account already holds them, and the deployment references the
 * account by id. The sizing travels with it because the server derives `kv_pool_tokens`,
 * `max_context`, `max_concurrent`, `price_per_second` and the cold-start timeout from it.
 *
 * @param {object} draft
 * @param {object|null} sizing
 */
export function applyPayload(draft, sizing) {
  const d = normalizeDraft(draft);
  const deployment = {
    repo: d.repo,
    gpu: d.gpu,
    gpu_count: Math.max(1, Math.trunc(toNumber(d.gpuCount) ?? 1)),
    weight_quant: d.weightQuant || 'none',
    kv_cache_dtype: d.kvCacheDtype || 'auto',
    shutdown_cooldown_secs: clampScaledown(d.shutdownCooldownSecs).seconds,
    mode: d.mode === 'batch' ? 'batch' : 'streaming',
  };
  if (deployment.mode === 'batch') {
    // The recorded fill policy, not a hidden default: `full` carries no timeout at all, because a
    // number nobody reads is a number that looks like a cap.
    deployment.batch = { size: Math.max(1, Math.trunc(toNumber(d.batchSize) ?? 1)), fill: d.batchFill };
    if (d.batchFill === 'timeout') {
      deployment.batch.timeout_secs = Math.max(1, Math.trunc(toNumber(d.batchTimeoutSecs) ?? 1));
    }
  }
  if (d.revision) deployment.revision = d.revision;
  if (optionalInt(d.maxModelLen) != null) deployment.max_model_len = optionalInt(d.maxModelLen);
  if (optionalInt(d.maxNumSeqs) != null) deployment.max_num_seqs = optionalInt(d.maxNumSeqs);
  if (d.baseUrl) deployment.base_url = d.baseUrl;
  const wire = wireSizing(d, sizing);
  if (wire) deployment.sizing = wire;

  // The token is named by id beside the account id, not inside the account: the same token may be
  // named by deployments on several accounts, and this one may need none.
  const hfToken = d.hfTokenId || null;
  return hfToken
    ? { action: 'create-deployment', account_id: d.accountId, hf_token: hfToken, deployment }
    : { action: 'create-deployment', account_id: d.accountId, deployment };
}

// ---------------------------------------------------------------------------
// model name
// ---------------------------------------------------------------------------

/**
 * Read a parameter count out of a model name.
 *
 * Only a token that is *only* a number plus a unit counts, so `FP8`, `2024` and `3.8` produce
 * nothing: a wrong count is worse than none, because it becomes a VRAM figure. `8x7B` is
 * deliberately not expanded — a MoE total is not `8 × 7B`, and guessing it would be a
 * fabrication.
 *
 * @param {string} name
 * @returns {{value: number|null, token: string|null}}
 */
export function parseModelNameSize(name) {
  if (typeof name !== 'string' || name.trim() === '') return { value: null, token: null };
  const multipliers = { K: 1e3, M: 1e6, B: 1e9, T: 1e12 };
  const tokens = name.split(/[^A-Za-z0-9.]+/).filter(Boolean);
  for (const token of tokens) {
    const match = token.match(/^(\d+(?:\.\d+)?)([KMBT])$/i);
    if (!match) continue;
    const amount = Number(match[1]);
    if (!Number.isFinite(amount)) continue;
    const unit = match[2].toUpperCase();
    const value = Math.round(amount * multipliers[unit]);
    if (!Number.isFinite(value) || value <= 0) continue;
    return { value, token: `${match[1]}${unit}` };
  }
  return { value: null, token: null };
}

// ---------------------------------------------------------------------------
// fit
// ---------------------------------------------------------------------------

/** Per-card VRAM as the catalog states it, under either spelling. */
function cardVram(gpu) {
  const entry = asObject(gpu);
  const bytes = toNumber(entry.vram_bytes ?? entry.vramBytes);
  if (bytes != null && bytes > 0) return bytes;
  const gib = toNumber(entry.vram_gib ?? entry.vramGib);
  return gib != null && gib > 0 ? gib * 1024 ** 3 : null;
}

/**
 * Does this draft fit the card it names, and if not, what exactly would make it fit?
 *
 * WHY this exists as one function rather than as branches in the deploy window: "it does not fit"
 * is only useful when it says what to change, and each answer is a recomputation, not a guess. The
 * suggestions are produced by re-running `applySizing` with one field changed at a time, so a
 * suggestion is printed only when that change — on this repository, on this catalog — actually
 * produces a fit. Nothing here is a rule of thumb like "use a bigger card".
 *
 * The order of the suggestions is the order of the cost they impose: a different card changes
 * nothing about the model, a smaller model quant costs quality, a smaller cache quant costs
 * accuracy on long contexts, a smaller batch costs concurrency, a smaller context costs reach.
 * The first suggestion is therefore the one to take unless one of the later costs matters more.
 *
 * @param {object} draft
 * @param {object|null} sizing the result of `applySizing` for this draft
 * @param {object|null} catalog
 * @returns {{ok: boolean, known: boolean, blockers: Array<{code: string, message: string}>,
 *   suggestions: Array<{field: string, value: number|string, label: string, detail: string}>,
 *   fit: {gpu: string, gpu_count: number, pool: number|null, max_model_len: number|null,
 *     sessions_at_max_len: number|null, needed: number, mode: string}}}
 */
export function fitReport(draft, sizing, catalog = null) {
  const d = normalizeDraft(draft);
  const index = catalogIndex(catalog);
  const probe = asObject(d.probe);
  const result = asObject(sizing ?? d.sizing);
  const gpuId = text(d.gpu);
  const gpuCount = Math.max(1, Math.trunc(toNumber(d.gpuCount) ?? 1));
  const mode = d.mode === 'batch' ? 'batch' : 'streaming';
  // Both modes serve concurrent sessions; the batch size *is* the concurrency a batch deployment
  // is asked for, so a batch is judged against its own N and a streaming endpoint against the
  // sessions the operator says it must hold. Judging streaming at one session was the old
  // assumption that "streaming" meant "one request at a time", which one vLLM card disproves.
  const needed = mode === 'batch'
    ? Math.max(1, Math.trunc(toNumber(d.batchSize) ?? 1))
    : Math.max(1, Math.trunc(toNumber(d.sessions) ?? 1));
  const pool = toNumber(result.kv_pool_tokens);
  const budget = toNumber(result.kv_budget_bytes);
  const maxLen = toNumber(result.max_model_len);
  const atMax = toNumber(result.simultaneous_sessions_at_max_len);

  const fit = {
    gpu: gpuId,
    gpu_count: gpuCount,
    pool,
    max_model_len: maxLen,
    sessions_at_max_len: atMax,
    needed,
    mode,
  };
  const blockers = [];
  const suggestions = [];

  // `applySizing` answers with `{draft, sizing, warnings}`; the figures a candidate is judged on
  // live in `sizing`, so each reader goes through the envelope here instead of reading the
  // envelope's own (absent) pool and session fields, which made every candidate look like a
  // refusal and silently emptied the suggestion list.
  const sizes = (candidate) => asObject(applySizing({ ...d, ...candidate }, probe, catalog).sizing);
  const sessionCount = (next) => toNumber(asObject(next).simultaneous_sessions_at_max_len);
  const poolOf = (next) => toNumber(asObject(next).kv_pool_tokens);
  const holds = (next) => {
    const p = poolOf(next);
    const s = sessionCount(next);
    if (p == null || !(p > 0)) return false;
    // `sessions_at_max_len` is null when no context is known; the pool alone then has to hold the
    // sessions at the context vLLM would use, which is the only figure available.
    if (s != null) return s >= needed;
    const len = toNumber(asObject(next).max_model_len);
    return len == null ? false : p >= needed * len;
  };

  if (Object.keys(result).length === 0 || result.method === 'unavailable') {
    const missing = asArray(result.missing).map((entry) => text(entry)).filter(Boolean);
    blockers.push({
      code: 'unknown',
      message: missing.length
        ? `The KV pool cannot be estimated from this repository and card, so fit is unknown rather than good: ${missing.join('; ')}.`
        : 'The KV pool has not been estimated yet, so fit is unknown rather than good.',
    });
    return { ok: false, known: false, blockers, suggestions, fit };
  }

  if (pool == null) {
    blockers.push({ code: 'unknown', message: 'The sizing result carries no KV pool figure, so fit is unknown rather than good.' });
    return { ok: false, known: false, blockers, suggestions, fit };
  }

  if (budget != null && budget <= 0) {
    blockers.push({
      code: 'weights',
      message: `The weights (${gib(toNumber(result.weights_bytes) ?? 0)} GiB) plus the ${gib(OVERHEAD_BYTES)} GiB of overhead do not fit the ${gib(toNumber(result.vram_bytes) ?? 0)} GiB budget on ${gpuId || 'this card'}${gpuCount > 1 ? ` ×${gpuCount}` : ''}.`,
    });
  } else if (!(pool > 0)) {
    blockers.push({
      code: 'pool',
      message: `The KV pool on ${gpuId || 'this card'} is empty, so no session fits even though the weights do.`,
    });
  } else if (atMax != null && atMax < needed) {
    blockers.push({
      code: 'sessions',
      message: `${mode === 'batch' ? `A batch of ${needed}` : `${needed} concurrent session${needed === 1 ? '' : 's'}`} at the full ${maxLen ?? '?'} tokens needs ${needed} slot${needed === 1 ? '' : 's'}, and ${gpuId || 'this card'} holds ${atMax}.`,
    });
  }

  if (blockers.length === 0) return { ok: true, known: true, blockers, suggestions, fit };

  // --- what would make it fit ---------------------------------------------------------------
  // A card: the smallest VRAM increase on the same platform that holds this draft unchanged.
  const gpu = gpuId && hasOwn(index.gpuById, gpuId) ? index.gpuById[gpuId] : null;
  const currentVram = cardVram(gpu);
  const platformId = text(d.platform);
  const platform = platformId && hasOwn(index.platformById, platformId) ? index.platformById[platformId] : null;
  const siblings = asArray(platform?.gpus).map(asObject)
    .filter((entry) => text(entry.id))
    .map((entry) => ({ id: text(entry.id), vram: cardVram(entry) }))
    .filter((entry) => entry.vram != null)
    .sort((a, b) => a.vram - b.vram);
  const currentCountVram = currentVram == null ? null : currentVram * gpuCount;
  for (const sibling of siblings) {
    if (sibling.id === gpuId) continue;
    if (currentCountVram != null && sibling.vram <= currentCountVram) continue;
    const next = sizes({ gpu: sibling.id });
    if (!holds(next)) continue;
    suggestions.push({
      field: 'gpu',
      value: sibling.id,
      label: `Change GPU to ${sibling.id}`,
      detail: `${gib(sibling.vram)} GiB per card instead of ${currentVram == null ? 'the card you chose' : `${gib(currentVram)} GiB`} holds this model, quant and batch unchanged.`,
    });
    break;
  }

  // A smaller model quant, but only one the repository actually ships: a suggestion to requantise
  // is a different, hours-long job and is not what "make this fit" means here.
  const quants = quantOptions(probe, gpuId, catalog).available.filter((entry) => entry.inRepo);
  const currentBpp = quants.find((entry) => entry.id === text(d.weightQuant))?.bytesPerParam ?? null;
  const smaller = quants
    .filter((entry) => entry.id !== text(d.weightQuant))
    .filter((entry) => (currentBpp == null || entry.bytesPerParam == null ? true : entry.bytesPerParam < currentBpp))
    .sort((a, b) => (b.bytesPerParam ?? 0) - (a.bytesPerParam ?? 0));
  for (const quant of smaller) {
    const next = sizes({ weightQuant: quant.id });
    if (!holds(next)) continue;
    suggestions.push({
      field: 'weightQuant',
      value: quant.id,
      label: `Use the ${quant.label || quant.id} weights`,
      detail: `The repository ships ${quant.id}${quant.bytesPerParam == null ? '' : ` at ${quant.bytesPerParam} bytes per parameter`}; it loads on ${gpuId || 'this card'} where ${text(d.weightQuant) || 'the chosen quant'} does not.`,
    });
    break;
  }

  // A smaller cache quant the catalog says this card can execute.
  const dtypes = kvDtypesForGpu(catalog, gpuId).filter((entry) => entry.legal !== false && entry.id);
  const currentWidth = dtypes.find((entry) => entry.id === text(d.kvCacheDtype))?.bytesPerElement ?? null;
  const narrower = dtypes
    .filter((entry) => entry.id !== text(d.kvCacheDtype) && entry.id !== 'auto')
    .filter((entry) => (currentWidth == null || entry.bytesPerElement == null ? true : entry.bytesPerElement < currentWidth))
    .sort((a, b) => (b.bytesPerElement ?? 0) - (a.bytesPerElement ?? 0));
  for (const dtype of narrower) {
    const next = sizes({ kvCacheDtype: dtype.id });
    if (!holds(next)) continue;
    suggestions.push({
      field: 'kvCacheDtype',
      value: dtype.id,
      label: `Use the ${dtype.label || dtype.id} KV cache`,
      detail: `${dtype.bytesPerElement ?? '?'} bytes per element instead of ${currentWidth ?? '?'} halves the cache${dtype.bytesPerElement === 1 ? '' : ' per element'}, which is enough for this batch on ${gpuId || 'this card'}; a cache quant is not free of accuracy cost on long contexts.`,
    });
    break;
  }

  // A smaller concurrency figure: the pool divided by the full context is the largest number of
  // sequences that can coexist, and vLLM would otherwise start and then fail to schedule the rest.
  if (atMax != null && atMax >= 1 && atMax < needed) {
    suggestions.push({
      field: mode === 'batch' ? 'batchSize' : 'sessions',
      value: atMax,
      label: mode === 'batch' ? `Set the batch size to ${atMax}` : `Ask for ${atMax} session${atMax === 1 ? '' : 's'}`,
      detail: `At the full ${maxLen ?? '?'}-token context the pool holds ${atMax} sequence${atMax === 1 ? '' : 's'}; asking for more does not fail to start, it queues the extra requests until one finishes.`,
    });
  }

  // A smaller context: the largest reference context at which the requested batch still fits.
  if (maxLen != null && maxLen > 0) {
    const rungs = [...SIZING_CONTEXT_LADDER].filter((context) => context < maxLen);
    for (let i = rungs.length - 1; i >= 0; i -= 1) {
      const next = sizes({ maxModelLen: rungs[i] });
      if (!holds(next)) continue;
      suggestions.push({
        field: 'maxModelLen',
        value: rungs[i],
        label: `Cap the context at ${rungs[i]} tokens`,
        detail: `${needed} sequence${needed === 1 ? '' : 's'} fit at ${rungs[i]} tokens where ${maxLen} does not; requests longer than the cap are refused rather than truncated.`,
      });
      break;
    }
  }

  return { ok: false, known: true, blockers, suggestions, fit };
}
