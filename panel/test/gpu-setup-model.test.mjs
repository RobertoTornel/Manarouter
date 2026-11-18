// Tests for panel/gpu-setup-model.js — the GPU setup flow's model, run under bare `node --test`.
//
// Everything here is pure data: no DOM, no network, no `document`. The catalog fixtures are shaped
// like the one the panel reads at runtime (see docs/research/ARCHITECTURE-gpu-rentals.md §2), and
// every expected figure is the arithmetic worked in §7 of that document.
//
// Where an assertion pins a *string*, it is because the string is the honest statement of a limit —
// a "(Paid)" marker, a per-card ceiling, or a "this is not a validation" caveat — and weakening it
// would let the panel say something the research does not support.

import test from 'node:test';
import assert from 'node:assert/strict';

import * as model from '../gpu-setup-model.js';

const {
  ACTIVATION_RESERVE_BYTES,
  B300_CUDA_MIN_VERSION,
  BLOCK_SIZE,
  DEFAULT_GPU_MEMORY_UTILIZATION,
  GPU_COUNT_LIMIT_DEFAULT,
  H100_PIN_ID,
  MODAL_GPU_COUNT_LIMITS,
  MODAL_GPU_IDS,
  OVERHEAD_BYTES,
  PLAN_IDS,
  RENAMED_MODAL_GPU_IDS,
  SCALEDOWN_WINDOW_LIMITS,
  STEP_ORDER,
  accountApplyPayload,
  appSlug,
  applyPayload,
  applyProbe,
  applySizing,
  catalogIndex,
  deploymentBundle,
  emptyDraft,
  fitReport,
  hfTokenApplyPayload,
  kvDtypesForGpu,
  maxGpuCountFor,
  modalAppName,
  modalBaseUrl,
  nextStep,
  normalizeDraft,
  parseModelNameSize,
  planMarker,
  planMarkerForGpu,
  prevStep,
  quantOptions,
  quantsForGpu,
  requiresHfToken,
  stepById,
  stepList,
  toNumber,
  validateStep,
} = model;

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/**
 * Qwen3.8-27B's real topology (ARCHITECTURE-gpu-rentals.md §7.1): 64 declared layers, of which
 * only the 16 `full_attention` ones hold a per-token KV cache. The other 48 are linear-attention
 * layers with a fixed-size recurrent state.
 */
const FULL_ATTENTION_LAYERS = 16;
const DECLARED_LAYERS = 64;

const ARCH = {
  num_hidden_layers: DECLARED_LAYERS,
  layer_types: [
    ...Array(DECLARED_LAYERS - FULL_ATTENTION_LAYERS).fill('linear_attention'),
    ...Array(FULL_ATTENTION_LAYERS).fill('full_attention'),
  ],
  num_attention_heads: 24,
  num_key_value_heads: 4,
  head_dim: 256,
  hidden_size: 5120,
  max_position_embeddings: 262144,
  torch_dtype: 'bfloat16',
};

const PARAMS_TOTAL = 27_781_427_952;

/** The catalog shape the panel loads; `price_usd_per_second` is the key §2 documents. */
function makeCatalog(gpus) {
  return {
    platforms: [{ id: 'modal', label: 'Modal', free_plan: 'unknown', gpus }],
    quant_methods: [
      { id: 'fp8', label: 'FP8', bytes_per_param: 1, min_compute_cap: 8.9 },
      { id: 'awq', label: 'AWQ', bytes_per_param: 0.5, min_compute_cap: 8.9, produced_quant_method: 'compressed-tensors' },
      { id: 'bf16', label: 'BF16', bytes_per_param: 2, min_compute_cap: 8.0 },
      { id: 'none', label: 'None', bytes_per_param: 2, min_compute_cap: 8.0 },
    ],
    kv_cache_dtypes: [
      { id: 'auto' },
      { id: 'fp8', bytes_per_element: 1, min_compute_cap: 8.9 },
      { id: 'bfloat16', bytes_per_element: 2, min_compute_cap: 8.0 },
    ],
    measured_references: [
      {
        repo: 'Qwen/Qwen3.8-27B',
        gpu: 'RTX-PRO-6000',
        kv_pool_tokens: 1_777_758,
        kv_cache_dtype: 'fp8',
        max_model_len: 262144,
        source: 'observed',
        note: 'vLLM startup line, 185 s cold start',
        measured_at: '2026-05-04',
      },
      {
        repo: 'Qwen/Qwen3.8-27B',
        gpu: 'L40S',
        kv_pool_tokens: 281_729,
        kv_cache_dtype: 'fp8',
        max_model_len: 262144,
        source: 'observed',
      },
    ],
  };
}

const GPUS = [
  { id: 'RTX-PRO-6000', vram_gib: 96, usable_vram_gib: 95, compute_capability: '12.0', price_usd_per_second: 0.000842, free_plan: 'unknown' },
  { id: 'L40S', vram_gib: 48, usable_vram_gib: 46, compute_capability: '8.9', price_usd_per_second: 0.000542, free_plan: 'unknown' },
  { id: 'A10', vram_gib: 24, compute_capability: '8.6', price_usd_per_second: 0.000306, free_plan: 'unknown' },
  { id: 'H100', vram_gib: 80, compute_capability: '9.0', price_usd_per_second: 0.001097, free_plan: 'unavailable' },
  { id: 'B300', vram_gib: 288, compute_capability: '10.3', price_usd_per_second: 0.001972, free_plan: 'unknown' },
  { id: 'T4', vram_gib: 16, compute_capability: '7.5', price_usd_per_second: 0.000164, free_plan: 'unknown' },
];

const CATALOG = makeCatalog(GPUS);

/** A public FP8 repo whose files the probe resolved: 30.9 GB, rev `b32584cc` (issue-4-spec.md). */
function publicProbe(overrides = {}) {
  return {
    repo: 'Qwen/Qwen3.8-27B',
    revision: 'b32584cc',
    gated: { state: false },
    arch: ARCH,
    params: { total: PARAMS_TOTAL },
    quants: [{ id: 'fp8', in_repo: true }],
    sizing: { kv_pool_tokens: 1_777_758, kv_cache_dtype: 'fp8' },
    ...overrides,
  };
}

/** The gated, unmodified-FP8 repo the operator actually deployed (issue-4-spec.md). */
function gatedProbe(overrides = {}) {
  return publicProbe({
    repo: 'orcarouter/Qwen3.8-27B-Uncensored-FP8',
    revision: 'b32584cc',
    gated: { state: 'manual' },
    ...overrides,
  });
}

/** A complete, valid draft with the probe already applied. */
function draft(overrides = {}, catalog = CATALOG) {
  const probe = publicProbe();
  const base = {
    ...emptyDraft('modal'),
    platform: 'modal',
    accountId: 'mana-modal',
    workspace: 'mana',
    plan: 'paid',
    tokenId: 'wk-x',
    tokenSecret: 'ws-y',
    repo: 'Qwen/Qwen3.8-27B',
    revision: 'b32584cc',
    gpu: 'RTX-PRO-6000',
    gpuCount: 1,
    weightQuant: 'fp8',
    kvCacheDtype: 'fp8',
    maxModelLen: 262144,
    maxNumSeqs: 54,
    probe,
    ...overrides,
  };
  const sizing = base.sizing ?? applySizing(base, base.probe, catalog).sizing;
  return { ...base, sizing };
}

const CANONICAL = {
  vram_bytes: 103_079_215_104, // 96 GiB
  usable_vram_bytes: 102_005_473_280, // 95 GiB, the capacity the operator measured
  kv_bytes_per_token: 32_768,
  kv_budget_bytes: 58_461_515_351,
  kv_pool_tokens: 1_784_103,
};

// ---------------------------------------------------------------------------
// frozen interface
// ---------------------------------------------------------------------------

test('the frozen interface is exported with the documented arity', () => {
  const frozen = {
    emptyDraft: 1,
    stepList: 1,
    stepById: 2,
    nextStep: 2,
    prevStep: 2,
    validateStep: 2,
    catalogIndex: 1,
    quantsForGpu: 2,
    kvDtypesForGpu: 2,
    planMarker: 3,
    requiresHfToken: 1,
    quantOptions: 3,
    applyProbe: 3,
    applySizing: 3,
    deploymentBundle: 3,
    applyPayload: 2,
    parseModelNameSize: 1,
  };
  for (const [name, arity] of Object.entries(frozen)) {
    assert.equal(typeof model[name], 'function', `${name} must be exported as a function`);
    // Optional trailing args (catalog on validateStep, gpu on planMarker's spec form) are allowed,
    // so this is a floor rather than an equality for the widened ones.
    assert.ok(
      model[name].length >= arity - 1,
      `${name} should accept at least ${arity - 1} argument(s); got ${model[name].length}`,
    );
  }
});

test('step order is platform, account, model, card, sizing, review', () => {
  assert.deepEqual(STEP_ORDER, ['platform', 'account', 'model', 'card', 'sizing', 'review']);
});

// ---------------------------------------------------------------------------
// constants the research fixes
// ---------------------------------------------------------------------------

test('the overhead constant is 8 GiB and is the same term under both names', () => {
  assert.equal(OVERHEAD_BYTES, 8_589_934_592);
  assert.equal(ACTIVATION_RESERVE_BYTES, OVERHEAD_BYTES);
});

test('vLLM defaults match the documented ones: util 0.92, block size 64', () => {
  assert.equal(DEFAULT_GPU_MEMORY_UTILIZATION, 0.92);
  assert.equal(BLOCK_SIZE, 64);
});

test('scaledown_window bounds are Modal’s 2 s to 1200 s with a 60 s default', () => {
  assert.deepEqual(SCALEDOWN_WINDOW_LIMITS, { min: 2, max: 1200, default: 60 });
});

test('the canonical Modal GPU ids are all present, and A10G is only a rename', () => {
  for (const id of ['T4', 'L4', 'A10', 'L40S', 'A100', 'A100-40GB', 'A100-80GB', 'RTX-PRO-6000', 'H100', 'H200', 'B200', 'B300']) {
    assert.ok(MODAL_GPU_IDS.includes(id), `${id} must be a canonical Modal GPU id`);
  }
  assert.ok(!MODAL_GPU_IDS.includes('A10G'), 'A10G is not a Modal id; the card is A10');
  assert.equal(RENAMED_MODAL_GPU_IDS.A10G, 'A10');
});

test('A10 is the published per-container exception at 4 cards; everything else defaults to 8', () => {
  assert.deepEqual(MODAL_GPU_COUNT_LIMITS, { A10: 4 });
  assert.equal(GPU_COUNT_LIMIT_DEFAULT, 8);
  assert.equal(H100_PIN_ID, 'H100!');
});

test('B300 needs CUDA 13.1 or newer', () => {
  assert.equal(B300_CUDA_MIN_VERSION, '13.1');
});

test('plans are free, paid and unknown — never an invented fourth', () => {
  assert.deepEqual(PLAN_IDS, ['free', 'paid', 'unknown']);
});

// ---------------------------------------------------------------------------
// parseModelNameSize — the panel reads the name before it queries the repo
// ---------------------------------------------------------------------------

test('a parameter count in the repo name is read before anything is fetched', () => {
  const cases = [
    ['Qwen3.8-27B-Uncensored-FP8', 27_000_000_000, '27B'],
    ['Qwen3.8-27B', 27_000_000_000, '27B'],
    ['0.5B-x', 500_000_000, '0.5B'],
    ['7b', 7_000_000_000, '7B'],
    ['512M', 512_000_000, '512M'],
    ['1.5T', 1_500_000_000_000, '1.5T'],
    ['4096K', 4096_000, '4096K'],
    ['405B', 405_000_000_000, '405B'],
    ['-27B-', 27_000_000_000, '27B'],
  ];
  for (const [name, value, token] of cases) {
    assert.deepEqual(parseModelNameSize(name), { value, token }, name);
  }
});

test('a name with no size says so rather than pattern-matching a number out of it', () => {
  for (const name of ['no-size', '2024-01-01', 'fp8', 'Mixtral-8x7B', '42', '', null, undefined]) {
    assert.deepEqual(parseModelNameSize(name), { value: null, token: null }, String(name));
  }
});

// ---------------------------------------------------------------------------
// emptyDraft
// ---------------------------------------------------------------------------

test('emptyDraft fills every documented key and starts on Modal with no plan claimed', () => {
  const d = emptyDraft('modal');
  assert.deepEqual(d, {
    platform: 'modal',
    accountId: '',
    accountLabel: '',
    workspace: '',
    plan: 'unknown',
    tokenId: '',
    tokenSecret: '',
    apiTokenId: '',
    apiTokenSecret: '',
    extraAccounts: '',
    accountMode: 'new',
    hfTokenNeeded: false,
    hfTokenId: '',
    hfTokenNew: '',
    hfTokenNewLabel: '',
    repo: '',
    revision: '',
    gpu: '',
    gpuCount: 1,
    weightQuant: '',
    kvCacheDtype: 'auto',
    maxModelLen: null,
    maxNumSeqs: null,
    shutdownCooldownSecs: 60,
    mode: 'streaming',
    sessions: 1,
    batchSize: 8,
    batchFill: 'full',
    batchTimeoutSecs: 60,
    requantizeTo: '',
    gpuMemoryUtilization: 0.92,
    baseUrl: '',
    sizing: null,
    probe: null,
  });
});

test('the review says which mode the deployment is and how its batch fills', () => {
  const { deploymentShapeNote } = model;
  assert.equal(deploymentShapeNote(draft({ gpu: 'RTX-PRO-6000', sessions: 6 })),
    'Streaming on RTX-PRO-6000, holding 6 simultaneous sessions.');
  assert.equal(deploymentShapeNote(draft({ gpu: 'RTX-PRO-6000', sessions: 1 })),
    'Streaming on RTX-PRO-6000, holding 1 simultaneous session.');
  assert.equal(deploymentShapeNote(draft({ gpu: 'l40s', gpuCount: 2, sessions: 3 })),
    'Streaming on 2 × l40s, holding 3 simultaneous sessions.');
  assert.equal(deploymentShapeNote(draft({ mode: 'batch', batchSize: 8, batchFill: 'full', gpu: 'RTX-PRO-6000' })),
    'Batch of 8 on RTX-PRO-6000: a batch waits until it is full, however long that takes.');
  // The timeout is the half-full variant, and it names the wait it was given.
  assert.equal(deploymentShapeNote(draft({ mode: 'batch', batchSize: 4, batchFill: 'timeout', batchTimeoutSecs: 30, gpu: 'l40s' })),
    'Batch of 4 on l40s: a batch ships after 30 s even if it is not full.');
  // Never silent about a card that has not been chosen: the sentence is a readout, not a promise.
  assert.match(deploymentShapeNote(draft({ mode: 'batch', batchSize: 2, gpu: '' })), /a card not chosen yet/);
});

test('emptyDraft returns a fresh object each call', () => {
  const a = emptyDraft('modal');
  const b = emptyDraft('modal');
  a.repo = 'x/y';
  assert.equal(b.repo, '');
  assert.notEqual(a, b);
});

test('emptyDraft keeps an unknown platform id rather than defaulting silently to modal', () => {
  assert.equal(emptyDraft('cerebrium').platform, 'cerebrium');
  assert.equal(emptyDraft(null).platform, '');
});

// ---------------------------------------------------------------------------
// stepList / stepById / navigation
// ---------------------------------------------------------------------------

test('stepList has six steps in order with the documented titles', () => {
  const steps = stepList(emptyDraft('modal'));
  assert.deepEqual(steps.map((s) => s.id), STEP_ORDER);
  const titles = steps.map((s) => s.title);
  for (const title of titles) assert.equal(typeof title, 'string');
  for (const step of steps) {
    assert.ok(Array.isArray(step.fields) && step.fields.length > 0, `${step.id} must have fields`);
    assert.equal(typeof step.hint, 'string');
  }
});

test('every field carries the same shape, so the panel can render one loop', () => {
  const shape = [
    'name', 'label', 'kind', 'hint', 'required', 'visible', 'enabled',
    'options', 'optionsSource', 'value', 'placeholder', 'min', 'max', 'step',
    'unit', 'readOnly', 'notes',
  ];
  for (const step of stepList(draft(), CATALOG)) {
    for (const field of step.fields) {
      assert.deepEqual(Object.keys(field).sort(), [...shape].sort(), `${step.id}.${field.name}`);
      assert.ok(Array.isArray(field.notes), `${step.id}.${field.name}.notes must be an array`);
      assert.equal(typeof field.name, 'string');
      assert.ok(['text', 'password', 'number', 'select', 'choice', 'readout', 'toggle', 'textarea'].includes(field.kind),
        `${step.id}.${field.name} has an unexpected kind ${field.kind}`);
    }
  }
});

test('stepList field inventory is stable per step', () => {
  const inventory = Object.fromEntries(
    stepList(emptyDraft('modal'), CATALOG).map((s) => [s.id, s.fields.map((f) => f.name)]),
  );
  assert.deepEqual(inventory, {
    platform: ['platform', 'platformDeployment'],
    // Two controls write the same draft key: a text input for a new id, and a select of recorded
    // ones for a reuse. Exactly one is visible, keyed by `accountMode`, so the duplicate is the
    // inventory being honest rather than two fields.
    account: [
      'accountMode', 'accountId', 'accountId', 'extraAccounts', 'workspace', 'plan', 'tokenId',
      'tokenSecret', 'apiTokenId', 'apiTokenSecret', 'planMarker', 'accountCredentialNote',
    ],
    model: [
      'repo', 'revision', 'hfTokenId', 'hfTokenNew', 'hfTokenNewLabel',
      'probeGated', 'probeParams', 'probeArch', 'probeQuants', 'probeWarnings',
    ],
    card: ['gpu', 'gpuCount', 'mode', 'gpuMemoryUtilization', 'weightQuant', 'kvCacheDtype', 'requantizeTo', 'gpuPrice', 'cardPlanMarker'],
    sizing: [
      'maxModelLen', 'maxNumSeqs', 'sessions', 'batchSize', 'batchFill', 'batchTimeoutSecs',
      'batchWaitNote', 'shutdownCooldownSecs',
      'sizingKvBasis', 'sizingKvPool', 'sizingMeasured', 'sizingCalibration',
      'sizingAssumptions', 'sizingUnavailable', 'sizingWarnings',
    ],
    review: [
      'deploymentShape','endpointUrl', 'baseUrl', 'bundleFilename', 'bundleCommand', 'bundleNotes', 'bundleWarnings', 'sizingDelta'],
  });
});

test('the model step names a recorded HF token, and only for a gated repo', () => {
  const fields = (d) => stepList(d, CATALOG).find((s) => s.id === 'model').fields;
  const byName = (d, name) => fields(d).find((f) => f.name === name);
  assert.equal(byName(draft(), 'hfTokenId').visible, false,
    'a public repository names no token, so the selector is not offered');
  const gated = draft({ probe: gatedProbe(), hfTokenNeeded: true });
  const select = byName(gated, 'hfTokenId');
  assert.equal(select.visible, true);
  assert.equal(select.kind, 'select');
  assert.equal(select.optionsSource, 'hfTokens', 'the list is the recorded tokens, not an account field');
  assert.equal(select.required, false, 'a token is named by id and may be added on the same step');
  const pasted = byName(gated, 'hfTokenNew');
  assert.equal(pasted.visible, true);
  assert.equal(pasted.kind, 'password', 'a pasted token is a secret field');
  assert.equal(byName(draft({ hfTokenNew: 'hf_x', hfTokenNeeded: true }), 'hfTokenNewLabel').visible, true,
    'a pasted token needs an id to be named by later');
});

test('the card step offers one mode axis, and batch is a choice not a second card', () => {
  const fields = (d) => stepList(d, CATALOG).find((s) => s.id === 'card').fields;
  const mode = fields(draft()).find((f) => f.name === 'mode');
  assert.equal(mode.kind, 'choice', 'two modes are radio cards, not a select and a second GPU field');
  assert.equal(mode.optionsSource, 'modes');
  assert.equal(mode.value, 'streaming');
  assert.equal(fields(draft({ mode: 'batch' })).find((f) => f.name === 'mode').value, 'batch');
  for (const step of stepList(draft(), CATALOG)) {
    assert.ok(!step.fields.some((f) => /batch.?gpu/i.test(f.name)),
      `${step.id} must carry no second-card field: one card, one job`);
  }
});

test('credentials are password fields and the bundle is read-only', () => {
  const all = stepList(draft(), CATALOG).flatMap((s) => s.fields);
  const byName = Object.fromEntries(all.map((f) => [f.name, f]));
  assert.equal(byName.tokenId.kind, 'password');
  assert.equal(byName.tokenSecret.kind, 'password');
  assert.equal(byName.bundleFilename.readOnly, true);
  assert.equal(byName.bundleCommand.readOnly, true);
});

test('stepById takes the draft first and returns null for anything unknown', () => {
  const d = draft();
  assert.equal(stepById(d, 'model').id, 'model');
  assert.equal(stepById(d, 'review').id, 'review');
  assert.equal(stepById(d, null), null);
  assert.equal(stepById(d, 'nope'), null);
  assert.equal(stepById(d, ''), null);
});

test('navigation is order only, not validity', () => {
  const invalid = emptyDraft('modal');
  assert.equal(nextStep(invalid, 'platform'), 'account');
  assert.equal(nextStep(invalid, 'sizing'), 'review');
  assert.equal(nextStep(invalid, 'review'), null);
  assert.equal(nextStep(invalid, 'nope'), null);
  assert.equal(prevStep(invalid, 'account'), 'platform');
  assert.equal(prevStep(invalid, 'platform'), null);
  assert.equal(prevStep(invalid, 'nope'), null);
});

// ---------------------------------------------------------------------------
// validateStep
// ---------------------------------------------------------------------------

const problemsOf = (d, step, catalog = CATALOG) => validateStep(d, step, catalog).problems;

test('a complete draft validates on every step', () => {
  const d = draft();
  for (const step of STEP_ORDER) {
    const result = validateStep(d, step, CATALOG);
    assert.equal(result.ok, true, `${step}: ${JSON.stringify(result.problems)}`);
    assert.deepEqual(result.problems, []);
    assert.deepEqual(result.errors, []);
  }
});

test('an unknown step is an error, not a problem on a field', () => {
  const result = validateStep(draft(), 'nope', CATALOG);
  assert.equal(result.ok, false);
  assert.deepEqual(result.problems, []);
  assert.equal(result.errors.length, 1);
  assert.equal(result.errors[0].code, 'unknown_step');
  assert.equal(
    result.errors[0].message,
    'There is no setup step "nope". Known steps: platform, account, model, card, sizing, review.',
  );
  assert.deepEqual(result.errors[0].details, []);
});

test('an empty repo blocks the model step before anything is fetched', () => {
  assert.deepEqual(problemsOf(draft({ repo: '' }), 'model'), [
    { field: 'repo', message: 'Paste a HuggingFace repo id (owner/name). Nothing is fetched until it is non-empty.' },
    { field: 'repo', message: 'The probe result is for "Qwen/Qwen3.8-27B", not this repo. Probe again before continuing.' },
  ]);
});

test('a repo id must look like owner/name', () => {
  assert.deepEqual(problemsOf(draft({ repo: 'justname' }), 'model'), [
    { field: 'repo', message: 'A repo id looks like owner/name, e.g. orcarouter/Qwen3.8-27B-Uncensored-FP8.' },
    { field: 'repo', message: 'The probe result is for "Qwen/Qwen3.8-27B", not this repo. Probe again before continuing.' },
  ]);
});

test('a probe that belongs to another repo is never silently reused', () => {
  assert.deepEqual(problemsOf(draft({ repo: 'other/x' }), 'model'), [
    { field: 'repo', message: 'The probe result is for "Qwen/Qwen3.8-27B", not this repo. Probe again before continuing.' },
  ]);
});

test('a gated repo needs a token named or pasted before the model step passes', () => {
  const gated = draft({
    probe: gatedProbe(), repo: 'orcarouter/Qwen3.8-27B-Uncensored-FP8', hfTokenNeeded: true,
    hfTokenId: '', hfTokenNew: '', hfTokenNewLabel: '',
  });
  const problems = problemsOf(gated, 'model');
  assert.equal(problems.length, 1);
  assert.equal(problems[0].field, 'hfTokenId');
  assert.match(problems[0].message, /token/i);
  assert.deepEqual(problemsOf({ ...gated, hfTokenId: 'hub-main' }, 'model'), [],
    'naming a recorded token is enough');
  assert.deepEqual(problemsOf({ ...gated, hfTokenNew: 'hf_placeholder', hfTokenNewLabel: 'hub-new' }, 'model'), [],
    'pasting a new one is enough too');
  assert.equal(problemsOf({ ...gated, hfTokenNew: 'hf_placeholder', hfTokenNewLabel: '' }, 'model').length, 1,
    'a pasted token without an id could not be named by a later rental');
});

test('the card step requires a GPU and at least one card', () => {
  assert.deepEqual(problemsOf(draft({ gpu: '' }), 'card'), [
    { field: 'gpu', message: 'Choose a GPU from the catalog.' },
  ]);
  assert.deepEqual(problemsOf(draft({ gpuCount: 0 }), 'card'), [
    { field: 'gpuCount', message: 'Card count must be a whole number of at least 1.' },
  ]);
});

test('per-card container limits are enforced from the card, and A10 is the exception at 4', () => {
  assert.deepEqual(problemsOf(draft({ gpu: 'A10', gpuCount: 8 }), 'card'), [
    { field: 'gpuCount', message: 'A10 runs at most 4 cards per container, so 8 is not a configuration Modal will accept.' },
  ]);
  assert.deepEqual(problemsOf(draft({ gpu: 'A10', gpuCount: 4 }), 'card'), []);
  assert.deepEqual(problemsOf(draft({ gpu: 'H100', gpuCount: 8 }), 'card'), []);
  assert.deepEqual(problemsOf(draft({ gpu: 'H100', gpuCount: 9 }), 'card'), [
    { field: 'gpuCount', message: 'H100 runs at most 8 cards per container, so 9 is not a configuration Modal will accept.' },
  ]);
});

test('the per-card limit holds without a catalog, because it is a Modal fact', () => {
  assert.deepEqual(problemsOf(draft({ gpu: 'A10', gpuCount: 8 }), 'card', null), [
    { field: 'gpuCount', message: 'A10 runs at most 4 cards per container, so 8 is not a configuration Modal will accept.' },
  ]);
});

test('gpu memory utilization must be in (0, 1]', () => {
  const message = 'GPU memory utilization must be greater than 0 and at most 1 (vLLM’s default is 0.92).';
  for (const value of [1.5, 0, -0.1, 2]) {
    assert.deepEqual(problemsOf(draft({ gpuMemoryUtilization: value }), 'card'), [
      { field: 'gpuMemoryUtilization', message },
    ]);
  }
  assert.deepEqual(problemsOf(draft({ gpuMemoryUtilization: 0.9 }), 'card'), []);
  assert.deepEqual(problemsOf(draft({ gpuMemoryUtilization: 1 }), 'card'), []);
});

test('scaledown_window is clamped to Modal’s 2–1200 s', () => {
  const message = 'Modal accepts scaledown_window from 2 to 1200 seconds (default 60).';
  for (const value of [1, 0, 1201, 5000]) {
    assert.deepEqual(problemsOf(draft({ shutdownCooldownSecs: value }), 'sizing'), [
      { field: 'shutdownCooldownSecs', message },
    ]);
  }
  for (const value of [2, 60, 1200]) {
    assert.deepEqual(problemsOf(draft({ shutdownCooldownSecs: value }), 'sizing'), []);
  }
});

test('mode is exactly one of streaming or batch, and the legacy spelling never survives a draft', () => {
  assert.equal(normalizeDraft(draft({ mode: 'batch' })).mode, 'batch');
  assert.equal(normalizeDraft(draft({ mode: 'streaming' })).mode, 'streaming');
  assert.equal(normalizeDraft(draft({ mode: 'interactive' })).mode, 'streaming',
    'older drafts said interactive; streaming is that same single mode under its wire name');
  assert.equal(normalizeDraft(draft({ mode: 'nonsense' })).mode, 'streaming');
  assert.equal(applyPayload(draft({ mode: 'batch' }), null).deployment.mode, 'batch');
  assert.equal(applyPayload(draft({ mode: 'interactive' }), null).deployment.mode, 'streaming');
});

test('the endpoint URL is derived from the workspace and app name, and only an override is validated', () => {
  const derived = modalBaseUrl(draft());
  assert.equal(derived, 'https://mana--manarouter-rental-mana-modal-qwen3-8-27b-streaming-rtx-pro-6000-serve.modal.run');
  assert.deepEqual(problemsOf(draft({ baseUrl: '' }), 'review'), [],
    'the normal path pastes nothing: the workspace and app name are enough');
  assert.deepEqual(problemsOf(draft({ workspace: '', baseUrl: '' }), 'review'), [
    { field: 'baseUrl', message: 'No endpoint URL can be derived: record the Modal workspace on the account step, or paste the URL `modal deploy` prints.' },
  ]);
  assert.deepEqual(problemsOf(draft({ baseUrl: 'http://mana--x.modal.run' }), 'review'), [
    { field: 'baseUrl', message: 'Use https://: the relay sends proxy-token headers, and they must not travel in clear text.' },
  ]);
  assert.deepEqual(problemsOf(draft({ baseUrl: 'https://mana.example.com' }), 'review'), [
    { field: 'baseUrl', message: '"mana.example.com" is not a *.modal.run host. `modal deploy` prints the app\'s real URL.' },
  ]);
  assert.deepEqual(problemsOf(draft({ baseUrl: 'https://other--app.modal.run' }), 'review'), []);
  const review = stepList(draft(), CATALOG).find((s) => s.id === 'review').fields;
  const field = review.find((f) => f.name === 'baseUrl');
  assert.equal(field.required, false, 'an override is optional; the derived URL is the default');
  assert.equal(review.find((f) => f.name === 'endpointUrl').value, derived,
    'the review shows the derived URL, not a blank readout waiting for a paste');
});

test('a long account and repository are trimmed from the front so the finished app name stays under 64 characters', () => {
  // Modal refuses a name of 64 characters or more, and it measures the finished name — the
  // `manarouter-` prefix counts. A long account and repository once produced a 79-character name
  // and `modal deploy` rejected it, so the budget is spent on the whole name, not just the slug.
  const repo = `org/${'very-long-model-name-'.repeat(6)}`;
  const app = modalAppName('an-account-with-a-long-id', repo, 'batch', 'RTX-PRO-6000', 4);
  assert.ok(app.length < 64, `\`${app}\` is ${app.length} characters and Modal will refuse it`);
  assert.ok(app.startsWith('manarouter-'), `got \`${app}\``);
  assert.ok(app.endsWith('-batch-rtx-pro-6000-x4'), `got \`${app}\``);
  assert.ok(!app.includes('--'), 'trimming a trailing dash must not leave a double dash');
  // The suffix is what separates two deployments of one checkpoint; truncating the tail would
  // collapse them onto one name.
  const streaming = modalAppName('an-account-with-a-long-id', repo, 'streaming', 'RTX-PRO-6000', 4);
  assert.notEqual(app, streaming);
  assert.ok(streaming.length < 64);
  // Short names are untouched by the wider budget.
  assert.equal(appSlug('mana-modal', 'Qwen/Qwen3-8B', 'streaming', 'RTX-PRO-6000', 1),
    'rental-mana-modal-qwen3-8b-streaming-rtx-pro-6000');
});

test('an API token in the proxy-token slot is rejected with the reason', () => {
  assert.deepEqual(problemsOf(draft({ tokenId: 'ak-abc123' }), 'account'), [
    { field: 'tokenId', message: 'That is a Modal API token (ak-…), not a proxy token. The relay sends proxy tokens as Modal-Key (wk-…).' },
  ]);
  assert.deepEqual(problemsOf(draft({ tokenId: '' }), 'account'), [
    { field: 'tokenId', message: 'The Modal proxy token id is required (Modal-Key).' },
  ]);
  assert.deepEqual(problemsOf(draft({ accountId: '' }), 'account'), [
    { field: 'accountId', message: 'An account id is required: it names the vault entry and the deployment’s billing account.' },
  ]);
  assert.deepEqual(problemsOf(draft({ plan: 'bogus' }), 'account'), [
    { field: 'plan', message: 'Plan must be one of free, paid, unknown; leave it on unknown rather than guessing.' },
  ]);
});

test('the platform step rejects an id with whitespace but does not invent a catalog check', () => {
  assert.deepEqual(problemsOf(draft({ platform: '' }), 'platform'), [
    { field: 'platform', message: 'Choose a platform. The catalog decides which ones exist.' },
  ]);
  assert.deepEqual(problemsOf(draft({ platform: 'mo dal' }), 'platform'), [
    { field: 'platform', message: '"mo dal" is not a catalog platform id.' },
  ]);
  // With no catalog there is nothing to check an id against, so it is accepted as written.
  assert.deepEqual(problemsOf(draft({ platform: 'nope' }), 'platform'), []);
});

test('a max model length must be whole tokens or empty', () => {
  assert.deepEqual(problemsOf(draft({ maxModelLen: 'abc' }), 'sizing'), [
    { field: 'maxModelLen', message: 'Max model length must be a whole number of tokens, or empty to use the config’s value.' },
  ]);
  assert.deepEqual(problemsOf(draft({ maxModelLen: null }), 'sizing'), []);
});

test('numeric fields accept the strings a form actually produces', () => {
  const strings = draft({
    gpuCount: '2',
    maxNumSeqs: '54',
    shutdownCooldownSecs: '60',
    maxModelLen: '262144',
    gpuMemoryUtilization: '0.9',
  });
  for (const step of ['card', 'sizing']) {
    assert.deepEqual(problemsOf(strings, step), [], step);
  }
});

test('a weight quant must be one the probed repository ships; re-quantising is a separate choice', () => {
  assert.deepEqual(problemsOf(draft({ weightQuant: 'zzz' }), 'card'), [{
    field: 'weightQuant',
    message: 'The probed repository ships fp8; "zzz" is not one of them. Choose it under "Requantise to (optional)" as a future GPU job instead.',
  }]);
  assert.deepEqual(problemsOf(draft({ weightQuant: 'awq' }), 'card'), [{
    field: 'weightQuant',
    message: 'The probed repository ships fp8; "awq" is not one of them. Choose it under "Requantise to (optional)" as a future GPU job instead.',
  }]);
  assert.deepEqual(problemsOf(draft({ weightQuant: 'fp8' }), 'card'), []);
  // Choosing a target in the requantise field is valid recorded intent, not a validation failure.
  assert.deepEqual(problemsOf(draft({ weightQuant: 'awq', requantizeTo: 'awq' }), 'card'), [{
    field: 'weightQuant',
    message: 'The probed repository ships fp8; "awq" is not one of them. Choose it under "Requantise to (optional)" as a future GPU job instead.',
  }], 'the requantise field does not make awq the weight quant this deployment serves');
  // The KV dtype has no repo-level vocabulary in the catalog, so it stays unchecked (documented gap).
  assert.deepEqual(problemsOf(draft({ kvCacheDtype: 'zzz' }), 'card'), []);
});

// ---------------------------------------------------------------------------
// catalogIndex / lookups
// ---------------------------------------------------------------------------

test('catalogIndex exposes lookups by id and no invented gpus array', () => {
  const index = catalogIndex(CATALOG);
  assert.deepEqual(Object.keys(index), [
    'platformById', 'gpuById', 'quantById', 'kvDtypeById',
    'gpuPlatformById', 'platforms', 'quants', 'kvDtypes', 'measuredReferences', 'catalog',
  ]);
  assert.equal('gpus' in index, false);
  assert.equal(index.gpuById['RTX-PRO-6000'].vram_gib, 96);
  assert.equal(index.platformById.modal.id, 'modal');
  assert.equal(index.quantById.fp8.bytes_per_param, 1);
  assert.equal(index.kvDtypeById.fp8.bytes_per_element, 1);
  assert.equal(index.gpuPlatformById['RTX-PRO-6000'], 'modal');
  assert.equal(index.measuredReferences.length, 2);
});

test('catalogIndex survives an empty or absent catalog', () => {
  for (const empty of [null, undefined, {}, { platforms: [] }]) {
    const index = catalogIndex(empty);
    assert.deepEqual(index.platforms, []);
    assert.deepEqual(index.quants, []);
    assert.deepEqual(index.measuredReferences, []);
    assert.equal(typeof index.gpuById, 'object');
  }
});

test('maxGpuCountFor reads the card, then the Modal table, then the default', () => {
  assert.equal(maxGpuCountFor('A10', CATALOG), 4);
  assert.equal(maxGpuCountFor('A10'), 4);
  assert.equal(maxGpuCountFor('H100', CATALOG), 8);
  assert.equal(maxGpuCountFor('RTX-PRO-6000', CATALOG), 8);
  assert.equal(maxGpuCountFor(null), 8);
  assert.equal(maxGpuCountFor(undefined), 8);
  // A legacy id is not a Modal id, so it gets the default rather than a fabricated exception.
  assert.equal(maxGpuCountFor('A10G', CATALOG), 8);
});

test('maxGpuCountFor prefers a positive figure the catalog states itself', () => {
  const catalog = makeCatalog([{ id: 'H100', vram_gib: 80, compute_capability: '9.0', max_gpu_count: 2 }]);
  assert.equal(maxGpuCountFor('H100', catalog), 2);
});

test('toNumber refuses whitespace and booleans instead of coercing them to numbers', () => {
  for (const value of [null, undefined, '', '  ', 'abc', true, false, {}, []]) {
    assert.equal(toNumber(value), null, `toNumber(${JSON.stringify(value)})`);
  }
  assert.equal(toNumber(12), 12);
  assert.equal(toNumber('12'), 12);
  assert.equal(toNumber('0.9'), 0.9);
  assert.equal(toNumber('1e3'), 1000);
  assert.equal(toNumber(' 12 '), 12);
  assert.equal(toNumber(NaN), null);
});

// ---------------------------------------------------------------------------
// quantsForGpu / kvDtypesForGpu
// ---------------------------------------------------------------------------

test('an FP8 card is legal for fp8 and says which capability it checked', () => {
  const fp8 = quantsForGpu(CATALOG, 'RTX-PRO-6000').find((q) => q.id === 'fp8');
  assert.deepEqual(fp8, {
    id: 'fp8',
    label: 'FP8',
    legal: true,
    reason: "RTX-PRO-6000 has compute capability 12.0, which meets FP8's 8.9.",
    confidence: 'documented',
    bytesPerParam: 1,
    minComputeCap: 8.9,
  });
});

test('A10 cannot execute fp8 (8.6 < 8.9) but can execute bf16', () => {
  const quants = quantsForGpu(CATALOG, 'A10');
  const fp8 = quants.find((q) => q.id === 'fp8');
  assert.equal(fp8.legal, false);
  assert.equal(fp8.reason, 'FP8 needs compute capability ≥ 8.9; A10 has 8.6.');
  const bf16 = quants.find((q) => q.id === 'bf16');
  assert.equal(bf16.legal, true);
  assert.equal(bf16.reason, "A10 has compute capability 8.6, which meets BF16's 8.0.");
});

test('an unknown card yields unverified, not a guess in either direction', () => {
  const quants = quantsForGpu(CATALOG, 'NOPE');
  assert.equal(quants.length, CATALOG.quant_methods.length);
  for (const quant of quants) {
    assert.equal(quant.legal, null);
    assert.equal(quant.confidence, 'unverified');
    assert.equal(quant.reason, `The catalog has no GPU "NOPE", so the quantisation "${quant.id}" cannot be checked against it.`);
  }
  const none = quantsForGpu(CATALOG, null).find((q) => q.id === 'fp8');
  assert.equal(none.legal, null);
  assert.match(none.reason, /"\(none\)"/);
  assert.deepEqual(quantsForGpu(null, 'A10'), []);
});

test('kvDtypesForGpu reports the element width with each dtype and checks its own minimum', () => {
  const dtypes = kvDtypesForGpu(CATALOG, 'A10');
  const fp8 = dtypes.find((d) => d.id === 'fp8');
  assert.deepEqual(fp8, {
    id: 'fp8',
    label: 'fp8',
    legal: false,
    reason: 'fp8 needs compute capability ≥ 8.9; A10 has 8.6.',
    confidence: 'documented',
    bytesPerElement: 1,
  });
  const bf = dtypes.find((d) => d.id === 'bfloat16');
  assert.equal(bf.legal, true);
  assert.equal(bf.bytesPerElement, 2);
});

test('an "auto" KV dtype carries no width, so it cannot be confirmed against any card', () => {
  const auto = kvDtypesForGpu(CATALOG, 'A10').find((d) => d.id === 'auto');
  assert.equal(auto.legal, null);
  assert.equal(auto.bytesPerElement, null);
  assert.equal(auto.confidence, 'unverified');
  assert.equal(auto.reason, 'The catalog does not record a compute capability for auto, so A10 cannot be confirmed.');

  const rt = kvDtypesForGpu(CATALOG, 'RTX-PRO-6000').find((d) => d.id === 'fp8');
  assert.equal(rt.legal, true);
  assert.equal(rt.reason, "RTX-PRO-6000 has compute capability 12.0, which meets fp8's 8.9.");
});

test('kvDtypesForGpu falls back to unverified without a card', () => {
  for (const dtype of kvDtypesForGpu(CATALOG, null)) {
    assert.equal(dtype.legal, null);
    assert.equal(dtype.confidence, 'unverified');
  }
});

// ---------------------------------------------------------------------------
// planMarker
// ---------------------------------------------------------------------------

test('an unknown plan is never rendered as free or as paid', () => {
  assert.deepEqual(planMarker(CATALOG, 'modal', 'unknown'), {
    paid: false,
    text: null,
    provenance: 'The account plan is unknown, so no plan marker is shown: an unrecorded plan is never rendered as free or as paid.',
  });
  for (const bogus of [null, '', 'bogus']) {
    assert.equal(planMarker(CATALOG, 'modal', bogus).text, null);
  }
});

test('with no card chosen there is no per-card free-tier fact to show, so no marker', () => {
  assert.deepEqual(planMarker(CATALOG, 'modal', 'paid'), {
    paid: false,
    text: null,
    provenance: 'The plan is recorded as "paid", but no card is selected and the catalog states no platform-level free_plan, so there is no free-tier fact to show.',
  });
});

test('(Paid) needs a catalog fact, and a platform-level one is used before a card is chosen', () => {
  const catalog = makeCatalog(GPUS);
  catalog.platforms[0].free_plan = 'unavailable';
  const marker = planMarker(catalog, 'modal', 'paid');
  assert.equal(marker.paid, true);
  assert.equal(marker.text, '(Paid)');
  assert.match(marker.provenance, /platform level/);
});

test('a per-card free_plan of unavailable marks the card paid', () => {
  const marker = planMarkerForGpu(CATALOG, 'modal', 'paid', 'H100');
  assert.deepEqual(marker, {
    paid: true,
    text: '(Paid)',
    provenance: 'H100 free_plan: "unavailable" (source: unknown); the plan is recorded as "paid".',
  });
});

test('a card with no free-tier fact gets no marker, because Modal documents no per-GPU gating', () => {
  for (const gpu of ['T4', 'L40S', 'A10', 'B300', 'RTX-PRO-6000']) {
    const marker = planMarkerForGpu(CATALOG, 'modal', 'paid', gpu);
    assert.equal(marker.paid, false, gpu);
    assert.equal(marker.text, null, gpu);
    assert.equal(
      marker.provenance,
      `${gpu} free_plan: "unknown" (source: unknown); Modal documents no per-GPU free-tier gating, so showing "(Paid)" here would be a fabrication.`,
    );
  }
});

test('a free plan on a card that needs a paid one keeps the marker and says why', () => {
  const marker = planMarkerForGpu(CATALOG, 'modal', 'free', 'H100');
  assert.equal(marker.text, '(Paid)');
  assert.match(marker.provenance, /contradicts a card documented as unavailable on the free tier/);
});

test('an unknown platform or card yields no marker', () => {
  assert.deepEqual(planMarker(CATALOG, 'nope', 'paid'), {
    paid: false,
    text: null,
    provenance: 'Platform "nope" is not in the catalog, so no plan marker is shown.',
  });
  assert.equal(planMarker(null, 'modal', 'paid').text, null);
  assert.deepEqual(planMarkerForGpu(CATALOG, 'modal', 'paid', 'V100'), {
    paid: false,
    text: null,
    provenance: 'The catalog has no GPU "V100", so no plan marker is shown.',
  });
});

// ---------------------------------------------------------------------------
// requiresHfToken
// ---------------------------------------------------------------------------

test('a gated repo needs a token, in every shape the probe might report it', () => {
  for (const gated of [true, 'auto', 'manual', { state: 'manual' }, { state: 'auto' }, { needs_token: true }]) {
    assert.equal(requiresHfToken({ gated }), true, JSON.stringify(gated));
  }
});

test('an ungated or unresolved repo does not claim a token is needed', () => {
  for (const gated of [false, 'false', 'unknown-string', null, undefined]) {
    assert.equal(requiresHfToken({ gated }), false, JSON.stringify(gated));
  }
  for (const probe of [{}, null, undefined, 'x', 3, []]) {
    assert.equal(requiresHfToken(probe), false, JSON.stringify(probe));
  }
});

// ---------------------------------------------------------------------------
// quantOptions
// ---------------------------------------------------------------------------

test('quantOptions lists what the repo ships and why each other quant is unavailable', () => {
  const options = quantOptions(publicProbe(), 'RTX-PRO-6000', CATALOG);
  assert.deepEqual(Object.keys(options), ['available', 'unavailable']);
  assert.deepEqual(options.available, [{
    id: 'fp8',
    label: 'FP8',
    legal: true,
    legalReason: "RTX-PRO-6000 has compute capability 12.0, which meets FP8's 8.9.",
    confidence: 'documented',
    bytesPerParam: 1,
    evidence: null,
    inRepo: true,
  }]);
  assert.deepEqual(options.unavailable.map((u) => u.id), ['awq', 'bf16', 'none']);
  assert.equal(options.unavailable[0].reason, 'The repository ships fp8; it does not ship awq.');
});

test('a quant the card cannot execute is unavailable for that reason, not silently dropped', () => {
  const options = quantOptions(publicProbe(), 'A10', CATALOG);
  const fp8 = options.unavailable.find((u) => u.id === 'fp8');
  assert.ok(fp8, 'fp8 must appear as unavailable on A10');
  assert.match(fp8.reason, /cannot execute it/);
  assert.match(fp8.reason, /FP8 needs compute capability ≥ 8\.9; A10 has 8\.6\./);
});

test('the quant list stays blank until a repo has actually been probed', () => {
  // Requirement 5's ordering: the repo is chosen first, and nothing about its quants is shown
  // until the probe has answered. A blank repo therefore yields no available quant at all.
  const fresh = emptyDraft('modal');
  assert.deepEqual(quantOptions(fresh.probe, 'RTX-PRO-6000', CATALOG).available, []);
  assert.deepEqual(quantOptions(null, 'A10', CATALOG).available, []);
});

test('quantOptions on a missing probe or catalog answers empty rather than guessing', () => {
  assert.deepEqual(quantOptions(null, null, null), { available: [], unavailable: [] });
  const noProbe = quantOptions(null, 'RTX-PRO-6000', CATALOG);
  assert.deepEqual(noProbe.available, []);
  assert.deepEqual(noProbe.unavailable.map((u) => u.id), ['fp8', 'awq', 'bf16', 'none']);
});

test('a quant the repository already ships is not offered as a requantisation job', () => {
  const options = quantOptions(publicProbe(), 'RTX-PRO-6000', CATALOG);
  assert.deepEqual(options.available.map((entry) => entry.id), ['fp8'],
    'the shipped quant is the one the deployment can serve');
  const jobs = options.unavailable.filter((entry) => entry.requantize);
  assert.deepEqual(jobs.map((entry) => entry.id), ['awq', 'bf16', 'none'],
    'only artefacts the repository lacks are future GPU jobs');
  assert.ok(!jobs.some((entry) => entry.id === 'fp8'),
    'requantising to a quant already present would be a job that produces nothing');
});

test('an unavailable quant carries the requantisation recipe with the recorded §8 figures', () => {
  const req = quantOptions(publicProbe(), 'RTX-PRO-6000', CATALOG).unavailable.find((u) => u.id === 'awq').requantize;
  assert.equal(req.requested, 'awq');
  assert.equal(req.unavailable_reason, 'the repository ships fp8; no awq sibling and no awq tensors were found');
  assert.equal(req.path, 'dense');
  assert.equal(req.requires_vram_bytes, 12_884_901_888);
  assert.equal(req.requires_extra_vram_bytes, 0);
  assert.equal(req.requires_host_ram_bytes, 69_000_000_000);
  assert.equal(req.requires_disk_bytes, 100_000_000_000);
  assert.equal(req.estimated_minutes, 480);
  assert.deepEqual(req.estimated_minutes_range, [450, 515]);
  assert.equal(req.k_gpu, 4);
  assert.equal(req.tool, 'llm-compressor 0.15.0');
  assert.equal(req.serve_command, 'vllm serve <output-dir> --max-model-len 262144 --kv-cache-dtype fp8');
});

test('the recipe fits on both measured cards and never claims the card is too small', () => {
  for (const gpu of ['L40S', 'RTX-PRO-6000', 'A10', 'T4']) {
    const req = quantOptions(publicProbe(), gpu, CATALOG).unavailable.find((u) => u.id === 'awq').requantize;
    assert.equal(req.feasible_on_gpu, true, gpu);
    assert.equal(req.failed_on, null, gpu);
    assert.ok(!/does not fit/i.test(req.note), `${gpu}: ${req.note}`);
    assert.ok(!req.warnings.some((w) => /does not fit/i.test(w)), gpu);
  }
});

test('the requested id is stated to be a label, not the artefact’s own quant_method', () => {
  const target = quantOptions(publicProbe(), 'RTX-PRO-6000', CATALOG).unavailable.find((u) => u.id === 'awq').requantize.target;
  assert.equal(target.id, 'awq');
  assert.equal(target.produced_quant_method, 'compressed-tensors');
  assert.equal(target.requested_not_produced, true);
  assert.match(target.produces, /pack-quantized/);
  assert.match(target.produced_quant_method_note, /not an AutoAWQ-format repository/);
});

test('the base model is the recommended source and the gated FP8 checkout the fallback', () => {
  const req = quantOptions(publicProbe(), 'RTX-PRO-6000', CATALOG).unavailable.find((u) => u.id === 'awq').requantize;
  assert.equal(req.source.id, 'bf16');
  assert.equal(req.source.gated, false);
  assert.equal(req.source.recommended, true);
  assert.match(req.source.reason, /ungated/);
  assert.equal(req.fallback_source.id, 'fp8');
  assert.match(req.fallback_source.reason, /half the download/);
  assert.match(req.fallback_source.reason, /lossy FP8 round-trip/);
});

test('both paths are labelled by quality, never by speed alone', () => {
  const req = quantOptions(publicProbe(), 'RTX-PRO-6000', CATALOG).unavailable.find((u) => u.id === 'awq').requantize;
  const [dense, modelFree] = req.paths;
  assert.equal(dense.path, 'dense');
  assert.equal(dense.label, 'Higher quality · ~8 h');
  assert.equal(modelFree.path, 'model-free');
  assert.equal(modelFree.label, 'Faster · lower quality');
  assert.equal(dense.verified_for_this_architecture, true);
  assert.equal(modelFree.verified_for_this_architecture, false);
  assert.match(modelFree.produces, /RTN/);
  assert.equal(modelFree.estimated_minutes, 32);
  assert.deepEqual(modelFree.estimated_minutes_range, [20, 45]);
  assert.ok(dense.steps.length >= 5);
  assert.ok(modelFree.steps.length >= 4);
});

test('the recipe says plainly that this repo does not run it', () => {
  const req = quantOptions(publicProbe(), 'RTX-PRO-6000', CATALOG).unavailable.find((u) => u.id === 'awq').requantize;
  assert.match(req.note, /does not run it/);
  assert.match(req.note, /does not claim to have run it/);
  assert.ok(req.assumptions.length >= 5);
  assert.ok(req.warnings.length >= 3);
});

test('the recipe’s per-card hours are priced from the catalog, and null when it has no price', () => {
  const req = quantOptions(publicProbe(), 'RTX-PRO-6000', CATALOG).unavailable.find((u) => u.id === 'awq').requantize;
  const row = req.gpu_hours_at_k.find((r) => r.gpu === 'RTX-PRO-6000');
  assert.equal(row.hours, 8);
  assert.equal(row.cost_usd, 24.2);
  assert.equal(row.price_usd_per_second, 0.000842);
  for (const r of req.gpu_hours_at_k) assert.equal(r.hours, 8);

  const noPrices = makeCatalog(GPUS.map(({ price_usd_per_second, ...rest }) => rest));
  const unpriced = quantOptions(publicProbe(), 'RTX-PRO-6000', noPrices).unavailable.find((u) => u.id === 'awq').requantize;
  assert.equal(unpriced.gpu_hours_at_k[0].cost_usd, null);
  assert.equal(unpriced.gpu_hours_at_k[0].price_usd_per_second, null);
});

test('a job that runs out of disk or host RAM names the resource that ran out', () => {
  const failing = (overrides) => quantOptions({ ...publicProbe(), ...overrides }, 'RTX-PRO-6000', CATALOG)
    .unavailable.find((u) => u.id === 'awq').requantize;
  assert.equal(failing({ provisioned_disk_bytes: 8e9 }).failed_on, 'disk');
  assert.equal(failing({ host_ram_bytes: 8e9 }).failed_on, 'host_ram');
  assert.equal(failing({ provisioned_disk_bytes: 500e9, host_ram_bytes: 200e9 }).failed_on, null);
  // The predicate is strict: exactly the required disk is enough.
  assert.equal(failing({ provisioned_disk_bytes: 100e9 }).failed_on, null);
  const short = failing({ provisioned_disk_bytes: 8e9 });
  assert.equal(short.feasible_on_gpu, false);
});

// ---------------------------------------------------------------------------
// applyProbe
// ---------------------------------------------------------------------------

test('applyProbe keeps the draft’s own keys and copies the probe’s repo and revision', () => {
  const d = draft({ repo: '', revision: '', gpu: 'L40S', gpuCount: 2, baseUrl: 'https://keep--me.modal.run' });
  const next = applyProbe(d, publicProbe(), CATALOG);
  assert.deepEqual(Object.keys(next), Object.keys(emptyDraft('modal')));
  assert.equal(next.repo, 'Qwen/Qwen3.8-27B');
  assert.equal(next.revision, 'b32584cc');
  assert.equal(next.gpu, 'L40S');
  assert.equal(next.gpuCount, 2);
  assert.equal(next.baseUrl, 'https://keep--me.modal.run');
  assert.equal(next.weightQuant, 'fp8');
  assert.equal(next.maxModelLen, 262144);
});

test('applyProbe records a disambiguated probe and flags a gated repo', () => {
  const open = applyProbe(emptyDraft('modal'), publicProbe(), CATALOG);
  assert.equal(open.hfTokenNeeded, false);
  assert.deepEqual(Object.keys(open.probe), ['repo', 'revision', 'gated', 'arch', 'params', 'quants', 'sizing']);
  assert.equal(open.probe.sizing.kv_pool_tokens, 1_777_758);
  assert.equal(applyProbe(emptyDraft('modal'), gatedProbe(), CATALOG).hfTokenNeeded, true);
});

test('applyProbe with no probe leaves the draft alone', () => {
  const d = draft();
  const next = applyProbe(d, null, CATALOG);
  assert.equal(next.repo, d.repo);
  assert.equal(next.weightQuant, d.weightQuant);
});

// ---------------------------------------------------------------------------
// applySizing — the canonical figures from §7
// ---------------------------------------------------------------------------

test('the canonical sizing reproduces §7 exactly, and says it is an analytic estimate', () => {
  const { draft: next, sizing, warnings } = applySizing(draft(), publicProbe(), CATALOG);
  assert.equal(sizing.gpu, 'RTX-PRO-6000');
  assert.equal(sizing.gpu_count, 1);
  assert.equal(sizing.vram_bytes, CANONICAL.vram_bytes);
  assert.equal(sizing.usable_vram_bytes, CANONICAL.usable_vram_bytes);
  assert.equal(sizing.gpu_memory_utilization, 0.92);
  assert.equal(sizing.block_size, 64);
  assert.equal(sizing.kv_bytes_per_token, CANONICAL.kv_bytes_per_token);
  assert.equal(sizing.kv_budget_bytes, CANONICAL.kv_budget_bytes);
  assert.equal(sizing.kv_pool_tokens, CANONICAL.kv_pool_tokens);
  assert.equal(sizing.weights_bytes, PARAMS_TOTAL);
  assert.equal(sizing.overhead_bytes, OVERHEAD_BYTES);
  assert.equal(sizing.method, 'analytic');
  assert.equal(sizing.confidence, 'analytic');
  assert.deepEqual(sizing.missing, []);
  assert.equal(sizing.reason, null);
  assert.equal(sizing.block_size, BLOCK_SIZE);
  assert.equal(next.sizing.kv_pool_tokens, CANONICAL.kv_pool_tokens);
  assert.deepEqual(warnings, sizing.warnings);
});

test('the KV per-token figure counts only the full-attention layers, and says so', () => {
  const { sizing } = applySizing(draft(), publicProbe(), CATALOG);
  assert.equal(sizing.num_declared_layers, 64);
  assert.equal(sizing.num_hidden_layers, 16);
  assert.equal(sizing.num_full_attention_layers, 16);
  assert.equal(sizing.num_key_value_heads, 4);
  assert.equal(sizing.head_dim, 256);
  assert.equal(sizing.kv_dtype_bytes, 1);
  assert.equal(sizing.layer_count_source, 'layer_types: 16 full_attention of 64');
  assert.equal(sizing.arch_source, 'huggingface config.json');
  // 16 x 4 x 256 x 1 x 2 = 32768; counting all 64 layers would give 131072, 4x too high.
  assert.equal(sizing.kv_bytes_per_token_basis,
    'full_attention_layers=16 of 64 (hybrid linear attention; the other 48 hold a fixed-size recurrent state and no per-token KV) x num_kv_heads=4 x head_dim=256 x dtype_bytes=1; x2 for K and V');
  assert.equal(sizing.formula, 'kv_bytes_per_token = 2 * full_attention_layers * num_key_value_heads * head_dim * kv_dtype_bytes');
  assert.ok(sizing.assumptions.some((a) => /Counting all 64 layers would overestimate the KV cache 4x/.test(a)));
});

test('the session ladder is the four documented contexts', () => {
  const { sizing } = applySizing(draft(), publicProbe(), CATALOG);
  // The conductor's `SessionsAtContext`, byte for byte: the same object is POSTed back to
  // `/api/gpu/apply`, which re-parses it and rejects a display-only `{context, sessions}` row.
  assert.deepEqual(sizing.simultaneous_sessions_at, [
    { context_len: 2048, blocks_per_session: 32, bytes_per_session: 67108864, sessions: 871 },
    { context_len: 8192, blocks_per_session: 128, bytes_per_session: 268435456, sessions: 217 },
    { context_len: 32768, blocks_per_session: 512, bytes_per_session: 1073741824, sessions: 54 },
    { context_len: 262144, blocks_per_session: 4096, bytes_per_session: 8589934592, sessions: 6 },
  ]);
  assert.equal(sizing.max_num_seqs, 54);
  assert.equal(sizing.max_num_seqs_effective, 54);
  assert.match(sizing.max_num_seqs_effective_basis, /a session at the full 262144 tokens would fit 6 time\(s\)/);
});

test('the sizing carries the figures the conductor derives, so the rentals fit is not null', () => {
  const { draft: next, sizing } = applySizing(draft(), publicProbe(), CATALOG);
  // `fit_of` in gpu_api.rs reads `kv_pool_tokens_aligned` and `simultaneous_sessions_at_max_len`
  // straight off the stored sizing. When the panel forwarded a display subset, a deployment that
  // had sized perfectly reported `known: true, kv_pool_tokens: null` in `GET /api/gpu/rentals`.
  assert.equal(sizing.kv_pool_tokens, CANONICAL.kv_pool_tokens);
  assert.equal(sizing.kv_blocks, 27_876);
  assert.equal(sizing.kv_pool_tokens_aligned, 1_784_064);
  assert.equal(sizing.kv_bytes_per_session_at_max_len, 8_589_934_592);
  assert.equal(sizing.simultaneous_sessions_at_max_len, 6);
  // The aligned pool is the last whole block, never more than the pool.
  assert.ok(sizing.kv_pool_tokens_aligned <= sizing.kv_pool_tokens);

  // And they reach the wire object, because the whole sizing is forwarded.
  const wire = applyPayload(next, sizing).deployment.sizing;
  assert.equal(wire.kv_pool_tokens_aligned, 1_784_064);
  assert.equal(wire.simultaneous_sessions_at_max_len, 6);
  assert.equal(wire.kv_blocks, 27_876);
});

test('a config without layer_types falls back to every declared layer and warns that it overestimates', () => {
  const { layer_types, ...naiveArch } = ARCH;
  const { sizing } = applySizing(draft(), publicProbe({ arch: naiveArch }), CATALOG);
  assert.equal(sizing.num_full_attention_layers, null);
  assert.equal(sizing.num_hidden_layers, 64);
  assert.equal(sizing.layer_count_source, 'num_hidden_layers');
  assert.equal(sizing.kv_bytes_per_token, 131_072);
  assert.equal(sizing.kv_pool_tokens, 446_025);
  assert.match(sizing.warnings[0], /overestimates the pool \(4x on the model in this workstream\)/);
});

test('the measurement is reported as an accuracy bound, never as validation', () => {
  const { sizing } = applySizing(draft(), publicProbe(), CATALOG);
  assert.equal(sizing.measured_reference.kv_pool_tokens, 1_777_758);
  assert.equal(sizing.measured_reference.card, 'RTX-PRO-6000');
  assert.equal(sizing.measured_reference.source, 'observed');
  assert.equal(sizing.delta_vs_measurement_pct, 0.36);
  assert.equal(sizing.delta_at_usable_vram_pct, -1.34);
  const joined = sizing.warnings.join('\n');
  assert.match(joined, /NOT independent confirmation/);
  assert.match(joined, /accuracy bound of roughly ±2 %/);
  assert.match(joined, /not as validation/);
  assert.ok(!/0\.36 % accurate|validated against/i.test(joined));
});

test('both measured cards are reported as calibration rows with signed residuals', () => {
  const { sizing } = applySizing(draft(), publicProbe(), CATALOG);
  assert.deepEqual(sizing.calibration.map((c) => [c.card, c.delta_pct, c.usable_vram_gib]), [
    ['RTX-PRO-6000', -1.34, 95],
    ['L40S', -1.76, 46],
  ]);
  assert.equal(sizing.calibration[0].predicted_kv_pool_tokens, 1_753_957);
  assert.equal(sizing.calibration[1].predicted_kv_pool_tokens, 276_775);
  assert.equal(sizing.calibration[0].vram_basis, 'usable');
});

test('an unexplained measurement is rejected rather than swallowed', () => {
  const { sizing } = applySizing(draft(), publicProbe({ params: { total: 120_000_000_000 } }), CATALOG);
  assert.equal(sizing.architecture_consistent, false);
  assert.equal(sizing.kv_pool_tokens, 0);
  assert.match(sizing.warnings[0], /cannot host the checkpoint/);
  assert.match(sizing.warnings[1], /implies an overhead of -\d+ bytes/);
  assert.match(sizing.warnings[1], /negative/);
});

test('a card with no recorded measurement says so and reports no delta', () => {
  const { sizing } = applySizing(draft({ gpu: 'H100' }), publicProbe(), CATALOG);
  assert.equal(sizing.measured_reference, null, 'no H100 measurement exists to compare against');
  // The calibration rows are repo-level facts about the two cards that were measured; they are
  // still shown on an unmeasured card, because they are what the extract was checked against.
  assert.deepEqual(sizing.calibration.map((c) => c.card), ['RTX-PRO-6000', 'L40S']);
  assert.equal(sizing.delta_vs_measurement_pct, null);
  assert.equal(sizing.delta_at_usable_vram_pct, null);
  assert.equal(sizing.overhead_implied_bytes, null);
  assert.equal(sizing.architecture_consistent, null);
  assert.equal(sizing.kv_pool_tokens, 1_301_758);
  assert.deepEqual(sizing.warnings, [
    'No measurement of this model on this card is recorded, so there is no delta to check the estimate against. vLLM\'s own startup line reports the authoritative KV size.',
    'Only RTX-PRO-6000 and L40S have been measured for this model; every other card is extrapolated, and its VRAM is a datasheet figure rather than the usable capacity the operator measured.',
  ]);
});

test('a measured card with no usable-capacity figure yields no Infinity', () => {
  const catalog = makeCatalog(GPUS.map(({ usable_vram_gib, ...rest }) => rest));
  const { sizing } = applySizing(draft(), publicProbe(), catalog);
  assert.equal(sizing.delta_vs_measurement_pct, 0.36);
  assert.equal(sizing.delta_at_usable_vram_pct, null, 'a missing basis must be null, not Infinity');
  const notes = stepList({ ...draft(), sizing }, catalog)
    .find((s) => s.id === 'sizing').fields.find((f) => f.name === 'sizingMeasured').notes.join('\n');
  assert.ok(!/Infinity|NaN/.test(notes), notes);
});

test('a zero-token measurement row yields no ratio at all', () => {
  const catalog = makeCatalog(GPUS);
  catalog.measured_references[0].kv_pool_tokens = 0;
  const { sizing } = applySizing(draft(), publicProbe(), catalog);
  assert.equal(sizing.delta_vs_measurement_pct, null);
  assert.equal(sizing.delta_at_usable_vram_pct, null);
});

test('a KV dtype with no width cannot be sized, and the gap is named', () => {
  const { sizing } = applySizing(
    draft({ kvCacheDtype: 'auto' }),
    publicProbe({ arch: { ...ARCH, torch_dtype: undefined } }),
    CATALOG,
  );
  assert.deepEqual(sizing.missing, ['a KV element width for --kv-cache-dtype "auto"']);
  assert.notEqual(sizing.method, 'analytic');
});

test('no probe and no catalog yields an unavailable sizing that names every missing input', () => {
  const result = applySizing(draft({ probe: null }), null, CATALOG);
  assert.deepEqual(Object.keys(result), ['draft', 'sizing', 'warnings']);
  assert.equal(result.sizing.method, 'unavailable');
  assert.deepEqual(result.sizing.missing, [
    'num_hidden_layers (or layer_types) from the probed config.json',
    'num_key_value_heads',
    'head_dim',
    'the probe carries no parameter count (probe.params.total)',
  ]);
  assert.equal(result.sizing.kv_pool_tokens, null);
  assert.equal(result.warnings.length, 1);
  assert.match(result.warnings[0], /The KV pool is not estimated because/);
});

test('a card too small for the weights reports a zero pool rather than a negative one', () => {
  const { sizing } = applySizing(draft({ gpu: 'T4' }), publicProbe(), CATALOG);
  assert.equal(sizing.kv_pool_tokens, 0);
  assert.match(sizing.warnings[0], /exceed 15805479649 bytes of budget on T4/);
  assert.equal(sizing.delta_vs_measurement_pct, null);
});

test('the sizing readouts render the assumptions and warnings rather than hiding them', () => {
  const d = draft();
  const fields = Object.fromEntries(
    stepList(d, CATALOG).find((s) => s.id === 'sizing').fields.map((f) => [f.name, f]),
  );
  assert.equal(fields.sizingKvBasis.value, 32_768);
  assert.equal(fields.sizingKvPool.value, 1_784_103);
  assert.equal(fields.sizingMeasured.value, 1_777_758);
  assert.equal(fields.sizingAssumptions.value, d.sizing.assumptions.length);
  assert.equal(fields.sizingWarnings.value, d.sizing.warnings.length);
  assert.deepEqual(fields.sizingAssumptions.notes, d.sizing.assumptions);
  assert.deepEqual(fields.sizingWarnings.notes, d.sizing.warnings);
  assert.equal(fields.sizingUnavailable.visible, false);
  assert.ok(fields.sizingMeasured.notes.some((n) => /±2 %/.test(n)));
  assert.ok(fields.sizingKvBasis.notes.some((n) => /full_attention_layers=16 of 64/.test(n)));
});

// ---------------------------------------------------------------------------
// deploymentBundle
// ---------------------------------------------------------------------------

test('the bundle is a complete runnable modal_app.py plus the command that deploys it', () => {
  const d = draft();
  const bundle = deploymentBundle(d, d.sizing, CATALOG);
  assert.deepEqual(Object.keys(bundle).sort(), ['command', 'filename', 'notes', 'text', 'warnings']);
  assert.equal(bundle.filename, 'modal_app.py');
  assert.equal(bundle.command, 'modal deploy modal_app.py');
  assert.match(bundle.text, /^"""modal_app\.py/);
  assert.match(bundle.text, /import modal/);
  assert.match(bundle.text, /app = modal\.App\("manarouter-rental-mana-modal-qwen3-8-27b-streaming-rtx-pro-6000"\)/,
    'the app name is the one the derived endpoint URL is built from');
  assert.match(bundle.text, /gpu="RTX-PRO-6000"/);
  assert.match(bundle.text, /scaledown_window=60/);
  assert.match(bundle.text, /@modal\.web_server\(port=8000/);
  // The server is spawned and serve() returns; blocking on the child stops Modal's proxy
  // from ever forwarding a request into an engine that is already listening.
  assert.match(bundle.text, /subprocess\.Popen\(\["vllm", "serve", \*VLLM_ARGS\]\)/);
  assert.doesNotMatch(bundle.text, /process\.wait\(\)/);
});

test('the bundle never emits a credential or a Secret literal', () => {
  const d = draft();
  const bundle = deploymentBundle(d, d.sizing, CATALOG);
  assert.ok(!/wk-x|ws-y/.test(bundle.text));
  assert.ok(!/hf_[A-Za-z0-9]{8,}/.test(bundle.text));
  assert.ok(!/modal\.Secret\(/.test(bundle.text));
  assert.ok(!/HF_TOKEN=/.test(bundle.text));
});

test('the bundle text is stable for the same draft', () => {
  const d = draft();
  assert.equal(deploymentBundle(d, d.sizing, CATALOG).text, deploymentBundle(d, d.sizing, CATALOG).text);
});

test('a gated repo wires the named HF secret and says where it is created', () => {
  const gated = draft({ probe: gatedProbe(), repo: 'orcarouter/Qwen3.8-27B-Uncensored-FP8' });
  const sizing = applySizing(gated, gated.probe, CATALOG).sizing;
  const bundle = deploymentBundle({ ...gated, sizing }, sizing, CATALOG);
  assert.match(bundle.text, /modal\.Secret\.from_name\("manarouter-hf"\)/);
  assert.ok(bundle.notes.some((n) => /modal secret create manarouter-hf/.test(n)));
  assert.ok(!/hf_[A-Za-z0-9]{8,}/.test(bundle.text));
});

test('a public repo needs no secret at all', () => {
  const d = draft();
  const bundle = deploymentBundle(d, d.sizing, CATALOG);
  assert.ok(!/Secret\.from_name/.test(bundle.text));
});

test('the bundle never emits a null, undefined or NaN into Python', () => {
  const d = draft({ maxModelLen: null, maxNumSeqs: null, revision: '' });
  const sizing = applySizing(d, d.probe, CATALOG).sizing;
  const bundle = deploymentBundle({ ...d, sizing }, sizing, CATALOG);
  assert.ok(!/\bnull\b|\bundefined\b|\bNaN\b/.test(bundle.text));
});

test('the bundle never emits --quantization, for any quant the draft can select', () => {
  for (const weightQuant of ['none', 'bf16', 'fp8', 'awq', 'gptq', 'compressed-tensors']) {
    const d = draft({ weightQuant });
    const sizing = applySizing(d, d.probe, CATALOG).sizing;
    const bundle = deploymentBundle({ ...d, sizing }, sizing, CATALOG);
    assert.ok(!/--quantization/.test(bundle.text), `${weightQuant} must not trigger --quantization`);
  }
});

test('when the draft’s quant is not what the repo ships, a note names the recipe to run', () => {
  const d = draft({ weightQuant: 'awq' });
  const bundle = deploymentBundle(d, d.sizing, CATALOG);
  const note = bundle.notes.join('\n');
  assert.match(note, /not what this repo ships/);
  assert.match(note, /it ships fp8/);
  assert.match(note, /requantisation recipe/);
  assert.match(bundle.warnings.join('\n'), /does not ship "awq"/);
});

test('a quant whose artefact disagrees with its label is surfaced as a warning', () => {
  // The catalog records that the "awq" recipe's artefact is written as quant_method
  // "compressed-tensors" (requantize-recipes.md §6.2). Serving it with --quantization awq would
  // make vLLM raise ValueError, so the disagreement has to be a warning, not a footnote.
  const d = draft({ weightQuant: 'awq' });
  const bundle = deploymentBundle(d, d.sizing, CATALOG);
  const warnings = bundle.warnings.join('\n');
  assert.match(warnings, /"awq" is produced as quant_method "compressed-tensors"/);
  assert.match(warnings, /raises ValueError at startup/);
  assert.match(warnings, /emits no --quantization flag/);
});

test('the disagreement warning survives a catalog that records no produced method', () => {
  // §6 documents only min_compute_cap and bytes_per_param on the catalog's quant table; the
  // produced method lives on `requantize.target` (§8). A catalog without it must still warn.
  const catalog = makeCatalog(GPUS.map((gpu) => ({ ...gpu })));
  catalog.quant_methods = catalog.quant_methods.map(({ produced_quant_method, ...rest }) => rest);
  const d = draft({ weightQuant: 'awq' }, catalog);
  const bundle = deploymentBundle(d, d.sizing, catalog);
  assert.match(bundle.warnings.join('\n'), /is produced as quant_method "compressed-tensors"/);
});

test('a native dtype the repo does ship is never accused of a label mismatch', () => {
  for (const weightQuant of ['fp8', 'none']) {
    const d = draft({ weightQuant });
    assert.deepEqual(deploymentBundle(d, d.sizing, CATALOG).warnings, [], weightQuant);
  }
});

test('an ungated click-through of the canonical draft warns about nothing', () => {
  const d = draft();
  assert.deepEqual(deploymentBundle(d, d.sizing, CATALOG).warnings, []);
});

test('the notes state that Modal has no deployment REST API and that vLLM’s line wins', () => {
  const d = draft();
  const bundle = deploymentBundle(d, d.sizing, CATALOG);
  const note = bundle.notes.join('\n');
  assert.match(note, /no deployment REST API/);
  assert.match(note, /GPU KV cache size: N tokens/);
  assert.match(note, /1784103 tokens/);
  assert.ok(!/validated/.test(note));
  assert.match(note, /per second/i);
  assert.match(note, /There is no --detach/);
  assert.match(note, /--strategy \[rolling\|recreate\]/);
  assert.ok(!/--detach/.test(bundle.text));
  assert.ok(!/--strategy/.test(bundle.text));
});

test('the docstring carries the model, the card and the sizing caveat', () => {
  const d = draft();
  const text = deploymentBundle(d, d.sizing, CATALOG).text;
  assert.match(text, /Model:   Qwen\/Qwen3\.8-27B/);
  assert.match(text, /Card:    RTX-PRO-6000/);
  assert.match(text, /Sizing:  Manarouter estimate 1784103 KV tokens — an estimate, not vLLM's measurement/);
  assert.match(text, /Run it with:  modal deploy modal_app\.py/);
  assert.equal(text.match(/"""/g).length, 2, 'the module docstring must be the only triple-quoted block');
});

test('the bundle clamps scaledown_window into Modal’s range and explains the clamp', () => {
  const bounded = (secs) => {
    const d = draft({ shutdownCooldownSecs: secs });
    return deploymentBundle(d, d.sizing, CATALOG);
  };
  assert.match(bounded(1).text, /scaledown_window=2,/);
  assert.match(bounded(1).notes.join('\n'), /1 s was outside/);
  assert.match(bounded(5000).text, /scaledown_window=1200,/);
  assert.match(bounded(5000).notes.join('\n'), /5000 s was outside/);
  assert.match(bounded(900).text, /scaledown_window=900,/);
});

test('multiple cards emit a counted GPU string and tensor parallelism', () => {
  const d = draft({ gpu: 'H100', gpuCount: 8 });
  const sizing = applySizing(d, d.probe, CATALOG).sizing;
  const bundle = deploymentBundle({ ...d, sizing }, sizing, CATALOG);
  assert.match(bundle.text, /gpu="H100!:8"/);
  assert.match(bundle.text, /--tensor-parallel-size/);
  assert.match(bundle.notes.join('\n'), /H200/);
});

test('A10 is clamped to 4 cards, and the note says the clamp is the published exception', () => {
  const d = draft({ gpu: 'A10', gpuCount: 8 });
  const sizing = applySizing(d, d.probe, CATALOG).sizing;
  const bundle = deploymentBundle({ ...d, sizing }, sizing, CATALOG);
  assert.match(bundle.text, /gpu="A10:4"/);
  const note = bundle.notes.join('\n');
  assert.match(note, /at most 4 of these cards/);
  assert.match(note, /you asked for 8/);
  assert.match(note, /A10 is the published exception at 4/);
});

test('B300 is flagged as needing CUDA 13.1 and no CUDA-12 image is emitted', () => {
  const d = draft({ gpu: 'B300', gpuCount: 1 });
  const sizing = applySizing(d, d.probe, CATALOG).sizing;
  const bundle = deploymentBundle({ ...d, sizing }, sizing, CATALOG);
  assert.match(bundle.text, /WARNING: B300 needs CUDA 13\.1\+/);
  assert.match(bundle.text, /a CUDA-12 image cannot drive this card/);
  assert.ok(!/cuda12[0-9]/i.test(bundle.text));
  assert.ok(!/nvidia\/cuda/i.test(bundle.text));
  assert.match(bundle.notes.join('\n'), /fails at startup/);
});

test('a legacy A10G id is kept, and the note names the canonical id it should be', () => {
  const d = draft({ gpu: 'A10G' });
  const sizing = applySizing(d, d.probe, CATALOG).sizing;
  const bundle = deploymentBundle({ ...d, sizing }, sizing, CATALOG);
  assert.match(bundle.text, /gpu="A10G"/);
  const note = bundle.notes.join('\n');
  assert.match(note, /is not a current Modal GPU id/);
  assert.match(note, /"A10"/);
});

test('an unknown card is named in the notes instead of being silently deployed', () => {
  const d = draft({ gpu: 'V100' });
  const sizing = applySizing(d, d.probe, CATALOG).sizing;
  const bundle = deploymentBundle({ ...d, sizing }, sizing, CATALOG);
  assert.match(bundle.notes.join('\n'), /"V100" is not in the catalog/);
});

test('the bundle says the endpoint is derived, and the printed one wins if it differs', () => {
  const derived = deploymentBundle(draft(), draft().sizing, CATALOG);
  assert.match(derived.notes.join('\n'),
    /Manarouter computes the endpoint URL as https:\/\/mana--manarouter-rental-mana-modal-qwen3-8-27b-streaming-rtx-pro-6000-serve\.modal\.run/);
  assert.match(derived.notes.join('\n'), /`modal deploy` prints the authoritative URL; if it differs, override/);
  const overridden = deploymentBundle(draft({ baseUrl: 'https://printed--by-modal.modal.run' }), draft().sizing, CATALOG);
  assert.match(overridden.notes.join('\n'), /overridden to https:\/\/printed--by-modal\.modal\.run/);
});

// ---------------------------------------------------------------------------
// applyPayload / accountApplyPayload
// ---------------------------------------------------------------------------

test('the deployment payload carries no credential, only the sizing estimate', () => {
  const d = draft();
  const payload = applyPayload(d, d.sizing);
  assert.equal(payload.action, 'create-deployment');
  assert.equal(payload.account_id, 'mana-modal');
  assert.deepEqual(
    { ...payload.deployment, sizing: undefined },
    {
      repo: 'Qwen/Qwen3.8-27B',
      gpu: 'RTX-PRO-6000',
      gpu_count: 1,
      weight_quant: 'fp8',
      kv_cache_dtype: 'fp8',
      shutdown_cooldown_secs: 60,
      mode: 'streaming',
      revision: 'b32584cc',
      max_model_len: 262144,
      max_num_seqs: 54,
      sizing: undefined,
    },
  );
  assert.equal(payload.deployment.sizing.kv_pool_tokens, 1_784_103);
  const text = JSON.stringify(payload);
  assert.ok(!/wk-x|ws-y/.test(text), 'proxy credentials must not travel in the deployment payload');
  assert.ok(!/token_id|token_secret|hf_token/.test(text));
});

test('no payload carries a second card, even from a draft written before the mode axis existed', () => {
  const d = draft({ batchGpu: 'h100' });
  assert.ok(!('batchGpu' in normalizeDraft(d)), 'the legacy field is dropped when a draft is read');
  const payload = applyPayload(d, d.sizing);
  assert.ok(!('batch_gpu' in payload.deployment),
    'a rental is one model on one card; the batch lane is a mode, not another GPU');
  assert.ok(!JSON.stringify(payload).includes('batch_gpu'));
});

test('the account payload writes the platform credential and no HuggingFace token', () => {
  assert.deepEqual(accountApplyPayload(draft()), {
    action: 'create-account',
    account: {
      id: 'mana-modal',
      label: 'mana-modal',
      platform: 'modal',
      plan: 'paid',
      workspace: 'mana',
      token_id: 'wk-x',
      token_secret: 'ws-y',
    },
  });
  assert.equal('hf_token' in accountApplyPayload(draft({ hfTokenId: 'hub-main' })).account, false,
    'a token is its own record; the account never carries one');
  // The optional Modal API token pair is a second vault entry, posted only when both halves exist.
  assert.equal('api_token_id' in accountApplyPayload(draft()).account, false,
    'no API pair means no API fields on the wire');
  assert.deepEqual(accountApplyPayload(draft({ apiTokenId: 'ak-x', apiTokenSecret: 'as-y' })).account, {
    id: 'mana-modal',
    label: 'mana-modal',
    platform: 'modal',
    plan: 'paid',
    workspace: 'mana',
    token_id: 'wk-x',
    token_secret: 'ws-y',
    api_token_id: 'ak-x',
    api_token_secret: 'as-y',
  });
});

test('reusing a recorded account posts nothing at all', () => {
  assert.equal(accountApplyPayload(draft({ accountMode: 'existing', accountId: 'mana-modal' })), null);
  assert.equal(accountApplyPayload(draft({ accountMode: 'new' })).action, 'create-account');
});

test('a pasted HuggingFace token is posted as its own record, once, under the id a rental names', () => {
  assert.equal(hfTokenApplyPayload(draft()), null, 'no pasted token means no write');
  assert.deepEqual(hfTokenApplyPayload(draft({ hfTokenNew: 'hf_placeholder', hfTokenNewLabel: 'hub-new' })), {
    action: 'create-hf-token',
    token: { id: 'hub-new', label: 'hub-new', secret: 'hf_placeholder' },
  });
});

test('the deployment names a token by id beside its account, or names none', () => {
  const withToken = applyPayload(draft({ hfTokenId: 'hub-main' }), draft().sizing);
  assert.equal(withToken.hf_token, 'hub-main');
  assert.equal(withToken.account_id, 'mana-modal');
  const publicRepo = applyPayload(draft({ hfTokenId: '' }), draft().sizing);
  assert.equal('hf_token' in publicRepo, false, 'a public repository names no token at all');
});

// ---------------------------------------------------------------------------
// one account per rental
// ---------------------------------------------------------------------------

test('creating an account whose id is already recorded is refused, and says to reuse it', () => {
  const d = draft();
  const accounts = [{ id: 'mana-modal', platform: 'modal', workspace: 'mana', has_credential: true }];
  const result = validateStep(d, 'account', CATALOG, accounts);
  assert.equal(result.ok, false);
  assert.deepEqual(result.problems, [{
    field: 'accountId',
    message: 'Account "mana-modal" is already recorded. Reuse it instead of creating a second one '
      + 'with the same id, which would collide on its vault entry.',
  }]);
  assert.deepEqual(validateStep(draft({ accountId: 'mana-modal-2' }), 'account', CATALOG, accounts).problems, []);
  assert.deepEqual(validateStep(d, 'account', CATALOG, []).problems, [],
    'with no recorded accounts there is nothing to collide with');
});

test('reusing an account names one that is recorded, and asks for no credential', () => {
  const accounts = [{ id: 'mana-modal', platform: 'modal', workspace: 'mana', has_credential: true }];
  const reuse = draft({ accountMode: 'existing', accountId: 'mana-modal', workspace: '', tokenId: '', tokenSecret: '' });
  assert.deepEqual(validateStep(reuse, 'account', CATALOG, accounts).problems, [],
    'a reused account writes nothing, so it needs no workspace or credential');
  assert.deepEqual(validateStep({ ...reuse, accountId: '' }, 'account', CATALOG, accounts).problems, [{
    field: 'accountId',
    message: 'Choose the recorded account this rental bills through.',
  }]);
  assert.deepEqual(validateStep({ ...reuse, accountId: 'ghost' }, 'account', CATALOG, accounts).problems, [{
    field: 'accountId',
    message: '"ghost" is not a recorded account on this platform, so there is nothing to reuse.',
  }]);
});

test('the account step always writes its own credential, so both halves are required', () => {
  const d = emptyDraft('modal');
  const problems = validateStep({ ...d, accountId: 'fresh' }, 'account', {}).problems.map((p) => p.field);
  assert.deepEqual(problems, ['workspace', 'tokenId', 'tokenSecret'],
    'the create path writes the credential and the workspace, so all three are required');
});

test('the account payload carries the Modal workspace as a non-secret field', () => {
  const payload = accountApplyPayload(draft({ workspace: 'mana-team' }));
  assert.equal(payload.account.workspace, 'mana-team');
  assert.equal(payload.account.label, 'mana-modal',
    'a flow that never asked for a label records the id, which is what the list has always shown');
  assert.equal(accountApplyPayload(draft({ accountLabel: 'Modal — mana' })).account.label,
    'Modal — mana', 'a label the operator typed is the one recorded, not the id');
  assert.equal(payload.account.token_secret, 'ws-y');
  assert.ok(!JSON.stringify(payload.account.workspace).includes('ws-y'),
    'the workspace is not the secret and must not be confused with one');
});

// ---------------------------------------------------------------------------
// fitReport — what fits, and what to change when it does not
// ---------------------------------------------------------------------------

/**
 * The sizing figures this catalog and probe produce on one RTX PRO 6000: a 1 784 103-token pool,
 * which is six slots at the full 262 144-token context. Every test below is written against that
 * one number so the boundary is exercised from both sides rather than guessed at.
 */
function fitted(overrides = {}) {
  const d = draft(overrides);
  return { draft: d, sizing: d.sizing, catalog: CATALOG };
}

test('streaming is judged on the sessions it must hold, not on one request at a time', () => {
  const { draft: d, sizing } = fitted();
  assert.equal(sizing.simultaneous_sessions_at_max_len, 6, 'the pool holds six full-context slots');
  assert.equal(fitReport({ ...d, sessions: 1 }, sizing, CATALOG).ok, true);
  assert.equal(fitReport({ ...d, sessions: 6 }, sizing, CATALOG).ok, true,
    'a streaming endpoint that must hold six sessions fits exactly at its limit');
  assert.equal(fitReport({ ...d, sessions: 7 }, sizing, CATALOG).ok, false,
    'one more session than the pool holds is a refusal, not a warning');
});

test('a batch is judged on its own size, so the same card refuses a batch it would stream', () => {
  const { draft: d, sizing } = fitted({ mode: 'batch', batchSize: 8 });
  const report = fitReport(d, sizing, CATALOG);
  assert.equal(report.ok, false);
  assert.equal(report.known, true, 'the pool is known; only the batch is too large');
  assert.deepEqual(report.blockers.map((b) => b.code), ['sessions']);
  assert.match(report.blockers[0].message, /^A batch of 8 at the full 262144 tokens needs 8 slots/);
  assert.equal(fitReport({ ...d, batchSize: 6 }, sizing, CATALOG).ok, true,
    'the same deployment with a batch of six fits what six sessions fit');
});

test('the fill policy never changes whether a deployment fits', () => {
  const { draft: d, sizing } = fitted({ mode: 'batch', batchSize: 7 });
  for (const fill of ['full', 'timeout']) {
    const report = fitReport({ ...d, batchFill: fill, batchTimeoutSecs: 30 }, sizing, CATALOG);
    assert.equal(report.ok, false, `${fill} does not make seven slots fit six`);
    assert.deepEqual(report.blockers.map((b) => b.code), ['sessions']);
    assert.deepEqual(report.fit, {
      gpu: 'RTX-PRO-6000',
      gpu_count: 1,
      pool: sizing.kv_pool_tokens,
      max_model_len: 262144,
      sessions_at_max_len: 6,
      needed: 7,
      mode: 'batch',
    }, 'how a batch fills is a latency choice; the fit verdict is about capacity alone');
  }
});

test('every suggestion names one concrete change, and only when that change actually fits', () => {
  const { draft: d, sizing } = fitted({ mode: 'batch', batchSize: 8 });
  const report = fitReport(d, sizing, CATALOG);
  assert.ok(report.suggestions.length > 0, 'a refusal must offer a way out');
  const names = report.suggestions.map((s) => s.field);
  assert.ok(!names.includes('requantizeTo'),
    're-quantising the weights is a second upload, not a way to make this deployment fit');
  for (const suggestion of report.suggestions) {
    assert.notEqual(suggestion.value, null, `${suggestion.field} names a value`);
    const changed = { ...d, [suggestion.field]: suggestion.value };
    assert.equal(fitReport(changed, applySizing(changed, changed.probe, CATALOG).sizing, CATALOG).ok, true,
      `${suggestion.label} must actually fit, not merely look smaller`);
  }
  // The order is the order of the cost each change imposes on the model, so the card comes first:
  // a bigger card keeps the batch, the context and the quant exactly as the operator asked for.
  assert.deepEqual(
    { field: report.suggestions[0].field, value: report.suggestions[0].value },
    { field: 'gpu', value: 'B300' },
    'a card that holds this draft unchanged is the first thing to offer',
  );
  // Dropping the batch to what the chosen card holds is still offered, further down: it keeps the
  // card, the context and the quant and gives up only the concurrency the card cannot serve.
  const concurrency = report.suggestions.find((s) => s.field === 'batchSize');
  assert.deepEqual({ field: concurrency?.field, value: concurrency?.value },
    { field: 'batchSize', value: 6 });
});

test('a refusal that cannot be fixed by dropping concurrency still names a change that fits', () => {
  // On a card the weights alone overflow there is no concurrency left to give up, so the pool-side
  // suggestions are the only way out. Reading the sizing envelope instead of its `sizing` made every
  // candidate look like a second refusal and left this case with no suggestion at all.
  const { draft: d } = fitted({ mode: 'streaming', sessions: 1 });
  const small = { ...d, gpu: 'T4', sizing: null };
  small.sizing = applySizing(small, small.probe, CATALOG).sizing;
  const report = fitReport(small, small.sizing, CATALOG);
  assert.equal(report.ok, false);
  assert.deepEqual(report.blockers.map((b) => b.code), ['weights'],
    'the weights alone do not fit a 16 GiB card');
  assert.ok(report.suggestions.length > 0, 'a weights refusal must still name a way out');
  assert.equal(report.suggestions[0].field, 'gpu');
  for (const suggestion of report.suggestions) {
    const changed = { ...small, [suggestion.field]: suggestion.value };
    assert.equal(fitReport(changed, applySizing(changed, changed.probe, CATALOG).sizing, CATALOG).ok, true,
      `${suggestion.label} must actually fit`);
  }
});

test('fit is reported as unknown, never as good, when the pool cannot be estimated', () => {
  const d = draft();
  const unavailable = { ...d.sizing, method: 'unavailable', kv_pool_tokens: null, kv_budget_bytes: null, missing: ['a catalog entry for GPU "RTX-PRO-6000"'] };
  const report = fitReport(d, unavailable, null);
  assert.equal(report.ok, false);
  assert.equal(report.known, false, '"I cannot tell" is not a pass');
  assert.deepEqual(report.blockers.map((b) => b.code), ['unknown']);
  assert.deepEqual(report.suggestions, [], 'no suggestion can be proven, so none is printed');
});
