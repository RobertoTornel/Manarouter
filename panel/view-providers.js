// view-providers.js — dense provider table with the selected provider's detail inline under its
// own row (no detached drawer).
//
// Table: pin/manual-move column, provider identity (name, id, dialect), account and model counts,
// health. Inline detail: accounts (add/enable/disable/delete), models (add manual / hide / delete /
// edit), timeouts, headers, route overrides, and the per-provider "sync models" action.
//
// NOTE ON SERVER STRINGS: provider ids, labels, model ids and error text come from the server
// and from third-party catalogues. Everything is attached with textContent (via h()); there is
// deliberately no innerHTML anywhere in this file.

import { api } from './api.js';
import {
  h, icon, chip, field, toggle, kv, emptyState, errorBlock, applyFieldErrors, clearFieldErrors,
  copyButton, copyText,
} from './dom.js';
import {
  deriveProviderHealth, accountState, formatAge, formatCompact, providerLevels, declaredLevels,
  parseTagList, tagListText,
} from './format.js';
import {
  placeholderNamesIn, NAME_PATTERN as PLACEHOLDER_NAME, VALUE_PATTERN as PLACEHOLDER_VALUE,
} from './placeholders.js';

// Input modalities an operator can declare per model. Empty means *derive* from the bundled
// capability table; an explicit list is how an operator corrects the seed.
const INPUT_MODALITIES = ['text', 'image', 'audio', 'video', 'pdf', 'file'];

// Server- and catalogue-supplied OAuth URLs are bound to an href and to window.open. Only http(s)
// is safe there: a javascript: or data: URL would execute in the panel's own origin. Anything that
// is not a well-formed http(s) URL is treated as unusable and is not offered as a link.
function httpUrl(value) {
  try {
    const url = new URL(String(value));
    return url.protocol === 'https:' || url.protocol === 'http:' ? url.href : null;
  } catch {
    return null;
  }
}

// --------------------------------------------------------------------------- OAuth redirect paste
//
// The manual fallback for the browser flow. The loopback capture can fail without telling anyone:
// the browser may be on another host, the port may be busy, or the tab may be closed after
// approval. The address bar still holds `?code=…&state=…`, so this block hands that URL to the
// waiting session. It is revealed the moment the tab opens rather than after a timeout — someone
// who already knows the capture failed should not sit through the ten-minute wait.
//
// `session.id` is a shared box the caller fills in once /authorize answers. The blocking /complete
// call stays in flight; this block only delivers the pasted URL and reports whether the server
// accepted it. It deliberately never calls /complete a second time: two completions would race for
// one callback.
export function buildRedirectPaste(p, ctx, session) {
  const input = h('input', {
    class: 'input mono',
    type: 'text',
    autocomplete: 'off',
    spellcheck: 'false',
    placeholder: 'http://127.0.0.1:1455/auth/callback?code=…&state=…',
  });
  // The loopback callback can only land on the machine the panel runs on. When the page is open
  // from anywhere else (a tailnet address, a phone), the redirect is *guaranteed* to fail, so say
  // so before it happens instead of letting the person think sign-in broke.
  const pageHost = globalThis.location?.hostname ?? 'localhost';
  const remote = !['localhost', '127.0.0.1', '[::1]', '::1'].includes(pageHost);
  const submit = h('button', { class: 'btn', type: 'button' }, 'Use this URL');
  const status = h('div', { class: 'muted' });
  const notice = h('p', { class: 'hint' },
    remote
      ? 'You are using the panel from another device, so the sign-in page will end on a URL this '
        + 'computer cannot open — that is expected. The address bar still holds the one-time code: '
        + 'copy the whole URL and paste it below.'
      : 'If the browser did not come back on its own, the address bar still holds the one-time code.');
  const block = h('div', { hidden: true },
    field('Paste the URL you were redirected to', input, {
      hint: 'The address-bar URL works, and so does just the code. It is sent exactly as pasted.',
    }),
    notice,
    h('div', { class: 'toolbar', style: 'padding:0;border:0;background:none;' }, submit),
    status);

  function show(redirectUri) {
    input.value = '';
    submit.disabled = false;
    status.replaceChildren();
    if (redirectUri) input.placeholder = `${redirectUri}?code=…&state=…`;
    block.hidden = false;
    // One action, not two: a browser that fails to reach the loopback leaves the whole URL on the
    // clipboard, and pasting it is the entire manual step.
    if (typeof input.focus === 'function') input.focus();
    input.addEventListener('paste', () => {
      setTimeout(() => {
        if (input.value.includes('code=') && !submit.disabled) submit.click();
      }, 0);
    }, { once: true });
  }

  function hide() {
    block.hidden = true;
    submit.disabled = false;
    // The pasted URL carries a one-time code; drop it from the DOM as soon as the flow is over.
    input.value = '';
    status.replaceChildren();
  }

  async function send() {
    if (submit.disabled) return;
    const url = input.value.trim();
    if (!url) {
      ctx.toast.warning('Paste the redirect URL, or just the code, first.');
      input.focus();
      return;
    }
    if (!session.id) {
      // The flow already ended: say so instead of dropping the paste on the floor.
      ctx.toast.warning('That sign-in already finished. Start it again to paste a redirect.');
      hide();
      return;
    }
    const sessionId = session.id;
    submit.disabled = true;
    status.replaceChildren(h('p', { class: 'muted' }, 'Checking the pasted URL…'));
    try {
      await api.post(`/api/oauth/sessions/${encodeURIComponent(sessionId)}/redirect`, {
        provider_id: p.id,
        url,
      });
      // /complete is already waiting on this session and resolves on its own once the server
      // accepts the URL; the existing flow then reloads the drawer and hides this block.
      status.replaceChildren(h('p', { class: 'muted' }, 'Checking… the sign-in should finish on its own.'));
    } catch (err) {
      // Server message in the form's own error style, plus the retryable input left enabled.
      // Wipe the pasted code/URL so it cannot outlive this failed attempt.
      input.value = '';
      status.replaceChildren(errorBlock(err));
      ctx.toast.error('Could not use the pasted URL', err);
      submit.disabled = false;
      input.focus();
    }
  }

  submit.addEventListener('click', () => { send(); });
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); send(); }
  });

  return { block, input, submit, send, show, hide };
}

// Must list exactly the spellings the backend *deserialises* (mr-core `dialect.rs`, serde
// kebab-case) — not the looser aliases `Dialect::parse` accepts. The two differ once: serde wants
// `open-ai`, `parse` wants `openai`. A value here that the backend rejects turns "Add provider"
// into a save-time error, which is how the pre-rename `command-code` spelling was caught. The Rust
// test `the_panel_offers_only_dialects_this_binary_deserialises` pins this list.
const DIALECTS = ['open-ai', 'anthropic', 'google', 'google-ai-studio', 'codex', 'copilot', 'antigravity', 'command-code-go', 'modal-vllm', 'cursor', 'zcode'];
// Must list exactly the capability spellings the backend *deserialises* (mr-core `dialect.rs`,
// serde kebab-case) and in the enum's order: `panel/test/capabilities.test.mjs` pins the ids to
// the Rust variants. A checkbox spelling the server refuses turns "Create provider" into a
// save-time error, and a capability the panel omits cannot be declared at all.
const CAPABILITIES = [
  { id: 'chat', label: 'Chat', group: 'Text', hint: 'Answers conversational requests on `POST /v1/chat/completions`.' },
  { id: 'responses', label: 'Responses', group: 'Text', hint: 'Serves the newer stateful `POST /v1/responses` route.' },
  { id: 'embeddings', label: 'Embeddings', group: 'Text', hint: 'Turns text into vectors for `POST /v1/embeddings`.' },
  { id: 'audio-transcription', label: 'Speech to text', group: 'Audio', hint: 'Transcribes uploaded audio on `POST /v1/audio/transcriptions`.' },
  { id: 'audio-speech', label: 'Text to speech', group: 'Audio', hint: 'Speaks text as audio for `POST /v1/audio/speech`.' },
  { id: 'images-generations', label: 'Image generation', group: 'Images and video', hint: 'Generates images on `POST /v1/images/generations`.' },
  { id: 'models', label: 'Models', group: 'Text', hint: 'Lists the provider’s models for `GET /v1/models`.' },
  { id: 'messages', label: 'Messages', group: 'Text', hint: 'Accepts Anthropic-shaped `POST /v1/messages` requests.' },
  { id: 'tool-calling', label: 'Tool calling', group: 'Text', hint: 'Honours tools and functions in a chat request and returns their calls.' },
  { id: 'reasoning', label: 'Reasoning', group: 'Text', hint: 'Can emit thinking or reasoning content alongside the answer.' },
  { id: 'vision', label: 'Vision', group: 'Text', hint: 'Accepts image input in prompts.' },
  { id: 'audio-translation', label: 'Audio translation', group: 'Audio', hint: 'Translates uploaded speech into English text on `POST /v1/audio/translations`.' },
  { id: 'images-edits', label: 'Image edits', group: 'Images and video', hint: 'Edits an existing image with a prompt on `POST /v1/images/edits`.' },
  { id: 'images-variations', label: 'Image variations', group: 'Images and video', hint: 'Produces alternatives from one image on `POST /v1/images/variations`.' },
  { id: 'moderations', label: 'Moderations', group: 'Other', hint: 'Classifies content on `POST /v1/moderations`.' },
  { id: 'video', label: 'Video', group: 'Images and video', hint: 'Accepts video jobs through `/v1/videos` and serves their status and rendered bytes.' },
  { id: 'realtime', label: 'Realtime', group: 'Audio', hint: 'Opens the Realtime handshake on `POST /v1/realtime/client_secrets` and `/v1/realtime/calls`.' },
  { id: 'vector-stores', label: 'Vector stores', group: 'Other', hint: 'Manages `/v1/vector_stores` and its files and search sub-resources.' },
  { id: 'containers', label: 'Containers', group: 'Other', hint: 'Manages `/v1/containers` and its files sub-resource.' },
  { id: 'fine-tuning', label: 'Fine-tuning', group: 'Other', hint: 'Manages `/v1/fine_tuning/jobs` and their events and checkpoints.' },
  { id: 'evals', label: 'Evals', group: 'Other', hint: 'Manages `/v1/evals`, its runs and output items.' },
  { id: 'search', label: 'Search', group: 'Other', hint: 'Answers hosted web search on `POST /v1/search`.' },
];
// The headings the capability editor renders, in the order it renders them. `chat` and `models`
// are what a plain text provider needs, which is why the Text group says so once instead of
// repeating it on both rows; the other groups exist for upstreams that serve one extra modality
// (Fish Audio: speech only; Deepgram: transcription only) or one management surface.
const CAPABILITY_GROUPS = [
  { id: 'Text', hint: 'What a plain text provider needs: Chat and Models. Add the rest only when the upstream actually serves them.' },
  { id: 'Audio', hint: 'Transcription, translation, speech and realtime are separate upstream routes; tick only the ones this provider offers.' },
  { id: 'Images and video', hint: 'Generation, editing, variations and video are separate upstream routes; tick only what this provider serves.' },
  { id: 'Other', hint: 'Management surfaces and hosted extras. None of these is needed for ordinary chat traffic.' },
];
// The same pair `default_capabilities()` in mr-core fills in when a provider's `capabilities` key
// is absent. A hand-edited config can also carry an empty array, which the server refuses, so the
// form treats that as absent too rather than presenting an unticked provider it cannot save.
const DEFAULT_CAPABILITIES = ['chat', 'models'];

/// Filter a Set/array of wire ids through the table's order, dropping ids this panel does not
/// know. Stable order keeps saves diff-clean. Dropping rather than throwing is deliberate: a
/// stale panel must not send a spelling this binary's server cannot deserialise, and one stale
/// id is no reason to lose the operator's other ticks.
function capabilityIds(selected) {
  const wanted = new Set(selected ?? []);
  return CAPABILITIES.filter((capability) => wanted.has(capability.id)).map((capability) => capability.id);
}

const KEY_STRATEGIES = ['priority', 'round-robin', 'random', 'least-used'];
// What a refusal parks: the credential (every model), that model on the credential, or that model
// on every credential of the provider. Widening past `account` is only right when the upstream
// meters models separately (a per-model quota) or shares one bucket across all of its keys.
const COOLDOWN_SCOPES = ['account', 'model', 'provider'];
const DELETE_PROVIDER_CONFIRM_KEY = 'manarouter.confirm.deleteProvider';
// Provider list preferences live in this browser: pins and a manual order are a per-operator
// view choice, not router configuration, so they must not create a config write (or a digest
// change that refreshes every other panel tab). Sort modes: name (default), added (the order the
// config file lists them in, our only proxy for creation date), keys (most accounts first) and
// manual.
const PROVIDER_SORT_KEY = 'manarouter.providers.sort';
const PROVIDER_PINS_KEY = 'manarouter.providers.pins';
const PROVIDER_ORDER_KEY = 'manarouter.providers.order';
const PROVIDER_SORT_MODES = [
  ['name', 'Name (A–Z)'],
  ['added', 'Date added'],
  ['keys', 'Number of keys'],
  ['manual', 'Manual order'],
];
const PROVIDER_SORT_IDS = new Set(PROVIDER_SORT_MODES.map(([id]) => id));

// A provider's label is optional, and seven of the live rows have none. An id-only row is still a
// row an operator has to identify at a glance, so fall back to the id as words. These aliases cover
// the hosts whose ids are not readable at a glance; labels always win when the server sends one.
const PROVIDER_NAME_ALIASES = Object.freeze({
  cheaperinference: 'Cheaper Inference',
  commandcodego: 'Command Code Go',
  freetheai: 'FreeTheAI',
  local8100: 'Local 8100',
  onerouter: 'OneRouter',
  uncloseai: 'Unclose AI',
  vllm: 'vLLM',
});
const PROVIDER_NAME_WORDS = Object.freeze({
  ai: 'AI',
  api: 'API',
  gpt: 'GPT',
  llm: 'LLM',
  nvidia: 'NVIDIA',
  openai: 'OpenAI',
  openrouter: 'OpenRouter',
  ovh: 'OVH',
  vllm: 'vLLM',
});

function providerDisplayName(id) {
  const raw = String(id ?? '').trim();
  if (!raw) return 'Unnamed provider';
  const alias = PROVIDER_NAME_ALIASES[raw.toLowerCase()];
  if (alias) return alias;
  return raw.split(/[-_.\s]+/).filter(Boolean).map((part) => {
    const known = PROVIDER_NAME_WORDS[part.toLowerCase()];
    if (known) return known;
    return part.charAt(0).toUpperCase() + part.slice(1);
  }).join(' ');
}

// Health is server-reported when available and derived otherwise. Its chip class is the only
// colour signal the formatter returns, so translate that into a small status dot.
function healthTone(health) {
  const match = /chip-(green|amber|red|neutral)/.exec(health?.cls ?? '');
  return match ? match[1] : 'neutral';
}

// Defensive: the current provider payload has no error field, but if a later control plane adds
// one the table should show it rather than silently claiming a provider is healthy.
function providerIssue(provider) {
  for (const value of [provider?.last_error, provider?.lastError, provider?.error]) {
    if (typeof value === 'string' && value.trim()) return value.trim();
    if (value && typeof value === 'object') {
      const message = value.message ?? value.detail ?? value.error;
      if (typeof message === 'string' && message.trim()) return message.trim();
    }
  }
  return '';
}

function storedIdList(key) {
  try {
    const parsed = JSON.parse(localStorage.getItem(key) ?? '[]');
    return Array.isArray(parsed) ? parsed.filter((id) => typeof id === 'string' && id) : [];
  } catch {
    return [];
  }
}

function storeIdList(key, ids) {
  try {
    localStorage.setItem(key, JSON.stringify(ids));
    return true;
  } catch {
    return false;
  }
}
// Sourced from catalog/providers.toml and the OAuth dialect docs. These are documented production
// endpoints, but remain suggestions: account-specific gateways or a deliberate proxy can differ.
// Provider presets, split into two clearly separated categories so that NVIDIA NIM, Codex,
// Copilot, Antigravity and Command Code (developer platforms / enterprise endpoints) are never
// mixed together with the standard OpenAI/Anthropic LLM APIs and the single-endpoint media APIs.
// Selecting one pre-fills dialect, base URL, auth kind, capabilities and, where the catalog names
// one, the route override that makes an OpenAI-shaped dialect hit the real endpoint. You still
// need to add an account with the actual API key afterward (the panel never stores plaintext
// keys).
const PROVIDER_PRESETS = {
  // Standard APIs — bearer/header auth, OpenAI/Anthropic-shaped endpoints. The last four are the
  // catalog capability-gated media upstreams: exactly one capability each, never chat.
  providers: [
    { name: 'Custom (manual)', dialect: 'open-ai', baseUrl: '', auth: 'bearer', hint: 'No defaults; set the dialect, base URL, and auth manually.' },
    { name: 'OpenAI', dialect: 'open-ai', baseUrl: 'https://api.openai.com/v1', auth: 'bearer', capabilities: ['chat', 'responses', 'embeddings', 'audio-transcription', 'audio-speech', 'images-generations', 'models', 'tool-calling', 'reasoning', 'vision'], hint: 'api.openai.com. Add your OpenAI API key as an account secret after saving.' },
    { name: 'OpenRouter', dialect: 'open-ai', baseUrl: 'https://openrouter.ai/api/v1', auth: 'bearer', capabilities: ['chat', 'responses', 'embeddings', 'models', 'tool-calling', 'reasoning', 'vision'], hint: 'openrouter.ai. Add your OpenRouter API key as an account secret after saving.' },
    { name: 'Groq', dialect: 'open-ai', baseUrl: 'https://api.groq.com/openai/v1', auth: 'bearer', capabilities: ['chat', 'models', 'audio-transcription', 'audio-speech', 'tool-calling', 'reasoning'], hint: 'groq.com. Add your Groq API key as an account secret after saving.' },
    { name: 'Together AI', dialect: 'open-ai', baseUrl: 'https://api.together.xyz/v1', auth: 'bearer', capabilities: ['chat', 'embeddings', 'models', 'tool-calling', 'reasoning', 'vision'], hint: 'together.xyz. Add your Together API key as an account secret after saving.' },
    { name: 'Mistral', dialect: 'open-ai', baseUrl: 'https://api.mistral.ai/v1', auth: 'bearer', capabilities: ['chat', 'embeddings', 'models', 'tool-calling', 'reasoning', 'vision'], hint: 'mistral.ai. Add your Mistral API key as an account secret after saving.' },
    { name: 'DeepSeek', dialect: 'open-ai', baseUrl: 'https://api.deepseek.com/v1', auth: 'bearer', capabilities: ['chat', 'responses', 'models', 'tool-calling', 'reasoning'], hint: 'deepseek.com. Add your DeepSeek API key as an account secret after saving.' },
    { name: 'Fireworks AI', dialect: 'open-ai', baseUrl: 'https://api.fireworks.ai/inference/v1', auth: 'bearer', capabilities: ['chat', 'embeddings', 'models', 'tool-calling', 'reasoning', 'vision'], hint: 'fireworks.ai. Add your Fireworks API key as an account secret after saving.' },
    { name: 'Perplexity', dialect: 'open-ai', baseUrl: 'https://api.perplexity.ai', auth: 'bearer', capabilities: ['chat', 'models', 'tool-calling', 'reasoning'], hint: 'perplexity.ai. Add your Perplexity API key as an account secret after saving.' },
    { name: 'Cohere', dialect: 'open-ai', baseUrl: 'https://api.cohere.com/compatibility/v1', auth: 'bearer', capabilities: ['chat', 'embeddings', 'models', 'tool-calling', 'reasoning', 'vision'], hint: 'cohere.com, OpenAI-compatibility surface. Add your Cohere API key as an account secret after saving.' },
    { name: 'Anthropic', dialect: 'anthropic', baseUrl: 'https://api.anthropic.com/v1', auth: 'bearer', capabilities: ['messages', 'models', 'tool-calling', 'reasoning', 'vision'], hint: 'api.anthropic.com. Add your Anthropic API key as an account secret after saving.' },
    { name: 'Google Gemini', dialect: 'google-ai-studio', baseUrl: 'https://generativelanguage.googleapis.com/v1beta', auth: 'api-key-header', capabilities: ['chat', 'embeddings', 'models', 'tool-calling', 'reasoning', 'vision'], authHeader: 'x-goog-api-key', hint: 'The public Gemini API at generativelanguage.googleapis.com, reached with an AI Studio API key. Add the key as an account secret after saving; this endpoint wants it in the x-goog-api-key header.' },
    { name: 'Fish Audio', dialect: 'open-ai', baseUrl: 'https://api.fish.audio', auth: 'bearer', capabilities: ['audio-speech'], routeOverrides: { 'audio-speech': 'v1/tts' }, hint: 'Speech synthesis only, on /v1/tts. Add your Fish Audio API key as an account secret after saving. The upstream body still uses Fish voice references rather than the OpenAI voice field.' },
    { name: 'ElevenLabs', dialect: 'open-ai', baseUrl: 'https://api.elevenlabs.io', auth: 'header', authName: 'xi-api-key', authPrefix: '', capabilities: ['audio-speech'], hint: 'Speech synthesis only, sent with the xi-api-key header. The catalog marks the endpoint UNCERTAIN: synthesis lives under /v1/text-to-speech/{voice_id}, and a static route override cannot carry a per-request voice id.' },
    { name: 'Deepgram', dialect: 'open-ai', baseUrl: 'https://api.deepgram.com', auth: 'header', authName: 'Authorization', authPrefix: 'Token ', capabilities: ['audio-transcription'], routeOverrides: { 'audio-transcription': 'v1/listen' }, hint: 'Transcription only, on /v1/listen. The Authorization header carries the key with a Token prefix; add the key as an account secret after saving.' },
    { name: 'Stability AI', dialect: 'open-ai', baseUrl: 'https://api.stability.ai', auth: 'bearer', capabilities: ['images-generations'], routeOverrides: { 'images-generations': 'v1/stable-image/generate/core' }, hint: 'Image generation only, on /v1/stable-image/generate/core. Add your Stability AI API key as an account secret after saving; the upstream body is multipart rather than the OpenAI JSON shape.' },
  ],
  // Developer platforms and enterprise endpoints — OAuth or dedicated infrastructure,
  // visually separated from the standard APIs above.
  platforms: [
    { name: 'NVIDIA NIM', dialect: 'open-ai', baseUrl: 'https://integrate.api.nvidia.com/v1', auth: 'bearer', capabilities: ['chat', 'embeddings', 'models', 'tool-calling', 'reasoning', 'vision'], hint: 'NVIDIA NIM inference endpoints. Add your NVIDIA API key as an account secret after saving.' },
    { name: 'Codex', dialect: 'codex', baseUrl: 'https://chatgpt.com/backend-api/codex', auth: 'oauth', platform: 'openai-codex', capabilities: ['responses', 'tool-calling', 'reasoning', 'vision'], hint: 'OpenAI Codex — OAuth sign-in from the panel.' },
    { name: 'Copilot', dialect: 'copilot', baseUrl: 'https://api.githubcopilot.com', auth: 'oauth', platform: 'github-copilot', capabilities: ['chat', 'responses', 'messages', 'embeddings', 'models', 'tool-calling', 'reasoning', 'vision'], hint: 'GitHub Copilot — OAuth sign-in from the panel.' },
    { name: 'Antigravity', dialect: 'antigravity', baseUrl: 'https://cloudcode-pa.googleapis.com', auth: 'oauth', platform: 'antigravity', capabilities: ['chat', 'models', 'tool-calling', 'reasoning', 'vision'], hint: 'Google Antigravity / Cloud Code — OAuth sign-in from the panel.' },
    { name: 'Cursor', dialect: 'cursor', baseUrl: 'https://api2.cursor.sh', auth: 'oauth', platform: 'cursor', capabilities: ['chat'], hint: 'Cursor — OAuth sign-in from the panel. The sign-in finishes in Cursor’s own tab and the panel learns of it by polling, so nothing is ever pasted back here.' },
    { name: 'Z Code', dialect: 'zcode', baseUrl: 'https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1', auth: 'oauth', platform: 'zcode', capabilities: ['messages', 'tool-calling', 'reasoning'], hint: 'Z.ai coding plan — sign in with the ZCode desktop app’s own login flow; the link opens at zcode.z.ai and the panel waits for the browser.' },
    { name: 'Command Code', dialect: 'command-code-go', baseUrl: 'https://api.commandcode.ai', auth: 'bearer', capabilities: ['chat', 'models', 'tool-calling', 'reasoning'], hint: 'Command Code — bearer auth.' },
    { name: 'Modal vLLM', dialect: 'modal-vllm', baseUrl: '', auth: 'modal', hint: 'Enter your Modal app’s HTTPS public URL (for example https://your-app--serve.modal.run). Add the Modal Token ID.Token Secret as the account secret after saving; the panel sends it as Modal-Key and Modal-Secret headers.' },
    { name: 'OpenCode Zen', dialect: 'open-ai', baseUrl: 'https://opencode.ai/zen/v1', auth: 'bearer', capabilities: ['chat', 'responses', 'models', 'tool-calling', 'reasoning'], hint: 'OpenCode Console, pay-as-you-go. Needs a funded key: the free models are refused outside the OpenCode client, so they cannot be routed from here.' },
    { name: 'OpenCode Go', dialect: 'open-ai', baseUrl: 'https://opencode.ai/zen/go/v1', auth: 'bearer', capabilities: ['chat', 'responses', 'models', 'tool-calling', 'reasoning'], hint: 'OpenCode Go subscription tier. Every request needs an x-opencode-session header, which this provider will not add for you — set it as a static header. Only the models your plan lists will answer; sync them from this host.' },
  ],
};

const DIALECT_BASE_URLS = {
  // Both Gemini dialects are the same host; they differ in their thinking rules, not their URL.
  'google-ai-studio': 'https://generativelanguage.googleapis.com/v1beta',
  google: 'https://generativelanguage.googleapis.com/v1beta',
  codex: 'https://chatgpt.com/backend-api/codex',
  antigravity: 'https://cloudcode-pa.googleapis.com',
  'command-code-go': 'https://api.commandcode.ai',
  copilot: 'https://api.githubcopilot.com',
  cursor: 'https://api2.cursor.sh',
};
// The dialects that have no API-key path: their request builders expect the header an OAuth
// login produces, and the Cursor transport refuses a turn before opening a socket when no such
// header is present. The panel used to fill in only the base URL when a dialect was chosen, so a
// provider created that way was stored with `auth: bearer` and could never reach the Connect
// button — it saved cleanly and then failed every request. Naming the platform next to the
// dialect lets choosing one choose the other.
//
// Derived from the preset table rather than written out again: two hand-kept copies of the same
// four pairs is exactly how the panel and the registry drift apart. The platform ids are the ones
// `/api/oauth` answers to (`openai-codex`, `github-copilot`), not the dialect spellings.
const DIALECT_OAUTH_PLATFORMS = Object.freeze(Object.fromEntries(
  PROVIDER_PRESETS.platforms
    .filter((preset) => preset.auth === 'oauth' && preset.platform)
    .map((preset) => [preset.dialect, preset.platform]),
));

const AUTH_KINDS = [
  ['bearer', 'Authorization: Bearer'],
  ['api-key-header', 'API key header (x-api-key)'],
  ['header', 'Custom header'],
  ['query-param', 'Query parameter'],
  ['oauth', 'OAuth (platform-managed)'],
  ['modal', 'Modal-Key + Modal-Secret'],
];

export function buildOAuthAccountForm(p, ctx, onConnected, isDrawerOpen = () => true) {
  const status = h('p', { class: 'muted' }, 'OAuth credentials are stored only in the encrypted local vault.');
  const progress = h('p', { class: 'muted' });
  const actions = h('div', { class: 'toolbar', style: 'padding:0;border:0;background:none;' });
  // /authorize fills the session id in here; the paste block below reads the same box, so it can
  // exist (hidden) before a session does and appear the instant the browser tab opens.
  const session = { id: null };
  const redirectPaste = buildRedirectPaste(p, ctx, session);
  const form = h('div', {}, status, actions, progress, redirectPaste.block);

  // One-click: the Connect button appears immediately. Discovery runs in the background only
  // to learn whether a device-code fallback is offered; it never blocks the button.
  const connect = h('button', { class: 'btn btn-primary', type: 'button', 'aria-label': 'Connect (opens provider sign-in in a new window)' }, 'Connect');
  actions.append(connect);

  let details = null;
  (async () => {
    try {
      const { platforms = [] } = await api.get('/api/oauth');
      details = platforms.find((x) => x.id === p.auth.platform) || null;
    } catch { details = null; }
    if (!details) return;
    const flows = Array.isArray(details.flows) && details.flows.length ? details.flows : [details.flow];
    // If this platform only supports device code, relabel the primary button.
    if (!flows.includes('browser') && flows.includes('device')) {
      connect.textContent = 'Connect with a device code';
    }
  })();

  connect.addEventListener('click', () => {
    connect.disabled = true;
    startFlow()
      .catch((err) => {
        connect.disabled = false;
        ctx.toast.error('OAuth sign-in failed', err);
        progress.replaceChildren(errorBlock(err));
      });
  });

  async function startFlow() {
    // details.id is the same platform id stored on the provider (p.auth.platform), so we don't
    // need to wait for discovery to start the browser flow.
    const platformId = (details && details.id) || p.auth.platform;
    let flows;
    if (details) {
      flows = Array.isArray(details.flows) && details.flows.length ? details.flows : [details.flow];
      if (flows.includes('screen')) return runScreen(platformId);
      if (flows.includes('browser')) return runBrowser(platformId);
      if (flows.includes('device')) return runDevice(platformId);
      throw new Error('No OAuth flow available for this platform.');
    }
    // Discovery hasn't answered yet: try browser, then device code, then the provider-page
    // flow. Each refuses immediately when it does not apply, so the cost of guessing is two
    // rejected requests rather than a wrong tab left open.
    try { return await runBrowser(platformId); }
    catch {
      try { return await runDevice(platformId); }
      catch { return await runScreen(platformId); }
    }
  }

  /// Open the provider's page, then wait for the loopback callback.
  ///
  /// The completion call is issued *after* the tab is opened and blocks until the browser comes
  /// back, which is what makes this a single click: the operator approves in the tab and this
  /// resolves on its own. The window is opened before the await so the browser does not treat it
  /// as a popup triggered by nothing.
  async function runBrowser(platformId) {
    const start = await api.post(`/api/oauth/${encodeURIComponent(platformId)}/authorize`, { provider_id: p.id });
    session.id = start.session_id;
    const authorizeUrl = httpUrl(start.authorization_url);
    if (!authorizeUrl) {
      // No URL to open means the browser flow cannot proceed. Fail fast instead of waiting
      // forever for a /complete that will never arrive.
      session.id = null;
      throw new Error('The provider did not return a sign-in URL. Try again, or use a device code if available.');
    }
    window.open(authorizeUrl, '_blank', 'noopener,noreferrer');
    // Immediate, not after a timeout: the person who knows the capture failed can act now. The
    // /complete call below is already queued and will resolve once the pasted URL is accepted.
    let redirectUri = null;
    try {
      redirectUri = new URL(authorizeUrl).searchParams.get('redirect_uri');
    } catch { redirectUri = null; }
    redirectPaste.show(redirectUri);
    progress.replaceChildren(
      h('span', {}, 'Waiting for you to finish signing in… '),
      h('a', { href: authorizeUrl, target: '_blank', rel: 'noopener noreferrer' }, 'Reopen the page'),
    );
    try {
      const done = await api.post(`/api/oauth/sessions/${encodeURIComponent(start.session_id)}/complete`, { provider_id: p.id });
      await reportConnected(done);
    } finally {
      // Success, failure or a flow abandoned by closing the drawer: no stale paste form is left
      // behind, and a later attempt starts from a clean block.
      session.id = null;
      redirectPaste.hide();
    }
  }

  async function runDevice(platformId) {
    const start = await api.post(`/api/oauth/${encodeURIComponent(platformId)}/device-code`, { provider_id: p.id });
    const verificationUrl = httpUrl(start.verification_uri);
    progress.replaceChildren(
      h('span', {}, 'Open '),
      verificationUrl
        ? h('a', { href: verificationUrl, target: '_blank', rel: 'noopener noreferrer' }, start.verification_uri)
        : h('span', { class: 'muted' }, start.verification_uri),
      h('span', {}, ' and enter this code: '),
      h('strong', { class: 'mono', style: 'letter-spacing:2px;font-size:15px;' }, start.user_code),
    );
    if (verificationUrl) window.open(verificationUrl, '_blank', 'noopener,noreferrer');
    const ttl = Number(start.expires_in_seconds);
    const deadline = Date.now() + (Number.isFinite(ttl) && ttl > 0 ? ttl * 1000 : 300000);
    let intervalMs = Math.max(1000, Number(start.interval_seconds) * 1000 || 1000);
    while (Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, intervalMs));
      const result = await api.post(`/api/oauth/sessions/${encodeURIComponent(start.session_id)}/poll`, { provider_id: p.id });
      if (result.status === 'complete') {
        await reportConnected(result);
        return;
      }
      if (result.status === 'failed') throw new Error(result.message);
      if (result.interval_seconds) intervalMs = Math.max(intervalMs, Number(result.interval_seconds) * 1000 || 0);
      if (result.retry_after_seconds) intervalMs = Math.max(intervalMs, Number(result.retry_after_seconds) * 1000 || 0);
    }
    throw new Error('The device code expired before it was approved. Start again.');
  }

  /// Open the provider's own page and watch it until the login is finished there.
  ///
  /// This flow has no redirect and no code: the page does the whole sign-in, and the only way to
  /// learn that it worked is to ask. The tab is opened before the first await for the same reason
  /// as the browser flow — a window opened afterwards is treated as a popup and blocked.
  async function runScreen(platformId) {
    const start = await api.post(`/api/oauth/${encodeURIComponent(platformId)}/screen`, { provider_id: p.id });
    session.id = start.session_id;
    const loginUrl = httpUrl(start.login_url);
    if (!loginUrl) {
      // No page to open means nothing can be finished. Fail now instead of polling a session the
      // operator cannot possibly complete.
      session.id = null;
      throw new Error('The provider did not return a sign-in page. Try again.');
    }
    window.open(loginUrl, '_blank', 'noopener,noreferrer');
    progress.replaceChildren(
      h('span', {}, 'Waiting for you to finish signing in… '),
      h('a', { href: loginUrl, target: '_blank', rel: 'noopener noreferrer' }, 'Reopen the page'),
    );
    // The server owns the cadence and the deadline; this bound only stops a browser left open
    // overnight from polling forever. It is comfortably longer than the session it polls.
    const deadline = Date.now() + 15 * 60 * 1000;
    let intervalMs = 1000;
    try {
      while (Date.now() < deadline) {
        await new Promise((resolve) => setTimeout(resolve, intervalMs));
        let result;
        try {
          result = await api.post(`/api/oauth/sessions/${encodeURIComponent(start.session_id)}/poll`, { provider_id: p.id });
        } catch (err) {
          // The server splits these cleanly: a session that has expired or vanished (404/410) is
          // gone, and a request it refuses (400) — or the right to poll it being denied (403) — is
          // not going to start working. Both end the flow. A busy session (409), a rate limit (429)
          // and any server-side fault are worth one more poll rather than a whole restart.
          const status = Number(err?.status) || 0;
          if (status >= 400 && status < 500 && status !== 409 && status !== 429) throw err;
          continue;
        }
        if (result.status === 'complete') {
          await reportConnected(result);
          return;
        }
        if (result.status === 'failed') throw new Error(result.message);
        if (result.retry_after_seconds) intervalMs = Math.max(1000, Number(result.retry_after_seconds) * 1000 || 0);
        if (result.interval_seconds) intervalMs = Math.max(intervalMs, Number(result.interval_seconds) * 1000 || 0);
      }
      throw new Error('The sign-in page expired before it was finished. Start again.');
    } finally {
      session.id = null;
    }
  }

  /// The single success path for all three flows.
  ///
  /// It lives in one place because the copies drift: the browser flow reported the account label
  /// while the device flow did not, so the same success read differently depending on how the
  /// operator got there.
  async function reportConnected(result) {
    progress.replaceChildren(h('span', {}, 'Connected.'));
    ctx.toast.success(`Connected ${result.account?.label || 'OAuth account'}.`);
    // If the drawer was closed mid-flow, don't reopen it or navigate — the user explicitly
    // dismissed it. Otherwise hand off to the single navigation helper.
    if (isDrawerOpen() && onConnected) {
      try { await onConnected(); } catch { /* afterAccountConnected reports its own failures */ }
    }
  }

  return form;
}

export async function renderProviders(mount, ctx) {
  let providers = [];
  let drawerId = null;
  let drawerOpen = false;
  // True only while the inline provider detail is the panel in workWrap. The form and account
  // drawers also use drawerId, but clicking their row should still open the provider detail.
  let detailOpen = false;
  // The open provider form, if any. Its page-leave guard and dirty flag live here so every close
  // path can ask before a half-filled provider is thrown away.
  let activeForm = null;
  let filterText = '';
  // Keep in-progress inline number edits across any redraw caused by a refresh or another action.
  const inlineDrafts = new Map();
  // Pins, manual order and the selected sort are browser preferences, never router config.
  let sortMode = (() => {
    try {
      const stored = localStorage.getItem(PROVIDER_SORT_KEY);
      return PROVIDER_SORT_IDS.has(stored) ? stored : 'name';
    } catch {
      return 'name';
    }
  })();
  let pinned = new Set(storedIdList(PROVIDER_PINS_KEY));
  let manualOrder = storedIdList(PROVIDER_ORDER_KEY);
  let draggedId = null;
  let dragSuppressed = false;

  // Scopes every override in lab-providers.css to this view; the shared sheet is untouched.
  mount.classList?.add('providers-view');

  const tableWrap = h('div', { class: 'table-scroll' });
  // The provider detail used to be a fixed right-hand overlay. It is now a panel in the page,
  // expanded directly under the provider's own row, so opening a provider never scrolls past
  // the rest of a long list to find it.
  const workWrap = h('div', { class: 'provider-work' });
  let detailRow = null;
  const count = h('span', { class: 'count provider-count', 'aria-live': 'polite' });
  const newBtn = h('button', { class: 'btn btn-primary provider-add', type: 'button' }, icon('plus'), 'Add provider');
  // Per-account quota cache: key = "providerId|accountId" -> { remaining, total, windows, error, fetched_at }
  const quotaCache = new Map();
  let quotaLoaded = false;

  async function loadQuota() {
    if (quotaLoaded) return quotaCache;
    quotaLoaded = true;
    try {
      const data = await api.get('/api/quota');
      const entries = Array.isArray(data) ? data
        : Array.isArray(data?.accounts) ? data.accounts
          : Array.isArray(data?.results) ? data.results
            : [];
      for (const entry of entries) {
        if (!entry || typeof entry !== 'object') continue;
        const provider = typeof entry.provider === 'string' ? entry.provider : entry.provider?.id;
        const account = typeof entry.account === 'string' ? entry.account : entry.account?.id;
        if (!provider || !account) continue;
        quotaCache.set(`${provider}|${account}`, entry);
      }
    } catch {
      // Quota endpoint may not be available in older builds — that's fine, we just skip quota display.
    }
    return quotaCache;
  }
  const refreshBtn = h('button', { class: 'btn btn-ghost provider-refresh', type: 'button' }, icon('sync'), 'Refresh');
  const sortSel = h('select', { class: 'input', 'aria-label': 'Sort providers' },
    ...PROVIDER_SORT_MODES.map(([id, text]) => h('option', { value: id }, text)));
  sortSel.value = sortMode;
  const sortControl = h('label', { class: 'provider-sort' },
    h('span', { class: 'provider-sort-label' }, 'Sort'), sortSel);
  const filterInput = h('input', {
    class: 'input provider-filter',
    type: 'search',
    placeholder: 'Filter by name, ID, or dialect',
    'aria-label': 'Filter providers by name, ID, or dialect',
    autocomplete: 'off',
    spellcheck: 'false',
  });
  filterInput.addEventListener('input', () => {
    filterText = filterInput.value.trim().toLowerCase();
    renderTable();
  });
  // Deleting still asks for an explicit click. The opt-out belongs at the bottom of the list, where
  // it reads as a preference about deletion rather than another action competing with Add provider.
  const deleteWarning = toggle('Do not warn before deleting a provider', {
    checked: deleteProviderWarningDisabled(),
  });
  deleteWarning.input.addEventListener('change', () => {
    try {
      if (deleteWarning.input.checked) localStorage.setItem(DELETE_PROVIDER_CONFIRM_KEY, 'true');
      else localStorage.removeItem(DELETE_PROVIDER_CONFIRM_KEY);
    } catch {
      ctx.toast.warning('This browser could not save the provider deletion preference.');
    }
  });
  const listFoot = h('div', { class: 'provider-list-foot' },
    h('p', { class: 'provider-list-hint' }, 'Pinned providers stay on top. Drag a row to reorder.'),
    h('div', { class: 'provider-delete-pref' },
      deleteWarning.row,
      h('p', { class: 'hint' }, 'Deleting a provider still requires an explicit Delete click.')));
  const toolbar = h('div', { class: 'toolbar provider-toolbar' },
    // The count belongs to the list the filter is narrowing, so it sits with the filter rather
    // than floating between the two groups. The right side holds only things you can act on.
    h('div', { class: 'provider-filter-wrap' }, filterInput, count),
    h('div', { class: 'provider-tools' },
      sortControl,
      h('span', { class: 'toolbar-divider', 'aria-hidden': 'true' }),
      refreshBtn,
      newBtn));

  mount.replaceChildren(toolbar, tableWrap, listFoot);

  // Escape closes an inline provider detail even when focus stayed on the row button that opened
  // it. A modal owns Escape while it is open, and must not close the panel behind it as well.
  mount.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape' || !drawerOpen || document.querySelector('.dialog-backdrop')) return;
    closeDrawer();
  });

  function saveSort(mode) {
    sortMode = mode;
    sortSel.value = mode;
    try {
      localStorage.setItem(PROVIDER_SORT_KEY, mode);
    } catch {
      ctx.toast.warning('This browser could not save the provider sort preference.');
    }
  }

  function savePins() {
    if (!storeIdList(PROVIDER_PINS_KEY, [...pinned])) {
      ctx.toast.warning('This browser could not save pinned providers.');
    }
  }

  function saveManualOrder(ids) {
    manualOrder = ids;
    if (!storeIdList(PROVIDER_ORDER_KEY, ids)) {
      ctx.toast.warning('This browser could not save the provider order.');
    }
  }

  /// The list in display order: pinned first, then the chosen sort inside each group.
  ///
  /// "Date added" is the order /api/providers returns, which is the config file's order — the
  /// provider record has no created_at, and inventing a timestamp from the newest sync would
  /// reorder a provider every time it refreshed.
  function orderedProviders() {
    const source = providers.slice();
    const configIndex = new Map(source.map((p, index) => [p.id, index]));
    const manualIndex = new Map(manualOrder.map((id, index) => [id, index]));
    if (sortMode === 'name') {
      source.sort((a, b) => String(a.id).localeCompare(String(b.id), undefined, { sensitivity: 'base', numeric: true }));
    } else if (sortMode === 'keys') {
      source.sort((a, b) => (b.accounts?.length ?? 0) - (a.accounts?.length ?? 0)
        || String(a.id).localeCompare(String(b.id), undefined, { sensitivity: 'base', numeric: true }));
    } else if (sortMode === 'manual') {
      source.sort((a, b) => (manualIndex.get(a.id) ?? Number.MAX_SAFE_INTEGER) - (manualIndex.get(b.id) ?? Number.MAX_SAFE_INTEGER)
        || (configIndex.get(a.id) ?? 0) - (configIndex.get(b.id) ?? 0));
    }
    // A stable partition: the chosen sort never lifts an unpinned provider above a pinned one.
    return [...source.filter((p) => pinned.has(p.id)), ...source.filter((p) => !pinned.has(p.id))];
  }

  function togglePin(id) {
    if (pinned.has(id)) pinned.delete(id); else pinned.add(id);
    savePins();
    renderTable();
  }

  function switchSort(mode) {
    if (mode === 'manual') {
      // Freeze what the operator is looking at before manual becomes the source of truth.
      saveManualOrder(orderedProviders().map((p) => p.id));
    }
    saveSort(mode);
    renderTable();
  }

  function reorderProvider(sourceId, targetId) {
    if (sourceId === targetId) return;
    if (pinned.has(sourceId) !== pinned.has(targetId)) return;
    const list = orderedProviders();
    const from = list.findIndex((p) => p.id === sourceId);
    if (from < 0) return;
    const [moved] = list.splice(from, 1);
    const insertAt = list.findIndex((p) => p.id === targetId);
    list.splice(insertAt < 0 ? list.length : insertAt, 0, moved);
    saveManualOrder(list.map((p) => p.id));
    saveSort('manual');
    renderTable();
  }

  sortSel.addEventListener('change', () => switchSort(sortSel.value));
  newBtn.addEventListener('click', () => { void openFormDrawer(null); });
  refreshBtn.addEventListener('click', () => reload(true));

  // Out-of-band writes have to reach this table on their own.
  //
  // The list used to be read once per page load, so a provider added by `mr provider add`, an
  // account added from another tab, or an `mr migrate` stayed invisible until the operator pressed
  // Refresh — the data was in the API the whole time, only the reader was stale. The 5 s health
  // poll already runs and carries the routing-snapshot digest, which the control plane rebuilds on
  // every configuration write; watching it costs no extra request and refreshes exactly when
  // something actually changed.
  let lastDigest = null;
  let pendingRefresh = false; // a change is known but the page is not in a state to redraw
  let digestRefreshInFlight = false;
  if (typeof ctx.onHealth === 'function') {
    ctx.onHealth((health) => {
      const digest = health?.snapshot?.digest ?? null;
      if (digest === null) return; // control plane unreachable: keep showing the last good list
      if (lastDigest === null) { lastDigest = digest; return; } // the first reading is the baseline
      if (digest !== lastDigest) {
        lastDigest = digest;
        pendingRefresh = true; // a write happened somewhere: remember it until the redraw is safe
      }
      if (!pendingRefresh) return; // nothing was written: no request, no redraw
      if (digestRefreshInFlight) return; // a write on this page is already reloading
      // A redraw replaces the table body and rebuilds an open drawer, so it is deferred while any
      // form is on screen — the next 5 s tick retries, and the change lands by itself once the
      // operator is done. Losing typed values to a background refresh is not an acceptable trade.
      if (drawerOpen || operatorIsEditing()) return;
      pendingRefresh = false;
      digestRefreshInFlight = true;
      reload(true).finally(() => { digestRefreshInFlight = false; });
    });
  }

  // An automatic refresh replaces the table body and, with a drawer open, the drawer body too. If
  // the operator's caret is in one of those fields, the change waits for a later tick instead of
  // eating a half-typed form.
  function operatorIsEditing() {
    const el = document.activeElement;
    if (!el || el.nodeType !== 1) return false;
    const tag = String(el.tagName).toUpperCase();
    if (tag !== 'INPUT' && tag !== 'TEXTAREA' && tag !== 'SELECT') return false;
    return typeof mount.contains === 'function' && mount.contains(el);
  }

  function deleteProviderWarningDisabled() {
    try { return localStorage.getItem(DELETE_PROVIDER_CONFIRM_KEY) === 'true'; }
    catch { return false; }
  }

  function refreshInlineDrawer(providerId) {
    const p = providers.find((provider) => provider.id === providerId);
    const body = workWrap.querySelector('.drawer-body');
    if (p && drawerId === providerId && body) { renderDetail(body, p); }
  }

  async function reload(force = false) {
    try {
      // Keep the server's config order: it is what "Date added" sorts by, and re-sorting here
      // would silently destroy that order before the operator ever chose a sort mode.
      providers = (await ctx.loadProviders(force)).slice();
    } catch (err) {
      tableWrap.replaceChildren(errorBlock(err));
      return;
    }
    renderTable();
    if (drawerId) {
      const fresh = providers.find((p) => p.id === drawerId);
      if (fresh) await openDetailDrawer(fresh, { silent: true });
      else closeDrawer();
    }
  }

  function providerMatches(p) {
    if (!filterText) return true;
    return [p.id, p.label, p.dialect].some((value) => String(value ?? '').toLowerCase().includes(filterText));
  }

  // The number carries the weight; the unit stays muted. No border or fill: six boxed counts in a
  // row turn the table into a row of badges, and the eye stops treating the provider name as the title.
  function countMetric(value, unit, { muted = false, empty = false, title = null } = {}) {
    if (empty) return h('span', { class: 'provider-metric is-empty', title }, 'No accounts');
    return h('span', {
      class: 'provider-metric' + (muted ? ' is-muted' : ''),
      title,
    },
      h('span', { class: 'provider-metric-n' }, String(value)),
      ' ',
      h('span', { class: 'provider-metric-unit' }, unit));
  }

  // The old row repeated a worded Copy button under every id. An icon-only control preserves the
  // action without adding 38 identical labels to the scan path; the icon and title confirm success.
  function copyIdButton(id) {
    const label = 'Copy provider ID ' + id;
    const btn = h('button', {
      class: 'icon-btn provider-copy',
      type: 'button',
      title: 'Copy provider ID',
      'aria-label': label,
    }, icon('copy'));
    btn.addEventListener('click', async (e) => {
      e.stopPropagation();
      const ok = await copyText(id);
      btn.replaceChildren(icon(ok ? 'check' : 'x'));
      btn.classList.toggle('copy-ok', ok);
      btn.title = ok ? 'Copied' : 'Copy failed';
      btn.setAttribute('aria-label', ok ? 'Copied provider ID ' + id : 'Could not copy provider ID ' + id);
      setTimeout(() => {
        btn.replaceChildren(icon('copy'));
        btn.classList.remove('copy-ok');
        btn.title = 'Copy provider ID';
        btn.setAttribute('aria-label', label);
      }, 1600);
    });
    return btn;
  }

  function emptyAddButton() {
    const btn = h('button', { class: 'btn btn-primary', type: 'button' }, icon('plus'), 'Add provider');
    btn.addEventListener('click', () => { void openFormDrawer(null); });
    return btn;
  }

  function endDrag() {
    draggedId = null;
    document.body?.classList.remove('provider-dragging');
    tableWrap.querySelectorAll('.drag-over, .dragging').forEach((row) => {
      row.classList.remove('drag-over');
      row.classList.remove('dragging');
    });
    setTimeout(() => { dragSuppressed = false; }, 0);
  }

  function providerTable(tbody) {
    return h('table', { class: 'tbl provider-table' },
      h('thead', {}, h('tr', {},
        h('th', { scope: 'col', class: 'pin-col' }, h('span', { class: 'sr-only' }, 'Pin and reorder')),
        h('th', { scope: 'col' }, 'Provider'),
        h('th', { scope: 'col', class: 'provider-accounts-col' }, 'Accounts'),
        h('th', { scope: 'col', class: 'provider-models-col' }, 'Models'),
        h('th', { scope: 'col', class: 'provider-health-col' }, 'Health'),
        h('th', { scope: 'col', class: 'actions-col' }, 'Actions'))),
      tbody);
  }

  function renderTable() {
    const ordered = orderedProviders();
    const visible = ordered.filter(providerMatches);
    const total = providers.length;
    const totalLabel = `${total} provider${total === 1 ? '' : 's'}`;
    // Split the count so the numerals carry weight and the unit stays a quiet label. A single text
    // node made "38 providers" read as loose metadata between two unrelated controls.
    const countParts = [h('strong', { class: 'provider-count-n' }, String(filterText ? visible.length : total))];
    if (filterText) countParts.push(' of ', h('strong', { class: 'provider-count-n' }, String(total)));
    countParts.push(total === 1 ? ' provider' : ' providers');
    count.replaceChildren(...countParts);
    count.title = filterText ? `Filtered by “${filterInput.value.trim()}”` : totalLabel;
    listFoot.hidden = total === 0;
    tableWrap.classList.toggle('is-empty', total === 0 || visible.length === 0);

    if (!total) {
      // With the create form open the empty card would sit behind a panel that has no table to
      // belong to. Render the normal table shell instead so the form has a place in the list.
      if (drawerOpen) {
        tableWrap.classList.remove('is-empty');
        tableWrap.replaceChildren(providerTable(h('tbody')));
        placeDetailRow();
        return;
      }
      tableWrap.replaceChildren(h('div', { class: 'provider-empty' },
        h('div', { class: 'provider-empty-mark', 'aria-hidden': 'true' }, icon('plus')),
        h('h2', {}, 'No providers yet'),
        h('p', {}, 'A provider is an upstream account source — OpenAI, Anthropic, a proxy — that the router forwards to. Add one, then connect an account and sync its models.'),
        emptyAddButton()));
      return;
    }

    if (!visible.length) {
      const clear = h('button', { class: 'btn btn-sm', type: 'button' }, 'Clear filter');
      clear.addEventListener('click', () => {
        filterInput.value = '';
        filterText = '';
        renderTable();
        filterInput.focus();
      });
      tableWrap.replaceChildren(h('div', { class: 'provider-empty is-filtered' },
        h('h2', {}, 'No providers match “', filterInput.value.trim(), '”'),
        h('p', {}, 'Try a provider name, ID, or dialect. The filter matches any part of the value.'),
        clear));
      return;
    }

    // A filter that hides the open provider would leave its detail row hanging under the next one.
    if (detailOpen && drawerId && !visible.some((p) => p.id === drawerId)) closeDrawer();

    const tbody = h('tbody');
    visible.forEach((p) => {
      const health = deriveProviderHealth(p);
      const tone = healthTone(health);
      const healthLabel = health.label ? health.label.charAt(0).toUpperCase() + health.label.slice(1) : 'Unknown';
      const issue = providerIssue(p);
      const accounts = Array.isArray(p.accounts) ? p.accounts : [];
      const enabledCount = accounts.filter((a) => a && a.enabled !== false).length;
      const models = Array.isArray(p.models) ? p.models : [];
      const isPinned = pinned.has(p.id);
      const isOpen = detailOpen && drawerId === p.id;
      const label = typeof p.label === 'string' ? p.label.trim() : '';
      const name = label || providerDisplayName(p.id);

      const openBtn = h('button', {
        class: 'btn btn-sm provider-open',
        type: 'button',
        'aria-expanded': isOpen ? 'true' : 'false',
      }, isOpen ? 'Close' : 'Open');
      openBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        if (detailOpen && drawerId === p.id) closeDrawer();
        else void openDetailDrawer(p);
      });

      // A provider with no accounts cannot serve anything, and the fix is a single action.
      // Surfacing it on the row means adding credentials is one click from the list rather than:
      // open the drawer, scroll to Accounts, find Add account / Connect.
      const needsAccount = accounts.length === 0;

      const pinBtn = h('button', {
        class: 'pin-btn' + (isPinned ? ' pinned' : ''),
        type: 'button',
        'aria-pressed': isPinned ? 'true' : 'false',
        'aria-label': (isPinned ? 'Unpin ' : 'Pin to top ') + p.id,
        title: isPinned ? 'Unpin' : 'Pin to top',
      }, icon('pin'));
      pinBtn.addEventListener('click', (e) => { e.stopPropagation(); togglePin(p.id); });

      // Reordering is always available, not only in Manual order: a drag freezes the list as it
      // is and switches the select to Manual. A move never crosses the pin boundary.
      const grip = h('span', {
        class: 'icon-btn provider-grip',
        'aria-hidden': 'true',
        title: 'Drag to reorder',
      }, icon('grip'));
      const pinCell = h('td', { class: 'pin-col' }, grip, pinBtn);

      // A dialect that just repeats the id is noise: "antigravity" twice tells you nothing. Keep
      // the reserved meta line so rows stay the same height, but leave it empty in that case.
      const dialect = String(p.dialect ?? '').trim();
      const showDialect = dialect !== '' && dialect.toLowerCase() !== String(p.id).toLowerCase();
      const identity = h('td', { class: 'provider-cell' },
        h('div', { class: 'provider-title-row' },
          h('span', { class: 'provider-disclosure', 'aria-hidden': 'true' }, h('span', { class: 'provider-chevron' })),
          h('span', { class: 'provider-name', title: name }, name),
          h('span', { class: 'provider-id mono', title: 'Provider ID: ' + p.id }, p.id),
          copyIdButton(p.id)),
        h('div', { class: 'provider-sub' },
          showDialect
            ? h('span', { class: 'provider-dialect', title: 'API dialect' }, dialect)
            : null));

      // "1/3 accounts" says more than a count plus a coloured dot, and it stops the account column
      // reusing the health column's status-dot language for a different meaning.
      const accountsMetric = accounts.length
        ? countMetric(
          enabledCount < accounts.length ? `${enabledCount}/${accounts.length}` : accounts.length,
          accounts.length === 1 ? 'account' : 'accounts',
          {
            muted: enabledCount === 0,
            title: `${enabledCount} of ${accounts.length} account${accounts.length === 1 ? '' : 's'} enabled`,
          })
        : countMetric(0, 'accounts', { empty: true, title: 'No accounts configured' });
      const modelsMetric = countMetric(models.length, models.length === 1 ? 'model' : 'models', {
        muted: models.length === 0,
        title: models.length === 0 ? 'No models synced yet' : `${models.length} models`,
      });

      const tr = h('tr', {
        class: 'row-click row-draggable'
          + (isPinned ? ' pinned-row' : '')
          + (isOpen ? ' is-open' : ''),
        tabindex: '0',
        'data-provider-id': p.id,
      },
        pinCell,
        identity,
        h('td', { class: 'provider-accounts-cell' }, accountsMetric),
        h('td', { class: 'provider-models-cell' }, modelsMetric),
        h('td', { class: 'provider-health-cell' },
          h('span', { class: 'provider-health is-' + tone, title: 'Health: ' + health.key },
            h('span', { class: 'provider-health-dot', 'aria-hidden': 'true' }),
            healthLabel),
          issue ? h('span', { class: 'provider-last-error', title: issue }, issue) : null),
        h('td', { class: 'actions' },
          ...(needsAccount
            ? [(() => {
                const actionLabel = p.auth?.type === 'oauth' ? 'Connect' : 'Add account';
                const btn = h('button', { class: 'btn btn-sm btn-primary', type: 'button' }, actionLabel);
                btn.addEventListener('click', (e) => { e.stopPropagation(); void openAccountDrawer(p.id); });
                return btn;
              })()]
            : []),
          openBtn),
      );

      const activate = () => {
        if (dragSuppressed) return;
        if (detailOpen && drawerId === p.id) closeDrawer();
        else void openDetailDrawer(p);
      };
      tr.addEventListener('click', activate);
      // The row is the disclosure control. Enter and Space open only when the row itself is
      // focused; activating the inner Open button must not bubble into a second open.
      tr.addEventListener('keydown', (e) => {
        if ((e.key === 'Enter' || e.key === ' ') && e.target === tr) {
          e.preventDefault();
          activate();
        }
      });

      tr.draggable = true;
      tr.addEventListener('dragstart', (e) => {
        if (e.target.closest('button, a, input, select, textarea')) { e.preventDefault(); return; }
        draggedId = p.id;
        dragSuppressed = true;
        if (e.dataTransfer) {
          e.dataTransfer.effectAllowed = 'move';
          try { e.dataTransfer.setData('text/plain', p.id); } catch { /* Firefox needs this only for external drags. */ }
        }
        document.body?.classList.add('provider-dragging');
        tr.classList.add('dragging');
      });
      tr.addEventListener('dragend', endDrag);
      tr.addEventListener('dragover', (e) => {
        if (!draggedId || draggedId === p.id || pinned.has(draggedId) !== isPinned) return;
        e.preventDefault();
        if (e.dataTransfer) e.dataTransfer.dropEffect = 'move';
        // Only one insertion line at a time; dragging up and down leaves stale classes otherwise.
        tableWrap.querySelectorAll('tr.drag-over').forEach((row) => {
          if (row !== tr) row.classList.remove('drag-over');
        });
        tr.classList.add('drag-over');
      });
      tr.addEventListener('dragleave', (e) => {
        if (e.relatedTarget && tr.contains(e.relatedTarget)) return;
        tr.classList.remove('drag-over');
      });
      tr.addEventListener('drop', (e) => {
        e.preventDefault();
        const source = draggedId;
        endDrag();
        if (source) reorderProvider(source, p.id);
      });
      tbody.append(tr);
    });
    tableWrap.replaceChildren(providerTable(tbody));
    if (drawerOpen) {
      placeDetailRow();
      if (detailOpen) syncRowState(drawerId, true);
    }
  }

  // ------------------------------------------------------------------ drawer

  /// Move the inline panel under the row it belongs to, replacing any previous detail row.
  function placeDetailRow() {
    if (detailRow) {
      detailRow.remove();
      detailRow = null;
    }
    const row = h('tr', { class: 'provider-detail-row' }, h('td', { colspan: '6' }, workWrap));
    // The genuine empty state has no table, but its Add provider button still opens this form.
    // Give the form the normal table shell rather than leaving workWrap detached from the page.
    let tbody = tableWrap.querySelector('tbody');
    if (!tbody) {
      tbody = h('tbody');
      tableWrap.classList.remove('is-empty');
      tableWrap.replaceChildren(providerTable(tbody));
    }
    if (!drawerId) {
      // A new provider has no row to sit under yet. Put the form at the top of the list, where
      // the action that opened it is, instead of leaving it detached from the document.
      detailRow = row;
      tbody.prepend(detailRow);
      return;
    }
    const anchor = tableWrap.querySelector('tr[data-provider-id="' + CSS.escape(drawerId) + '"]');
    if (!anchor) return;
    detailRow = row;
    anchor.after(detailRow);
  }

  // Keep the row's Open/Close affordance honest when the panel is opened, replaced or closed.
  function syncRowState(id, open) {
    if (!id) return;
    tableWrap.querySelectorAll('tr[data-provider-id]').forEach((row) => {
      if (row.getAttribute('data-provider-id') !== id) return;
      row.classList.toggle('is-open', open);
      const btn = row.querySelector('.provider-open');
      if (!btn) return;
      btn.textContent = open ? 'Close' : 'Open';
      btn.setAttribute('aria-expanded', open ? 'true' : 'false');
    });
  }

  function releaseForm(form = activeForm) {
    if (!form) return;
    form.dirty = false;
    if (form.guard && ctx.leaveGuard === form.guard) ctx.leaveGuard = null;
    form.guard = null;
    if (activeForm === form) activeForm = null;
  }

  async function confirmDiscardForm(message) {
    const form = activeForm;
    if (!form?.dirty) return true;
    if (form.confirming) return false;
    form.confirming = true;
    try {
      return await ctx.confirm({
        title: 'Leave without saving?',
        message,
        confirmLabel: 'Discard changes',
        cancelLabel: 'Keep editing',
        // Discarding a draft is destructive, so focus starts on Keep editing rather than the
        // button that throws the work away.
        danger: true,
      });
    } finally {
      form.confirming = false;
    }
  }

  async function requestCloseDrawer(message = 'This provider has unsaved changes. Closing the form will discard them.') {
    if (!(await confirmDiscardForm(message))) return;
    closeDrawer();
  }

  function closeDrawer() {
    releaseForm();
    const closingId = drawerId;
    const wasDetail = detailOpen;
    detailOpen = false;
    drawerId = null;
    drawerOpen = false;
    if (detailRow) {
      detailRow.remove();
      detailRow = null;
    }
    workWrap.replaceChildren();
    if (wasDetail) syncRowState(closingId, false);
    // Closing the create form in the empty state must bring the empty card back; the table shell
    // was only there to host the form.
    if (!providers.length) renderTable();
  }

  function drawerShell(title, ariaLabel) {
    const head = h('div', { class: 'drawer-head' },
      h('h2', {}, title),
      h('span', { class: 'spacer' }));
    const body = h('div', { class: 'drawer-body' });
    const drawer = h('div', { class: 'work-panel', role: 'region', 'aria-label': ariaLabel ?? title }, head, body);
    drawer.addEventListener('keydown', (e) => {
      if (e.key !== 'Escape') return;
      e.preventDefault();
      void requestCloseDrawer();
    });
    workWrap.replaceChildren(drawer);
    drawerOpen = true;
    placeDetailRow();
    return { drawer, head, body };
  }

  async function openDetailDrawer(provider, { silent = false } = {}) {
    if (!(await confirmDiscardForm('This provider has unsaved changes. Opening a provider will discard them.'))) return;
    closeDrawer();
    drawerId = provider.id;
    detailOpen = true;
    const title = provider.label ? provider.label + ' (' + provider.id + ')' : String(provider.id);
    const { drawer, head, body } = drawerShell(title);
    syncRowState(provider.id, true);
    head.append(copyButton(provider.id, { className: "btn btn-sm", title: "Copy provider ID" }));
    head.append(closeButton());
    await renderDetail(body, provider);
    // Move focus into the panel so Escape reaches its handler even when the opener was activated
    // by a pointer (or the row was repainted and focus fell back to the document body).
    if (!silent) {
      drawer.tabIndex = -1;
      drawer.focus({ preventScroll: true });
      // Keep the panel in view without yanking a background digest refresh around.
      requestAnimationFrame(() => { if (drawer.isConnected) drawer.scrollIntoView({ block: 'nearest' }); });
    }
  }

  function closeButton(label = 'Close provider detail') {
    const b = h('button', { class: 'icon-btn', type: 'button', 'aria-label': label }, icon('x'));
    b.addEventListener('click', () => { void requestCloseDrawer(); });
    return b;
  }

  async function renderDetail(body, p) {
    const health = deriveProviderHealth(p);

    body.replaceChildren();

    // --- summary ---
    // The heading already carries "(id)", so a dialect chip that only repeats the id is redundant.
    const dialect = String(p.dialect ?? '').trim();
    const showDialect = dialect !== '' && dialect.toLowerCase() !== String(p.id).toLowerCase();
    const meta = h('div', { style: 'display:flex;gap:6px;flex-wrap:wrap;margin-bottom:8px;' },
      showDialect ? chip(dialect) : null,
      chip(health.label, health.cls, `health: ${health.key}`),
      p.enabled === false ? chip('provider disabled', 'chip-neutral', 'intentionally off — not an error') : null,
      p.models_synced_at ? chip(`models synced ${formatAge(p.models_synced_at)}`) : null);
    body.append(meta);

    // --- actions ---
    const syncBtn = h('button', { class: 'btn', type: 'button' }, icon('sync'), ' Sync models');
    const editBtn = h('button', { class: 'btn', type: 'button' }, 'Edit');
    const enableBtn = h('button', { class: 'btn', type: 'button' }, p.enabled === false ? 'Enable provider' : 'Disable provider');
    const delBtn = h('button', { class: 'btn btn-danger', type: 'button' }, 'Delete provider');
    const actions = h('div', { class: 'toolbar', style: 'margin-top:10px;' }, syncBtn, editBtn, enableBtn, delBtn);
    body.append(actions);

    syncBtn.addEventListener('click', async () => {
      if (syncBtn.disabled) return; // no double submit while a sync is running
      syncBtn.disabled = true;
      syncBtn.replaceChildren(icon('sync'), ' Syncing…');
      syncBtn.querySelector('svg')?.classList.add('spin');
      try {
        await api.post(`/api/providers/${encodeURIComponent(p.id)}/models/sync`);
        ctx.toast.success(`Model sync for ${p.id} finished.`);
        await reload(true);
      } catch (err) {
        syncBtn.disabled = false;
        syncBtn.replaceChildren(icon('sync'), ' Sync models');
        ctx.toast.error(`Model sync for ${p.id} failed`, err);
      }
    });

    editBtn.addEventListener('click', () => { void openFormDrawer(p); });

    enableBtn.addEventListener('click', async () => {
      enableBtn.disabled = true;
      try {
        // Spread the server object so fields this form does not display survive the round trip.
        await api.put(`/api/providers/${encodeURIComponent(p.id)}`, { ...p, enabled: p.enabled === false });
        ctx.toast.success(`Provider ${p.id} ${p.enabled === false ? 'enabled' : 'disabled'}.`);
        await reload(true);
      } catch (err) {
        enableBtn.disabled = false;
        ctx.toast.error('Could not update the provider', err);
      }
    });

    delBtn.addEventListener('click', async () => {
      const accounts = Array.isArray(p.accounts) ? p.accounts : [];
      const warns = accounts.length
        ? [`This provider has ${accounts.length} account${accounts.length === 1 ? '' : 's'}. Deleting it removes their stored credentials too — they cannot be recovered from the panel.`]
        : [];
      // Even with the warning preference enabled, this handler only runs after the explicit Delete
      // click. The trade-off is one fewer safeguard against an accidental click, not auto-deletion.
      if (!deleteProviderWarningDisabled()) {
        const ok = await ctx.confirm({
          title: `Delete provider “${p.id}”?`,
          message: 'Combos and aliases referencing its models will stop resolving.',
          warnLines: warns,
          confirmLabel: 'Delete provider',
          danger: true,
        });
        if (!ok) return;
      }
      try {
        await api.del(`/api/providers/${encodeURIComponent(p.id)}`);
        ctx.toast.success(`Provider ${p.id} deleted.`);
        closeDrawer();
        await reload(true);
      } catch (err) {
        ctx.toast.error('Delete failed', err);
      }
    });

    // --- accounts ---
    await loadQuota();
    const accounts = Array.isArray(p.accounts) ? p.accounts : [];
    const accSection = h('section', {}, h('h3', {}, `Accounts (${accounts.length})`));
    if (accounts.length) {
      const accTb = h('tbody');
      for (const a of accounts) {
        const s = accountState(a);
        const quota = quotaCache.get(`${p.id}|${a.id}`);
        const quotaChip = quota && quota.windows && quota.windows.length
          ? quotaChipFrom(quota.windows)
          : null;
        const toggle = h('button', {
          class: 'btn btn-sm', type: 'button',
          'aria-label': `${a.enabled === false ? 'Enable' : 'Disable'} account ${a.id ?? ''}`,
        }, a.enabled === false ? 'Enable' : 'Disable');
        toggle.addEventListener('click', () => toggleAccount(p, a, toggle));
        const del = h('button', {
          class: 'icon-btn danger', type: 'button', 'aria-label': `Delete account ${a.id ?? ''}`,
        }, icon('trash'));
        del.addEventListener('click', () => deleteAccount(p, a));



        accTb.append(h('tr', {},
          h('td', { class: 'mono' }, a.id ?? ''),
          h('td', {}, a.label ?? a.identity ?? ''),
          buildInlineAccountTags(p, a),
          h('td', {}, chip(s.label, s.cls, `state: ${s.key}`), quotaChip),
          buildInlineAccountNumber(p, a, 'priority', 'priority'),
          buildInlineAccountNumber(p, a, 'weight', 'weight'),
          buildInlineAccountCredit(p, a),
          h('td', { class: 'actions' }, toggle, buildInlineAccountPlaceholders(p, a), buildInlineAccountSecret(p, a), del)));
      }
      accSection.append(h('div', { class: 'table-scroll provider-accounts-scroll' },
        h('table', { class: 'tbl' },
          h('thead', {}, h('tr', {},
            h('th', { scope: 'col' }, 'ID'), h('th', { scope: 'col' }, 'Label'),
            h('th', { scope: 'col', title: 'Free-text notes. A tag that names a person in Settings sends this account through that person\u2019s proxy.' }, 'Tags'),
            h('th', { scope: 'col' }, 'State'), h('th', { scope: 'col' }, 'Pri'),
            h('th', { scope: 'col' }, 'Weight'),
            h('th', { scope: 'col', title: 'Known credit in US dollars; a fine-tuning job is only started on an account with more than $15' }, 'Credit'),
            h('th', { scope: 'col' }, ''))),
          accTb)));
    } else {
      accSection.append(emptyState('No accounts.',
        p.auth?.type === 'oauth'
          ? 'Connect an account below to let this provider serve requests.'
          : 'Add an account to give this provider credentials.'));
    }
    accSection.append(buildAccountForm(p, () => afterAccountConnected(p.id)));
    body.append(accSection);

    // --- models ---
    const models = Array.isArray(p.models) ? p.models : [];
    const testAllBtn = h('button', { class: 'btn btn-sm btn-test-all-models', type: 'button' }, 'Test all models');
    const hideFailedToggle = toggle('Hide failed models', {
      id: `provider-${p.id}-hide-failed`,
      help: 'Automatically hide any model that fails the test from the relay listing.',
    });
    hideFailedToggle.row.classList.add('provider-models-hide-toggle');

    const searchInput = h('input', {
      class: 'input mono provider-models-search',
      type: 'search',
      placeholder: 'Search models…',
      'aria-label': 'Search models for ' + (p.label || p.id),
      autocomplete: 'off',
      spellcheck: 'false',
    });

    const modelMatchCount = h('span', { class: 'hint provider-models-match-count mono' });

    const modelHead = h('div', { class: 'provider-models-head' },
      h('div', { class: 'provider-models-title-wrap' },
        h('h3', {}, `Models (${models.length})`),
        modelMatchCount,
      ),
      models.length ? h('div', { class: 'provider-models-actions' }, searchInput, hideFailedToggle.row, testAllBtn) : null,
    );
    const modelSection = h('section', {}, modelHead);
    let modelTb = null;
    if (models.length) {
      modelTb = h('tbody');
      let testingProviderModels = false;
      testAllBtn.addEventListener('click', async () => {
        if (testingProviderModels) {
          testingProviderModels = false;
          testAllBtn.textContent = 'Stopping…';
          return;
        }

        const ok = await ctx.confirm({
          title: `Test all ${models.length} models for ${p.label || p.id}?`,
          message: `This will send test prompts to ${models.length} models and may consume quota. ${hideFailedToggle.input.checked ? 'Failed models will be automatically hidden.' : ''}`,
          confirmLabel: 'Start testing',
          cancelLabel: 'Cancel',
          danger: false,
        });
        if (!ok) return;

        testingProviderModels = true;
        testAllBtn.classList.add('btn-danger');

        let tested = 0;
        let passed = 0;
        let failed = 0;
        let hidden = 0;

        for (const m of models) {
          if (!testingProviderModels) break;

          testAllBtn.textContent = `Stop (${tested + 1}/${models.length} · ${passed} ok · ${failed} fail)`;

          const rowBtn = modelTb.querySelector(`tr[data-model-id="${CSS.escape(m.id)}"] .btn-test`);
          const label = rowBtn?.querySelector('span');
          if (label) label.textContent = 'Testing…';

          let success = false;
          try {
            await api.post('/api/models/test', {
              provider: p.id,
              model: m.id,
              prompt: 'Reply with the single word OK.',
            }, { skipAuthRedirect: true });
            success = true;
            passed += 1;
            if (label) label.textContent = 'OK';
          } catch {
            success = false;
            failed += 1;
            if (label) label.textContent = 'failed';
          }

          if (!success && hideFailedToggle.input.checked && !m.hidden) {
            try {
              await api.post(`/api/providers/${encodeURIComponent(p.id)}/models`, { ...m, hidden: true });
              m.hidden = true;
              hidden += 1;
              const toggleBtn = modelTb.querySelector(`tr[data-model-id="${CSS.escape(m.id)}"] button[aria-label*="model ${m.id}"]`);
              if (toggleBtn) toggleBtn.textContent = 'Show';
              const idCell = modelTb.querySelector(`tr[data-model-id="${CSS.escape(m.id)}"] td:first-child`);
              if (idCell && !idCell.querySelector('.chip')) {
                idCell.append(h('div', {}, chip('hidden')));
              }
            } catch {
              // quiet
            }
          }

          tested += 1;
        }

        testingProviderModels = false;
        testAllBtn.classList.remove('btn-danger');
        testAllBtn.textContent = 'Test all models';

        ctx.toast.success(`Tested ${tested} models for ${p.id}: ${passed} passed, ${failed} failed${hidden ? ` (${hidden} hidden)` : ''}.`);
        if (hidden > 0) {
          await reload(true);
        }
      });

      for (const m of models) {
        const toggle = h('button', {
          class: 'btn btn-sm', type: 'button',
          'aria-label': `${m.hidden ? 'Show' : 'Hide'} model ${m.id}`,
        }, m.hidden ? 'Show' : 'Hide');
        toggle.addEventListener('click', () => toggleModelHidden(p, m, toggle));
        const del = h('button', {
          class: 'icon-btn danger', type: 'button', 'aria-label': `Delete model ${m.id}`,
        }, icon('trash'));
        del.addEventListener('click', () => deleteModel(p, m));
        const test = h('button', {
          class: 'btn btn-sm btn-test', type: 'button',
          'aria-label': `Test model ${m.id}`, title: 'Send one tiny completion through the router',
        }, icon('play'), h('span', {}, 'Test'));
        test.addEventListener('click', () => testModel(p, m, test));

        const editorRow = buildModelEditor(p, m);
        editorRow.hidden = true;
        const edit = h('button', {
          class: 'btn btn-sm', type: 'button',
          'aria-label': `Edit model ${m.id}`, 'aria-expanded': 'false', 'data-model-edit': m.id,
        }, 'Edit');
        edit.addEventListener('click', () => {
          const opening = editorRow.hidden;
          for (const row of modelTb.querySelectorAll('.model-editor-row')) row.hidden = true;
          for (const btn of modelTb.querySelectorAll('[data-model-edit]')) btn.setAttribute('aria-expanded', 'false');
          editorRow.hidden = !opening;
          edit.setAttribute('aria-expanded', opening ? 'true' : 'false');
          if (opening) editorRow.querySelector('input')?.focus();
        });

        modelTb.append(h('tr', { dataset: { modelId: m.id } },
          h('td', {},
            h('span', { class: 'mono model-id' }, m.id, ' ', copyButton(m.id, { className: 'btn btn-sm icon-btn', title: 'Copy model ID' })),
            m.label ? h('div', { class: 'hint' }, m.label) : null,
            m.hidden ? h('div', {}, chip('hidden')) : null),
          h('td', {}, chip(m.source ?? 'synced', 'chip-neutral', 'source')),
          h('td', { class: 'num' }, m.context_length != null ? formatCompact(Number(m.context_length)) : '—'),
          h('td', { class: 'num model-price' }, modelPriceShort(m)),
          h('td', {}, h('span', { class: 'mono' }, m.default_reasoning_level ?? 'inherit')),
          h('td', { class: 'actions' }, test, edit, toggle, del)));
        modelTb.append(editorRow);
      }
      modelSection.append(h('div', { class: 'table-scroll model-table-scroll' },
        h('table', { class: 'tbl model-table' },
          h('thead', {}, h('tr', {},
            h('th', { scope: 'col' }, 'Model'), h('th', { scope: 'col' }, 'Source'),
            h('th', { scope: 'col', class: 'num' }, 'Ctx'),
            h('th', { scope: 'col', class: 'num' }, 'Price / Mtok'),
            h('th', { scope: 'col' }, 'Default level'),
            h('th', { scope: 'col' }, ''))),
          modelTb)));
    } else {
      modelSection.append(emptyState('No models yet.', 'Run “Sync models” or add one manually below.'));
    }

      function filterProviderModels(query) {
        const q = String(query ?? '').trim().toLowerCase();
        let matched = 0;
        for (const m of models) {
          const idMatches = m.id.toLowerCase().includes(q);
          const labelMatches = (m.label || '').toLowerCase().includes(q);
          const visible = !q || idMatches || labelMatches;
          const tr = modelTb ? modelTb.querySelector(`tr[data-model-id="${CSS.escape(m.id)}"]`) : null;
          if (tr) tr.hidden = !visible;
          const editorRow = tr?.nextElementSibling?.classList.contains('model-editor-row') ? tr.nextElementSibling : null;
          if (editorRow && !visible) editorRow.hidden = true;
          if (visible) matched += 1;
        }
        if (q) {
          modelMatchCount.textContent = `· ${matched} of ${models.length} matching`;
        } else {
          modelMatchCount.textContent = '';
        }
      }
      searchInput.addEventListener('input', () => filterProviderModels(searchInput.value));

    modelSection.append(buildModelForm(p));
    body.append(modelSection);

    // Base URL, capabilities, auth and note follow Models so the provider's main content stays
    // easy to scan.
    const declaredCapabilities = Array.isArray(p.capabilities) && p.capabilities.length
      ? p.capabilities
      : DEFAULT_CAPABILITIES;
    body.append(h('section', {}, h('h3', {}, 'Connection details'), kv([
      ['base URL', h('span', { class: 'mono' }, p.base_url ?? '—')],
      ['capabilities', h('span', { class: 'mono' }, declaredCapabilities.join(', '))],
      ['auth', authLabel(p.auth)],
      ['key strategy', h('span', { class: 'mono' }, p.key_strategy ?? 'priority')],
      ['cooldown scope', h('span', { class: 'mono' }, p.cooldown_scope ?? 'account')],
      ['KV-token pool', p.kv_pool_tokens == null ? '—' : formatCompact(Number(p.kv_pool_tokens))],
      ['declared context', p.max_context == null ? '—' : formatCompact(Number(p.max_context))],
      ['max concurrent', p.max_concurrent == null ? '—' : String(p.max_concurrent)],
      ['GPU price', p.price_per_second == null ? '—' : `$${Number(p.price_per_second).toFixed(4)} / second`],
      ['Cold-start TTFB timeout', p.cold_start_ttfb_timeout_ms == null ? '—' : `${formatCompact(Number(p.cold_start_ttfb_timeout_ms))} ms`],
      ['note', p.note ?? '—'],
    ])));

    // --- timeouts / headers / overrides ---
    const timeoutSection = h('section', {}, h('h3', {}, 'Timeouts'));
    const timeoutFields = [
      ['connect_timeout_ms', 'connect'], ['ttfb_timeout_ms', 'TTFB'],
      ['idle_timeout_ms', 'idle'], ['total_timeout_ms', 'total'],
      ['cold_start_ttfb_timeout_ms', 'cold-start TTFB'],
    ];
    for (const [key, label] of timeoutFields) timeoutSection.append(buildInlineNumber(p, key, label, 'Provider timeout in milliseconds.'));
    timeoutSection.append(h('p', { class: 'hint' }, 'Blank values inherit the configured timeout defaults.'));
    const prewarmState = chip(p.prewarm ? 'on' : 'off', 'chip-neutral', 'per-provider prewarm override');
    timeoutSection.append(kv([
      ['prewarm override', h('span', {}, prewarmState, ' ', h('span', { class: 'hint' }, 'Global latency.prewarm also applies server-side.'))],
    ]));
    body.append(
      timeoutSection,
      buildKvEditor('Headers', p.headers ?? {}, (next) => updateProviderField(p, { headers: next })),
      buildKvEditor('Route overrides', p.route_overrides ?? {}, (next) => updateProviderField(p, { route_overrides: next })),
    );
  }

  function modelPricing(model) {
    const pricing = model.pricing ?? {};
    const input = pricing.input_per_mtok ?? model.input_price_per_mtok;
    const output = pricing.output_per_mtok ?? model.output_price_per_mtok;
    if (input == null && output == null) return null;
    const rate = (value) => value == null ? '—' : formatCompact(Number(value));
    return `$${rate(input)} / $${rate(output)} / 1M in/out`;
  }

  function modelPriceShort(model) {
    const pricing = model.pricing ?? {};
    const input = pricing.input_per_mtok ?? model.input_price_per_mtok;
    const output = pricing.output_per_mtok ?? model.output_price_per_mtok;
    if (input == null && output == null) return h('span', { class: 'hint' }, '—');
    const rate = (value) => value == null ? '—' : '$' + formatCompact(Number(value));
    return h('span', { title: modelPricing(model) ?? '' }, rate(input) + ' / ' + rate(output));
  }

  /// Full-width editor for one model, opened under its row.
  ///
  /// It saves once through the same endpoint the form uses, with a full copy of the model plus
  /// the edited fields. The previous design wrote every keystroke-sized change through its own
  /// little request, which made a typo a config write and made the table eight columns of live
  /// inputs. Unknown fields on the model are carried through untouched.
  function buildModelEditor(p, m) {
    const tr = h('tr', { class: 'model-editor-row' });
    const td = h('td', { colspan: '6' });
    tr.append(td);

    const labelInput = h('input', { class: 'input', value: m.label ?? '', placeholder: m.id });
    const contextInput = h('input', {
      class: 'input', type: 'number', min: '0', step: '1',
      value: m.context_length ?? '', placeholder: 'inherit / unknown',
    });
    const outputInput = h('input', {
      class: 'input', type: 'number', min: '0', step: '1',
      value: m.max_output_tokens ?? '', placeholder: 'provider default',
    });
    const oldInputPrice = m.pricing?.input_per_mtok ?? m.input_price_per_mtok ?? null;
    const oldOutputPrice = m.pricing?.output_per_mtok ?? m.output_price_per_mtok ?? null;
    const inputPrice = h('input', {
      class: 'input', type: 'number', min: '0', step: 'any',
      value: oldInputPrice ?? '', placeholder: 'unknown',
    });
    const outputPrice = h('input', {
      class: 'input', type: 'number', min: '0', step: 'any',
      value: oldOutputPrice ?? '', placeholder: 'unknown',
    });

    const levels = providerLevels(p, m);
    const chosenLevels = new Set(declaredLevels(m));
    const levelRow = h('div', { class: 'model-chip-row' });
    for (const level of levels) {
      const btn = h('button', {
        class: 'chip model-editor-chip' + (chosenLevels.has(level) ? ' active' : ''),
        type: 'button', 'aria-pressed': chosenLevels.has(level) ? 'true' : 'false',
      }, String(level));
      btn.addEventListener('click', () => {
        if (chosenLevels.has(level)) chosenLevels.delete(level); else chosenLevels.add(level);
        const on = chosenLevels.has(level);
        btn.classList.toggle('active', on);
        btn.setAttribute('aria-pressed', on ? 'true' : 'false');
      });
      levelRow.append(btn);
    }

    const defaultSel = h('select', { class: 'input' }, h('option', { value: '' }, 'Inherit global'));
    for (const level of levels) defaultSel.append(h('option', { value: String(level) }, String(level)));
    const currentDefault = m.default_reasoning_level == null ? '' : String(m.default_reasoning_level);
    if (currentDefault && !levels.some((level) => String(level) === currentDefault)) {
      defaultSel.append(h('option', { value: currentDefault }, currentDefault + ' (current)'));
    }
    defaultSel.value = currentDefault;

    const chosenModalities = new Set(Array.isArray(m.input_modalities) ? m.input_modalities.map(String) : []);
    const modalityRow = h('div', { class: 'model-chip-row' });
    for (const modality of INPUT_MODALITIES) {
      const btn = h('button', {
        class: 'chip model-editor-chip' + (chosenModalities.has(modality) ? ' active' : ''),
        type: 'button', 'aria-pressed': chosenModalities.has(modality) ? 'true' : 'false',
      }, modality);
      btn.addEventListener('click', () => {
        if (chosenModalities.has(modality)) chosenModalities.delete(modality); else chosenModalities.add(modality);
        const on = chosenModalities.has(modality);
        btn.classList.toggle('active', on);
        btn.setAttribute('aria-pressed', on ? 'true' : 'false');
      });
      modalityRow.append(btn);
    }

    const save = h('button', { class: 'btn btn-primary', type: 'button' }, 'Save model');
    const cancel = h('button', { class: 'btn', type: 'button' }, 'Cancel');
    const closeEditor = () => {
      tr.hidden = true;
      tr.previousElementSibling?.querySelector('[data-model-edit]')?.setAttribute('aria-expanded', 'false');
    };
    cancel.addEventListener('click', closeEditor);

    function readNumber(input, label, { integer = false, max = null } = {}) {
      const raw = input.value.trim();
      if (!raw) return { ok: true, value: null };
      const value = Number(raw);
      const invalid = !Number.isFinite(value) || value < 0 || (integer && !Number.isInteger(value))
        || (max != null && value > max);
      if (invalid) {
        ctx.toast.warning(label + ' must be ' + (integer ? 'a whole number' : 'a number') + ' of 0 or more.');
        input.focus();
        return { ok: false, value: null };
      }
      return { ok: true, value };
    }

    function put(target, key, value) {
      if (value === null || value === undefined) delete target[key];
      else target[key] = value;
    }

    save.addEventListener('click', async () => {
      if (save.disabled) return;
      const context = readNumber(contextInput, 'Context length', { integer: true, max: 4294967295 });
      if (!context.ok) return;
      const maxOutput = readNumber(outputInput, 'Maximum output tokens', { integer: true, max: 4294967295 });
      if (!maxOutput.ok) return;
      const inPrice = readNumber(inputPrice, 'Input price');
      if (!inPrice.ok) return;
      const outPrice = readNumber(outputPrice, 'Output price');
      if (!outPrice.ok) return;

      const next = { ...m };
      const label = labelInput.value.trim();
      if (label) next.label = label; else delete next.label;
      put(next, 'context_length', context.value);
      put(next, 'max_output_tokens', maxOutput.value);
      put(next, 'default_reasoning_level', defaultSel.value || null);

      const orderedLevels = levels.filter((level) => chosenLevels.has(level));
      if (orderedLevels.length) next.reasoning_levels = orderedLevels; else delete next.reasoning_levels;
      const orderedModalities = INPUT_MODALITIES.filter((modality) => chosenModalities.has(modality));
      if (orderedModalities.length) next.input_modalities = orderedModalities; else delete next.input_modalities;

      const samePrice = (input, old) => {
        const typed = input.value.trim() === '' ? null : Number(input.value);
        const previous = old == null ? null : Number(old);
        return typed === previous;
      };
      if (!samePrice(inputPrice, oldInputPrice) || !samePrice(outputPrice, oldOutputPrice)) {
        put(next, 'input_price_per_mtok', inPrice.value);
        put(next, 'output_price_per_mtok', outPrice.value);
        next.pricing = {
          ...(m.pricing ?? {}),
          input_per_mtok: inPrice.value,
          output_per_mtok: outPrice.value,
          source: 'manual',
        };
        if (inPrice.value === null && outPrice.value === null && !m.pricing) delete next.pricing;
      }

      save.disabled = true;
      const before = save.textContent;
      save.textContent = 'Saving…';
      try {
        await api.post('/api/providers/' + encodeURIComponent(p.id) + '/models', next);
        ctx.toast.success('Saved ' + m.id + '.');
        await reload(true);
      } catch (err) {
        ctx.toast.error(err?.message ?? String(err));
        save.disabled = false;
        save.textContent = before;
      }
    });

    td.append(h('div', { class: 'model-editor' },
      h('div', { class: 'model-editor-title' },
        h('strong', {}, 'Edit model'),
        h('span', { class: 'mono' }, m.id)),
      h('div', { class: 'model-field-grid' },
        field('Display label', labelInput, { hint: 'Falls back to the model id.' }),
        field('Default reasoning level', defaultSel, { hint: 'Requests without a level suffix use this.' }),
        field('Context length', contextInput, { hint: 'Tokens. Blank inherits discovery metadata.' }),
        field('Maximum output tokens', outputInput, { hint: 'Blank means the provider default.' }),
        field('Input price / Mtok', inputPrice, { hint: 'USD per million input tokens.' }),
        field('Output price / Mtok', outputPrice, { hint: 'USD per million output tokens.' })),
      h('div', { class: 'model-editor-group' },
        h('span', { class: 'model-editor-label' }, 'Reasoning levels'),
        levelRow,
        h('p', { class: 'hint' }, 'None selected means automatic: derive from the model catalogue and dialect.')),
      h('div', { class: 'model-editor-group' },
        h('span', { class: 'model-editor-label' }, 'Input modalities'),
        modalityRow,
        h('p', { class: 'hint' }, 'None selected means automatic. Empty is normal.')),
      h('div', { class: 'model-editor-actions' }, cancel, save)));
    return tr;
  }

  /// Send one small request to this model and report what came back.
  ///
  /// The panel test speaks the OpenAI shape, which is what most providers accept unchanged. The
  /// Test one model with a tiny completion, through the relay's loopback probe.
  ///
  /// The probe runs the ordinary dispatch path, so this is a real end-to-end check for every
  /// dialect — not only the OpenAI shape — and it fails for the reason a real request would fail
  /// (a dead credential, a cooling account, a model the account does not offer). The button keeps
  /// the verdict beside it so several models can be compared without chasing toasts.
  async function testModel(p, m, button) {
    if (button.disabled) return; // one test per model at a time
    const label = button.querySelector('span');
    button.disabled = true;
    if (label) label.textContent = 'Testing…';
    const started = performance.now();
    try {
      // `skipAuthRedirect`: the provider's own 401 is forwarded as-is, and treating it as a dead
      // panel session would sign the operator out over a credential the login form cannot fix.
      const result = await api.post('/api/models/test', {
        provider: p.id,
        model: m.id,
        prompt: 'Reply with the single word OK.',
      }, { skipAuthRedirect: true });
      const ms = Math.round(performance.now() - started);
      const answer = typeof result?.response === 'string' ? result.response.trim() : '';
      if (label) label.textContent = 'OK';
      button.title = answer
        ? `Answered in ${ms} ms: ${answer.slice(0, 200)}`
        : `Answered with no text in ${ms} ms`;
      // The answer goes in the message: a toast's second argument is an error's detail list, not
      // free text, so anything passed there would simply vanish.
      ctx.toast.success(answer
        ? `${p.id} / ${m.id} answered: “${answer.slice(0, 120)}”`
        : `${p.id} / ${m.id} answered with no text.`);
    } catch (err) {
      // The reason belongs in the message: a toast's second argument is the error's detail *list*
      // and is dropped when it is not one, so `err` passed there vanished and the operator was left
      // with "did not answer" and no cause.
      if (label) label.textContent = 'failed';
      button.title = err?.message ?? 'the model did not answer';
      ctx.toast.error(`${p.id} / ${m.id} did not answer: ${err?.message ?? 'the request failed'}`);
    } finally {
      // The verdict stays until the next test, so the button keeps telling the truth rather than
      // resetting to a neutral label that hides what just happened.
      button.disabled = false;
    }
  }

  async function updateProviderField(p, patch, errorTitle = 'Could not update the provider') {
    try {
      await api.put(`/api/providers/${encodeURIComponent(p.id)}`, { ...p, ...patch });
      await reload(true);
      return true;
    } catch (err) {
      ctx.toast.error(errorTitle, err);
      return false;
    }
  }

  /// Replace an existing account's credential without deleting and re-adding it.
  ///
  /// A key rotation is the normal case (a leaked key, a new quota period), and until now the only
  /// way to do it in the panel was delete + add, which lost the account's id, priority, weight and
  /// quota history. The new secret goes through the same server-side vault path as a create: it is
  /// sent once, extracted to the vault, and answered with a `secret_ref` — never echoed back.
  ///
  /// The editor opens as a full-width row underneath the account rather than inside the actions
  /// cell: the drawer is ~490px wide and six columns plus an input pushed the table into a
  /// horizontal scrollbar that squeezed the id column to one character per line.
  function buildInlineAccountSecret(p, account) {
    // An icon button, not a labelled one: the account table is already crowded. The label lives in
    // aria-label/title, which is what a screen reader announces.
    const edit = h('button', {
      class: 'icon-btn', type: 'button',
      'aria-label': `Replace the API key for account ${account.id}`,
      title: 'Replace API key',
    }, icon('key'));

    edit.addEventListener('click', () => {
      const row = edit.closest('tr');
      if (!row) return;
      // Only one editor at a time: a second click must not stack rows.
      row.nextElementSibling?.classList.contains('inline-editor') && row.nextElementSibling.remove();

      const input = h('input', {
        class: 'input mono', type: 'password', autocomplete: 'new-password',
        placeholder: 'new API key', 'aria-label': 'New API key', style: 'width:220px',
      });
      const save = h('button', { class: 'btn btn-sm btn-primary', type: 'button' }, 'Save');
      const cancel = h('button', { class: 'btn btn-sm', type: 'button' }, 'Cancel');
      const editorRow = h('tr', { class: 'inline-editor' },
        h('td', { colspan: '6' },
          h('div', { style: 'display:flex;gap:6px;align-items:center;flex-wrap:wrap;' },
            h('span', { class: 'hint' }, `New key for ${account.label ?? account.id}:`),
            input, save, cancel)));
      row.after(editorRow);
      input.focus();
      const close = () => editorRow.remove();
      cancel.addEventListener('click', close);
      input.addEventListener('keydown', (e) => {
        if (e.key === 'Escape') close();
        if (e.key === 'Enter') save.click();
      });
      save.addEventListener('click', async () => {
        const key = input.value.trim();
        if (!key) {
          ctx.toast.warning('Enter the new API key.');
          return;
        }
        save.disabled = true;
        save.textContent = 'Saving…';
        try {
          // Field names are the Account struct's; the server extracts `secret` into the vault and
          // answers with a secret_ref, exactly as a create does.
          await api.put(`/api/providers/${encodeURIComponent(p.id)}/accounts/${encodeURIComponent(account.id)}`,
            { ...account, secret: key });
          // The plaintext must not outlive the request.
          input.value = '';
          ctx.toast.success('Key replaced.');
          await reload(true);
        } catch (err) {
          save.disabled = false;
          save.textContent = 'Save';
          ctx.toast.error('Could not replace the key', err);
        }
      });
    });
    return edit;
  }

  /// Edit the values a templated base URL needs, one account at a time.
  ///
  /// Two shipped providers cannot spell their endpoint without an account-owned value
  /// (`cloudflare-ai` addresses `…/accounts/{account_id}/…`, Snowflake puts `{account}` in the
  /// hostname), and until now the only way to supply one was to hand-edit `config.json`. The editor
  /// is hidden entirely for a provider whose URL has no placeholders and whose account has none, so
  /// the crowded actions cell does not grow a button that would do nothing.
  function buildInlineAccountPlaceholders(p, account) {
    const required = placeholderNamesIn(p.base_url);
    const existing = account.placeholders && typeof account.placeholders === 'object'
      ? Object.entries(account.placeholders).filter(([name, value]) => typeof name === 'string' && typeof value === 'string')
      : [];
    if (required.length === 0 && existing.length === 0) return null;

    const label = account.label || account.id;
    const button = h('button', {
      class: 'icon-btn', type: 'button',
      'aria-label': `Edit URL placeholders for account ${account.id}`,
      title: required.length
        ? `URL placeholders (this endpoint needs ${required.map((n) => `{${n}}`).join(', ')})`
        : 'URL placeholders',
    }, icon('link'));

    button.addEventListener('click', () => {
      const row = button.closest('tr');
      if (!row) return;
      row.nextElementSibling?.classList.contains('inline-editor') && row.nextElementSibling.remove();

      const rows = h('div', { style: 'display:flex;flex-direction:column;gap:6px;' });
      const missing = h('p', { class: 'hint', style: 'color:var(--amber,#c98a2b);display:none' });
      const problem = h('p', { class: 'hint', style: 'color:var(--danger,#c0392b);display:none' });

      const addRow = (name = '', value = '') => {
        const nameInput = h('input', {
          class: 'input mono', type: 'text', value: name, placeholder: 'account_id',
          'aria-label': 'Placeholder name', style: 'width:160px',
        });
        const valueInput = h('input', {
          class: 'input mono', type: 'text', value: value, placeholder: 'value used in the URL',
          'aria-label': `Value for {${name || 'the placeholder'}}`, style: 'width:240px',
        });
        const remove = h('button', {
          class: 'icon-btn danger', type: 'button', 'aria-label': 'Remove this placeholder',
          title: 'Remove',
        }, icon('trash'));
        const entry = h('div', { style: 'display:flex;gap:6px;align-items:center;flex-wrap:wrap;' },
          nameInput, h('span', { class: 'hint' }, '='), valueInput, remove);
        remove.addEventListener('click', () => { entry.remove(); refreshMissing(); });
        nameInput.addEventListener('input', refreshMissing);
        rows.append(entry);
        return nameInput;
      };

      // Which of the endpoint's names still have no value. Shown while editing rather than only on
      // save: a staged account is legitimate, but a request against one answers 502, so the gap is
      // visible before it costs a round trip.
      const refreshMissing = () => {
        const names = new Set();
        for (const entry of rows.children) {
          const [nameInput, , valueInput] = entry.querySelectorAll('input');
          if (!nameInput || !valueInput) continue;
          const name = nameInput.value.trim();
          if (name && PLACEHOLDER_VALUE.test(valueInput.value.trim())) names.add(name);
        }
        const absent = required.filter((n) => !names.has(n));
        if (absent.length === 0) { missing.style.display = 'none'; missing.textContent = ''; return; }
        missing.style.display = '';
        missing.textContent = `Still missing: ${absent.map((n) => `{${n}}`).join(', ')}. The endpoint answers 502 until ${absent.length === 1 ? 'it has a value' : 'they have values'}.`;
      };

      for (const [name, value] of existing) addRow(name, value);
      if (existing.length === 0) addRow(required[0] ?? '', '');
      refreshMissing();

      const add = h('button', { class: 'btn btn-sm', type: 'button' }, icon('plus'), 'Add placeholder');
      add.addEventListener('click', () => addRow().focus());
      const save = h('button', { class: 'btn btn-sm btn-primary', type: 'button' }, 'Save');
      const cancel = h('button', { class: 'btn btn-sm', type: 'button' }, 'Cancel');

      const editorRow = h('tr', { class: 'inline-editor' },
        h('td', { colspan: '7' },
          h('div', { style: 'display:flex;flex-direction:column;gap:8px;' },
            h('p', { class: 'hint' },
              `Placeholders for ${label}. A base URL may name a value this account owns, such as `,
              h('span', { class: 'mono' }, '{account_id}'), '; the value is spliced into the URL, so only letters, digits, ',
              h('span', { class: 'mono' }, '- . _ ~'), ' are accepted.'),
            rows, missing, problem,
            h('div', { style: 'display:flex;gap:6px;align-items:center;flex-wrap:wrap;' },
              add, h('span', { class: 'spacer' }), save, cancel))));

      row.after(editorRow);
      const close = () => editorRow.remove();
      cancel.addEventListener('click', close);
      editorRow.addEventListener('keydown', (e) => { if (e.key === 'Escape') close(); });

      save.addEventListener('click', async () => {
        const next = {};
        const problems = [];
        for (const entry of rows.children) {
          const [nameInput, , valueInput] = entry.querySelectorAll('input');
          if (!nameInput || !valueInput) continue;
          const name = nameInput.value.trim();
          const value = valueInput.value.trim();
          // An untouched blank row is not an entry the operator asked to keep.
          if (!name && !value) continue;
          if (!PLACEHOLDER_NAME.test(name)) {
            problems.push(`"${name}" is not a usable name: start with a letter or underscore, then letters, digits or underscores, at most 64.`);
            continue;
          }
          if (Object.hasOwn(next, name)) {
            problems.push(`{${name}} is listed twice.`);
            continue;
          }
          if (!PLACEHOLDER_VALUE.test(value)) {
            problems.push(`{${name}} needs a value made of letters, digits, '-', '.', '_' or '~'.`);
            continue;
          }
          next[name] = value;
        }
        if (problems.length) {
          problem.style.display = '';
          problem.textContent = problems.join(' ');
          return;
        }
        save.disabled = true;
        save.textContent = 'Saving…';
        try {
          // Field names are the Account struct's, and the whole map is sent: a value this editor
          // cannot show is never silently dropped by the round trip.
          await api.put(`/api/providers/${encodeURIComponent(p.id)}/accounts/${encodeURIComponent(account.id)}`,
            { ...account, placeholders: next });
          const absent = required.filter((n) => !Object.hasOwn(next, n));
          if (absent.length) {
            ctx.toast.warning(`Placeholders saved, but ${absent.map((n) => `{${n}}`).join(', ')} ${absent.length === 1 ? 'is' : 'are'} still missing.`);
          } else {
            ctx.toast.success(Object.keys(next).length ? 'Placeholders saved.' : 'Placeholders cleared.');
          }
          await reload(true);
        } catch (err) {
          save.disabled = false;
          save.textContent = 'Save';
          ctx.toast.error('Could not save the placeholders', err);
        }
      });
    });
    return button;
  }

  function buildInlineAccountNumber(p, account, key, label) {
    const slot = `account:${p.id}:${account.id}:${key}`;
    return buildInlineNumericCell(account[key], label, slot, async (value) => {
      await api.put(`/api/providers/${encodeURIComponent(p.id)}/accounts/${encodeURIComponent(account.id)}`,
        { ...account, [key]: value });
      await reload(true);
    }, (err) => ctx.toast.error(`Could not update account ${label}`, err),
    { allowNegative: key === 'priority', iconOnly: true });
  }

  /// The account's **declared** credit, in US dollars.
  ///
  /// This is the only balance the panel can set: the cached provider figure lives in the quota
  /// view and is read-only. Dollars are decimal, so this cannot reuse the integer cell above;
  /// an empty field clears the declaration and returns the account to "unknown".
  function buildInlineAccountCredit(p, account) {
    const slot = `account:${p.id}:${account.id}:credit_usd`;
    const value = account.credit_usd;
    const cell = h('td', { class: 'num' });

    const show = (usd) => (usd == null ? '—' : `$${Number(usd).toFixed(2)}`);
    const renderView = () => {
      const edit = h('button', {
        class: 'icon-btn', type: 'button', 'aria-label': 'Edit declared credit in US dollars',
        title: 'Declared credit in US dollars',
      }, icon('pencil'));
      edit.addEventListener('click', () => renderEditor(inlineDrafts.get(slot) ?? (value == null ? '' : String(value))));
      cell.replaceChildren(h('span', {
        class: 'mono',
        title: value == null
          ? 'No declared credit. Fine-tuning needs more than $15 of known credit.'
          : 'The balance the operator declared for this account.',
      }, show(value)), edit);
    };

    function renderEditor(initial) {
      const input = h('input', {
        class: 'input', type: 'number', min: '0', step: '0.01', value: initial,
        'aria-label': 'Declared credit in US dollars', style: 'width:110px',
      });
      const save = h('button', { class: 'btn btn-sm btn-primary', type: 'button' }, 'Save');
      const clear = h('button', { class: 'btn btn-sm', type: 'button' }, 'Clear');
      const cancel = h('button', { class: 'btn btn-sm', type: 'button' }, 'Cancel');
      cell.replaceChildren(h('div', {
        style: 'display:flex;gap:4px;align-items:center;flex-wrap:wrap;min-width:200px;',
      }, input, save, clear, cancel));
      input.addEventListener('input', () => inlineDrafts.set(slot, input.value));
      const restore = () => { inlineDrafts.delete(slot); renderView(); };
      const persist = async (next) => {
        save.disabled = true;
        clear.disabled = true;
        save.textContent = 'Saving…';
        try {
          await api.put(`/api/providers/${encodeURIComponent(p.id)}/accounts/${encodeURIComponent(account.id)}`,
            { ...account, credit_usd: next });
          inlineDrafts.delete(slot);
          ctx.toast.success(next == null
            ? 'Declared credit cleared; the account reports no known balance.'
            : `Declared credit set to $${next.toFixed(2)}.`);
          await reload(true);
        } catch (err) {
          save.disabled = false;
          clear.disabled = false;
          save.textContent = 'Save';
          ctx.toast.error('Could not update the declared credit', err);
        }
      };
      save.addEventListener('click', () => {
        const raw = input.value.trim();
        if (!raw) {
          ctx.toast.warning('Enter a dollar amount, or use Clear to forget the declaration.');
          return;
        }
        const number = Number(raw);
        if (!Number.isFinite(number) || number < 0) {
          ctx.toast.warning('The declared credit must be a dollar amount, 0 or more.');
          return;
        }
        persist(Math.round(number * 100) / 100);
      });
      clear.addEventListener('click', () => persist(null));
      cancel.addEventListener('click', restore);
      input.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') save.click();
        if (e.key === 'Escape') restore();
      });
    }

    if (inlineDrafts.has(slot)) renderEditor(inlineDrafts.get(slot));
    else renderView();
    return cell;
  }

  /// The account's tags: a small free-text note, edited in place.
  ///
  /// A tag is not decoration. A tag that names a person in Settings → People is what sends this
  /// account out through that person's machine, and a combo step can name a tag instead of an
  /// account id — so this cell is the only place either of those facts is expressed. Chips keep
  /// several tags readable while the table is being scanned; the editor is one comma-separated
  /// field, the same shape `mr account tag` takes.
  function buildInlineAccountTags(p, account) {
    const slot = `account:${p.id}:${account.id}:tags`;
    const tags = Array.isArray(account.tags) ? account.tags.map(String) : [];
    const cell = h('td', { class: 'tags-cell' });
    const edit = h('button', {
      class: 'icon-btn', type: 'button',
      'aria-label': `Edit tags for account ${account.id ?? ''}`,
      title: 'Tags for this account',
    }, icon('pencil'));
    edit.addEventListener('click', () => renderEditor(inlineDrafts.get(slot) ?? tags.join(', ')));

    function renderView() {
      cell.replaceChildren(h('div', { class: 'tags-view' },
        ...(tags.length
          ? tags.map((tag) => chip(tag, 'chip-neutral', `tag: ${tag}`))
          : [h('span', { class: 'hint' }, 'no tags')]),
        edit));
    }

    function renderEditor(initial) {
      const input = h('input', {
        class: 'input mono', type: 'text', value: initial, placeholder: 'alice, work',
        'aria-label': `Tags for account ${account.id ?? ''}`, style: 'min-width:150px',
      });
      const save = h('button', { class: 'btn btn-sm btn-primary', type: 'button' }, 'Save');
      const cancel = h('button', { class: 'btn btn-sm', type: 'button' }, 'Cancel');
      cell.replaceChildren(h('div', { class: 'tags-edit' }, input, save, cancel));
      input.addEventListener('input', () => inlineDrafts.set(slot, input.value));
      const restore = () => { inlineDrafts.delete(slot); renderView(); };
      const persist = async () => {
        const next = parseTagList(input.value);
        save.disabled = true;
        save.textContent = 'Saving…';
        try {
          await api.put(`/api/providers/${encodeURIComponent(p.id)}/accounts/${encodeURIComponent(account.id)}`,
            { ...account, tags: next });
          inlineDrafts.delete(slot);
          ctx.toast.success(next.length ? 'Tags saved.' : 'Tags cleared.');
          await reload(true);
        } catch (err) {
          save.disabled = false;
          save.textContent = 'Save';
          ctx.toast.error('Could not save the account tags', err);
        }
      };
      save.addEventListener('click', persist);
      cancel.addEventListener('click', restore);
      input.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') { e.preventDefault(); persist(); }
        if (e.key === 'Escape') { e.preventDefault(); restore(); }
      });
      input.focus();
      input.select();
    }

    if (inlineDrafts.has(slot)) renderEditor(inlineDrafts.get(slot));
    else renderView();
    return cell;
  }

  function buildInlineModelNumber(p, model, key, label) {
    const slot = `model:${p.id}:${model.id}:${key}`;
    return buildInlineNumericCell(model[key], label, slot, async (value) => {
      await api.post(`/api/providers/${encodeURIComponent(p.id)}/models`, { ...model, [key]: value });
      await reload(true);
    }, (err) => ctx.toast.error(`Could not update model ${label}`, err), { compact: true });
  }

  function buildInlineNumericCell(value, label, slot, persist, reportError, { compact = false, suffix = '', allowNegative = false, iconOnly = false } = {}) {
    const cell = h('td', { class: compact || iconOnly ? 'num' : '' });
    const showValue = (n) => n == null ? '—' : `${compact ? formatCompact(Number(n)) : n}${suffix}`;
    // Two full "Edit" buttons per account row read as four identical actions. A quiet pencil keeps
    // the number as the thing being scanned; its aria-label and title still name the field.
    const edit = compact || iconOnly
      ? h('button', { class: 'icon-btn', type: 'button', 'aria-label': `Edit ${label}`, title: `Edit ${label}` }, icon('pencil'))
      : h('button', { class: 'btn btn-sm', type: 'button', 'aria-label': `Edit ${label}` }, 'Edit');
    const renderView = () => cell.replaceChildren(h('span', { class: 'mono' }, showValue(value)), edit);
    const draft = inlineDrafts.get(slot);
    if (draft !== undefined) renderEditor(draft);
    else renderView();

    function renderEditor(initial) {
      const input = h('input', { class: 'input', type: 'number', min: allowNegative ? '-2147483648' : '0', step: '1', value: initial, 'aria-label': label });
      const save = h('button', { class: 'btn btn-sm btn-primary', type: 'button' }, 'Save');
      const cancel = h('button', { class: 'btn btn-sm', type: 'button' }, 'Cancel');
      cell.replaceChildren(h('div', { style: 'display:flex;gap:4px;align-items:center;min-width:140px;' }, input, save, cancel));
      input.addEventListener('input', () => inlineDrafts.set(slot, input.value));
      const restore = () => {
        inlineDrafts.delete(slot);
        renderView();
      };
      const finish = async () => {
        if (!input.value.trim()) {
          ctx.toast.warning(`Enter a value for ${label}.`);
          return;
        }
        const number = Number(input.value);
        if (!Number.isSafeInteger(number) || (!allowNegative && number < 0) || (allowNegative && number < -2147483648) || number > (allowNegative ? 2147483647 : 4294967295)) {
          ctx.toast.warning(`${label} must be a whole number in the supported range.`);
          return;
        }
        inlineDrafts.set(slot, input.value);
        save.disabled = true;
        try {
          await persist(number);
          inlineDrafts.delete(slot);
          refreshInlineDrawer(p.id);
        } catch (err) {
          save.disabled = false;
          reportError(err);
        }
      };
      save.addEventListener('click', finish);
      input.addEventListener('keydown', (event) => {
        if (event.key === 'Enter') { event.preventDefault(); finish(); }
        if (event.key === 'Escape') { event.preventDefault(); restore(); }
      });
      cancel.addEventListener('click', restore);
      input.addEventListener('blur', () => {
        // Blur keeps a valid draft for later editing; save is explicit to avoid losing intent.
        inlineDrafts.set(slot, input.value);
      });
    }

    edit.addEventListener('click', () => {
      renderEditor(inlineDrafts.get(slot) ?? value ?? '');
    });
    return cell;
  }

  // --------------------------------------------------------------------------- Per-model overrides

  /// Inline select for a model's `default_reasoning_level`.
  ///
  /// Blank means *inherit global* — the tier falls through to `routing.default_reasoning_level`
  /// in the configuration (default `high`). Picking a level writes an explicit per-model default.
  function buildInlineModelReasoning(p, model) {
    const key = 'default_reasoning_level';
    const slot = `model:${p.id}:${model.id}:${key}`;
    const cell = h('td', { class: 'num' });
    const value = model[key] ?? null;
    const edit = h('button', { class: 'icon-btn', type: 'button', 'aria-label': `Edit default reasoning level for ${model.id}`, title: `Edit default reasoning level` }, icon('pencil'));
    const renderView = () => cell.replaceChildren(h('span', { class: 'mono' }, value ?? 'inherit global'), edit);
    const draft = inlineDrafts.get(slot);
    if (draft !== undefined) renderEditor(draft);
    else renderView();

    function renderEditor(initial) {
      // Only the levels this provider's dialect can send are offered: anything else would be
      // refused when the filter is saved. A stored value outside the list is kept as an option so
      // editing another field cannot silently rewrite it.
      const levels = providerLevels(p, model);
      const choices = !initial || levels.includes(initial) ? levels : [...levels, initial];
      const select = h('select', { class: 'input', 'aria-label': 'Default reasoning level' },
        h('option', { value: '', selected: !initial }, 'inherit global'),
        ...choices.map((level) => h('option', { value: level, selected: initial === level }, level)));
      const save = h('button', { class: 'btn btn-sm btn-primary', type: 'button' }, 'Save');
      const cancel = h('button', { class: 'btn btn-sm', type: 'button' }, 'Cancel');
      cell.replaceChildren(h('div', { style: 'display:flex;gap:4px;align-items:center;' }, select, save, cancel));
      select.value = initial ?? '';
      const restore = () => {
        inlineDrafts.delete(slot);
        renderView();
      };
      const finish = async () => {
        const next = select.value || null;
        inlineDrafts.set(slot, next);
        save.disabled = true;
        try {
          await persist(next);
          inlineDrafts.delete(slot);
          refreshInlineDrawer(p.id);
        } catch (err) {
          save.disabled = false;
          ctx.toast.error(`Could not update default reasoning level for ${model.id}`, err);
        }
      };
      save.addEventListener('click', finish);
      select.addEventListener('change', finish);
      cancel.addEventListener('click', restore);
    }

    async function persist(next) {
      const value = next instanceof Set ? Array.from(next) : next;
      await api.post(`/api/providers/${encodeURIComponent(p.id)}/models`, { ...model, [key]: value });
      await reload(true);
    }

    edit.addEventListener('click', () => renderEditor(value));
    return cell;
  }

  /// Inline chips for a model's **declared** `reasoning_levels`.
  ///
  /// Separate from `buildInlineModelReasoning` on purpose: that one picks the *default* tier, this
  /// one decides *which tiers the model offers at all*. The drawer previously exposed only the
  /// former, so an operator could choose a default from the dialect's vocabulary but had no way to
  /// grant or remove a level on a particular model — the declared list could only be edited on the
  /// Models page, behind a collapsed "Configure" panel.
  ///
  /// Off means *derive* (the bundled capability table, then the dialect). On writes an explicit
  /// list, which always wins over the derived one.
  function buildInlineModelLevels(p, model) {
    const key = 'reasoning_levels';
    const slot = `model:${p.id}:${model.id}:${key}`;
    const cell = h('td', {});
    const offered = providerLevels(p, model);
    const declared = declaredLevels(model);
    const value = new Set(declared);
    const edit = h('button', {
      class: 'icon-btn', type: 'button',
      'aria-label': `Edit supported reasoning levels for ${model.id}`,
      title: 'Edit supported reasoning levels',
    }, icon('pencil'));
    const renderView = () => {
      const chips = declared.length
        ? declared.map((level) => chip(String(level), 'chip-neutral', 'reasoning level'))
        : [h('span', { class: 'muted' }, 'automatic')];
      cell.replaceChildren(h('div', { style: 'display:flex;gap:4px;flex-wrap:wrap;' }, ...chips), edit);
    };
    const draft = inlineDrafts.get(slot);
    if (draft !== undefined) renderEditor(draft);
    else renderView();

    function renderEditor(initial) {
      const chips = offered.map((level) => {
        // A declared level this dialect cannot send would be dropped on save. Keep it visible and
        // marked rather than letting the checkbox imply it survives.
        const unsendable = initial.has(level) && !offered.includes(level);
        const btn = h('button', {
          class: 'chip' + (initial.has(level) ? ' chip-active' : ''),
          type: 'button',
          'aria-pressed': initial.has(level) ? 'true' : 'false',
          title: unsendable ? `${level} is not sendable on this provider` : level,
        }, level);
        btn.addEventListener('click', () => {
          const next = new Set(initial);
          if (next.has(level)) next.delete(level); else next.add(level);
          inlineDrafts.set(slot, next);
          renderEditor(next);
        });
        return btn;
      });
      const save = h('button', { class: 'btn btn-sm btn-primary', type: 'button' }, 'Save');
      const cancel = h('button', { class: 'btn btn-sm', type: 'button' }, 'Cancel');
      cell.replaceChildren(h('div', {},
        h('div', { style: 'display:flex;gap:4px;flex-wrap:wrap;align-items:center;' }, chips, save, cancel),
        h('p', { class: 'hint', style: 'margin:4px 0 0;' }, `None checked means automatic (${offered.join(', ')}).`)));
      const restore = () => {
        inlineDrafts.delete(slot);
        renderView();
      };
      const finish = async () => {
        const next = inlineDrafts.get(slot) ?? new Set();
        save.disabled = true;
        inlineDrafts.delete(slot);
        try {
          await persist(next);
        } catch (err) {
          save.disabled = false;
          ctx.toast.error(`Could not update reasoning levels for ${model.id}`, err);
        }
      };
      save.addEventListener('click', finish);
      cancel.addEventListener('click', restore);
    }

    async function persist(next) {
      const list = Array.from(next);
      await api.post(`/api/providers/${encodeURIComponent(p.id)}/models`, { ...model, [key]: list });
      await reload(true);
    }

    edit.addEventListener('click', () => renderEditor(value));
    return cell;
  }

  /// Inline chips for a model's `input_modalities`.
  ///
  /// Empty means *derive* from the bundled capability table. Toggling a chip writes an explicit
  /// per-model list.
  function buildInlineModelModalities(p, model) {
    const key = 'input_modalities';
    const slot = `model:${p.id}:${model.id}:${key}`;
    const cell = h('td', {});
    const value = new Set(model[key] ?? []);
    const edit = h('button', { class: 'icon-btn', type: 'button', 'aria-label': `Edit input modalities for ${model.id}`, title: `Edit input modalities` }, icon('pencil'));
    const renderView = () => {
      const chips = model[key]?.length
        ? model[key].map((mod) => chip(String(mod), 'chip-neutral', 'input modality'))
        : [h('span', { class: 'muted' }, 'automatic')];
      cell.replaceChildren(h('div', { style: 'display:flex;gap:4px;flex-wrap:wrap;' }, ...chips), edit);
    };
    const draft = inlineDrafts.get(slot);
    if (draft !== undefined) renderEditor(draft);
    else renderView();

    function renderEditor(initial) {
      const chips = INPUT_MODALITIES.map((mod) => {
        const btn = h('button', {
          class: 'chip' + (initial.has(mod) ? ' chip-active' : ''),
          type: 'button',
          'aria-pressed': initial.has(mod) ? 'true' : 'false',
        }, mod);
        btn.addEventListener('click', () => {
          const next = new Set(initial);
          if (next.has(mod)) next.delete(mod); else next.add(mod);
          inlineDrafts.set(slot, next);
          renderEditor(next);
        });
        return btn;
      });
      const save = h('button', { class: 'btn btn-sm btn-primary', type: 'button' }, 'Save');
      const cancel = h('button', { class: 'btn btn-sm', type: 'button' }, 'Cancel');
      cell.replaceChildren(h('div', { style: 'display:flex;gap:4px;flex-wrap:wrap;align-items:center;' }, chips, save, cancel));
      const restore = () => {
        inlineDrafts.delete(slot);
        renderView();
      };
      const finish = async () => {
        const next = inlineDrafts.get(slot) ?? new Set();
        save.disabled = true;
        // Clear the draft before saving so the re-render after persist renders the view,
        // not the editor.
        inlineDrafts.delete(slot);
        try {
          await persist(next);
        } catch (err) {
          save.disabled = false;
          ctx.toast.error(`Could not update input modalities for ${model.id}`, err);
        }
      };
      save.addEventListener('click', finish);
      cancel.addEventListener('click', restore);
    }

    async function persist(next) {
      const value = next instanceof Set ? Array.from(next) : next;
      await api.post(`/api/providers/${encodeURIComponent(p.id)}/models`, { ...model, [key]: value });
      await reload(true);
    }

    edit.addEventListener('click', () => renderEditor(value));
    return cell;
  }

  function buildInlineNumber(p, key, label, hint) {
    const slot = `provider:${p.id}:${key}`;
    const row = h('div', { class: 'field' });
    const value = p[key];
    const draft = inlineDrafts.get(slot);
    const edit = h('button', { class: 'btn btn-sm', type: 'button', 'aria-label': `Edit ${label} timeout` }, 'Edit');
    const renderView = () => {
      row.replaceChildren(h('div', { class: 'label-line' }, h('span', {}, label), edit),
        h('span', { class: 'mono' }, value == null ? '—' : `${value} ms`),
        hint ? h('p', { class: 'hint' }, hint) : null);
    };
    const renderEditor = (initial) => {
      const input = h('input', { class: 'input', type: 'number', min: '0', step: '1', value: initial, 'aria-label': `${label} timeout in milliseconds` });
      const save = h('button', { class: 'btn btn-sm btn-primary', type: 'button' }, 'Save');
      const cancel = h('button', { class: 'btn btn-sm', type: 'button' }, 'Cancel');
      row.replaceChildren(h('div', { class: 'label-line' }, h('span', {}, label)),
        h('div', { style: 'display:flex;gap:6px;align-items:center;' }, input, save, cancel),
        hint ? h('p', { class: 'hint' }, hint) : null);
      input.addEventListener('input', () => inlineDrafts.set(slot, input.value));
      const closeEditor = () => {
        inlineDrafts.delete(slot);
        const latest = providers.find((provider) => provider.id === p.id) ?? p;
        const body = row.closest('.drawer-body');
        if (body) renderDetail(body, latest);
      };
      const finish = async () => {
        if (!input.value.trim()) { ctx.toast.warning('Enter a timeout value in milliseconds.'); return; }
        const number = Number(input.value);
        if (!Number.isSafeInteger(number) || number < 0 || number > 18446744073709551615) { ctx.toast.warning('Timeout must be a non-negative whole number in the supported range.'); return; }
        inlineDrafts.set(slot, input.value);
        save.disabled = true;
        if (await updateProviderField(p, { [key]: number })) {
          inlineDrafts.delete(slot);
          refreshInlineDrawer(p.id);
        } else save.disabled = false;
      };
      save.addEventListener('click', finish);
      input.addEventListener('keydown', (event) => {
        if (event.key === 'Enter') { event.preventDefault(); finish(); }
        if (event.key === 'Escape') { event.preventDefault(); closeEditor(); }
      });
      input.addEventListener('blur', () => inlineDrafts.set(slot, input.value));
      cancel.addEventListener('click', closeEditor);
    };
    if (draft !== undefined) renderEditor(draft);
    else renderView();
    edit.addEventListener('click', () => renderEditor(inlineDrafts.get(slot) ?? value ?? ''));
    return row;
  }

  function ms(v) { return v == null ? '—' : `${v} ms`; }

  function authLabel(auth) {
    if (!auth) return 'bearer (default)';
    if (typeof auth === 'string') return auth;
    const type = auth.type ?? 'bearer';
    const extra = auth.header ?? auth.name ?? auth.param ?? auth.platform ?? '';
    return extra ? `${type} (${extra})` : type;
  }

  // Inline-editable key/value section (headers, route overrides).
  // Each entry is a key + value input with a delete button; "Add entry" appends a blank row.
  // Saves the whole map back to the provider via PUT /api/providers/{id}.
  function buildKvEditor(title, currentMap, onSave) {
    const entries = Object.entries(currentMap ?? {});
    const list = h('div', { class: 'stack' });
    const status = h('div');

    function draw() {
      list.replaceChildren();
      if (!entries.length) {
        list.append(h('p', { class: 'hint' }, 'none'));
      }
      for (const [index, [key, value]] of entries.entries()) {
        const keyIn = h('input', { class: 'input mono', type: 'text', value: key, 'aria-label': `Key ${index + 1}` });
        const valIn = h('input', { class: 'input mono', type: 'text', value: String(value ?? ''), 'aria-label': `Value ${index + 1}` });
        const remove = h('button', { class: 'btn btn-sm', type: 'button' }, 'Remove');
        remove.addEventListener('click', () => {
          entries.splice(index, 1);
          draw();
        });
        const row = h('div', { class: 'card', style: 'margin:8px 0;padding:10px;' },
          h('div', { class: 'grid-2' },
            field('Key', keyIn),
            field('Value', valIn)),
          remove);
        list.append(row);
      }
    }
    draw();

    const add = h('button', { class: 'btn', type: 'button' }, 'Add entry');
    add.addEventListener('click', () => {
      entries.push(['', '']);
      draw();
    });

    const save = h('button', { class: 'btn btn-primary', type: 'button' }, 'Save');
    save.addEventListener('click', async () => {
      const next = {};
      let invalid = false;
      for (const row of list.querySelectorAll('.card')) {
        const [keyIn, valIn] = row.querySelectorAll('input');
        const k = keyIn.value.trim();
        if (!k) { invalid = true; break; }
        next[k] = valIn.value;
      }
      if (invalid) {
        status.replaceChildren(h('div', { class: 'form-errors', role: 'alert' }, 'Every entry needs a non-empty key.'));
        return;
      }
      save.disabled = true;
      try {
        await onSave(next);
        status.replaceChildren();
        ctx.toast.success(`${title} saved.`);
      } catch (err) {
        status.replaceChildren(errorBlock(err));
        save.disabled = false;
      }
    });

    return h('section', {},
      h('h3', {}, title),
      h('p', { class: 'hint' }, 'Add, edit, or remove entries. Empty keys are not allowed.'),
      list,
      h('div', { class: 'dialog-actions', style: 'margin-top:8px;' }, add, save),
      status);
  }

  function mapKvSection(map) {
    const entries = Object.entries(map ?? {});
    if (!entries.length) return h('p', { class: 'hint' }, 'none');
    return kv(entries.map(([k, v]) => [k, h('span', { class: 'mono' }, String(v))]));
  }

  async function toggleAccount(p, a, btn) {
    btn.disabled = true;
    try {
      await api.put(`/api/providers/${encodeURIComponent(p.id)}/accounts/${encodeURIComponent(a.id)}`,
        { ...a, enabled: a.enabled === false });
      await reload(true);
    } catch (err) {
      btn.disabled = false;
      ctx.toast.error('Could not update the account', err);
    }
  }

  async function deleteAccount(p, a) {
    const ok = await ctx.confirm({
      title: `Delete account “${a.id}”?`,
      message: 'Its stored credential is removed with it.',
      confirmLabel: 'Delete account',
    });
    if (!ok) return;
    try {
      const result = await api.del(`/api/providers/${encodeURIComponent(p.id)}/accounts/${encodeURIComponent(a.id)}`);
      // Removing the last account of a provider is a *successful* delete whose resulting
      // configuration the conductor can only accept with an advisory: that provider can never
      // serve again. The save path reports success plus a top-level `warnings` array, so say the
      // delete happened — and then surface every advisory instead of letting the success message
      // bury it. `result` is undefined when the response carried no JSON body at all.
      ctx.toast.success('Account deleted.');
      const warnings = Array.isArray(result?.warnings) ? result.warnings : [];
      for (const warning of warnings) {
        if (typeof warning === 'string' && warning.trim()) ctx.toast.warning(warning);
      }
      await reload(true);
    } catch (err) {
      ctx.toast.error('Delete failed', err);
    }
  }

  async function toggleModelHidden(p, m, btn) {
    btn.disabled = true;
    try {
      // POST /api/providers/{id}/models is the upsert path; hidden is authored metadata and the
      // manual entry wins over anything a later sync knows (docs/03-configuration.md §models).
      await api.post(`/api/providers/${encodeURIComponent(p.id)}/models`, { ...m, hidden: !m.hidden });
      await reload(true);
    } catch (err) {
      btn.disabled = false;
      ctx.toast.error('Could not change model visibility', err);
    }
  }

  async function deleteModel(p, m) {
    const ok = await ctx.confirm({
      title: `Delete model “${m.id}”?`,
      message: m.source === 'synced'
        ? 'It was synced from the provider; a later sync may bring it back.'
        : 'It was entered manually and will not come back.',
      confirmLabel: 'Delete model',
    });
    if (!ok) return;
    try {
      await api.del(`/api/providers/${encodeURIComponent(p.id)}/models/${encodeURIComponent(m.id)}`);
      ctx.toast.success(`Model ${m.id} deleted.`);
      await reload(true);
    } catch (err) {
      ctx.toast.error('Delete failed', err);
    }
  }

  // ------------------------------------------------------------------ quota chip

  function quotaChipFrom(windows) {
    // Show the most constrained window: min remaining percent across windows.
    let minPercent = 100;
    let minLabel = '';
    for (const w of windows ?? []) {
      if (!w || typeof w !== 'object') continue;
      const remaining = w.remaining ?? null;
      const total = w.total ?? null;
      if (remaining == null || total == null || total <= 0) continue;
      const pct = Math.min(100, Math.max(0, Math.round((remaining / total) * 100)));
      if (pct < minPercent) {
        minPercent = pct;
        minLabel = w.family ?? 'window';
      }
    }
    if (minPercent === 100 && minLabel === '') return null;
    const cls = minPercent <= 10 ? 'chip-red' : minPercent <= 30 ? 'chip-amber' : 'chip-green';
    return chip(`quota ${minLabel}: ${minPercent}% left`, cls, `${minPercent}% of quota remaining`);
  }

  // ------------------------------------------------------------------ forms

  function buildAccountForm(p, onConnected, describedBy) {
    if (p.auth?.type === 'oauth') {
      // The drawer can be closed while the operator finishes the sign-in in the provider's tab.
      // Read it lazily: the form outlives this call, so a captured boolean would go stale.
      return buildOAuthAccountForm(p, ctx, onConnected, () => drawerOpen);
    }
    const label = h('input', { class: 'input', type: 'text', 'data-field': 'label' });
    const identity = h('input', { class: 'input mono', type: 'text', 'data-field': 'identity', placeholder: 'e.g. work@account' });
    const tags = h('input', { class: 'input mono', type: 'text', 'data-field': 'tags', placeholder: 'alice, work' });
    const secret = h('input', { class: 'input mono', type: 'password', 'data-field': 'secret', autocomplete: 'new-password', ...(describedBy ? { 'aria-describedby': describedBy } : {}) });
    const revealSecret = h('button', {
      class: 'btn btn-sm', type: 'button',
      'aria-pressed': 'false',
      'aria-label': 'Show secret',
    }, 'Show');
    let secretShown = false;
    revealSecret.addEventListener('click', () => {
      secretShown = !secretShown;
      secret.type = secretShown ? 'text' : 'password';
      revealSecret.textContent = secretShown ? 'Hide' : 'Show';
      revealSecret.setAttribute('aria-pressed', String(secretShown));
      revealSecret.textContent = secretShown ? 'Hide' : 'Show';
      revealSecret.setAttribute('aria-label', secretShown ? 'Hide secret' : 'Show secret');
      if (secretShown) {
        // Auto re-hide so a revealed secret never outlives the moment.
        setTimeout(() => {
          secretShown = false;
          secret.type = 'password';
          revealSecret.textContent = 'Show';
          revealSecret.setAttribute('aria-pressed', 'false');
          revealSecret.setAttribute('aria-label', 'Show secret');
        }, 10000);
      }
    });
    const secretBox = field('Secret / API key', secret);
    secretBox.append(revealSecret);
    secretBox.classList.add('secret-field');
    // Local servers (Ollama, llama.cpp) really do accept requests with no credential, so the
    // requirement needs an explicit, visible opt-out rather than a silent empty submit. Without
    // this an account created with a blank key only fails later, inside a request, as an opaque
    // upstream 401 or an endless "no usable account".
    const keyless = h('input', { type: 'checkbox', 'data-field': 'keyless' });
    keyless.addEventListener('change', () => {
      secret.disabled = keyless.checked;
      revealSecret.disabled = keyless.checked;
      if (keyless.checked) secret.value = '';
      else secret.focus();
    });
    const keylessRow = h('label', { class: 'checkbox-row' },
      keyless,
      h('span', {}, 'This provider needs no API key (local Ollama or llama.cpp)'));
    const add = h('button', { class: 'btn btn-primary', type: 'submit' }, 'Add account');
    const form = h('form', { novalidate: true },
      h('div', { class: 'grid-fields' },
        field('Label', label),
        field('Identity (optional)', identity),
        field('Tags (optional)', tags, { hint: 'Free-text notes, comma separated. A tag that names a person in Settings sends this account through that person\u2019s machine.' }),
        secretBox),
      keylessRow,
      add);
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      clearFieldErrors(form);
      const key = secret.value.trim();
      if (!key && !keyless.checked) {
        applyFieldErrors(form, {
          message: 'This account has no key.',
          details: [{
            field: 'secret',
            message: 'Enter the API key, or tick "This provider needs no API key" if the server accepts requests without one.',
          }],
        });
        secret.focus();
        return;
      }
      add.disabled = true;
      add.textContent = 'Adding…';
      try {
        // Field names are the Account struct's (mr-core/src/model.rs); the secret is sent once
        // and stored server-side as a secret_ref. Validation errors land next to their fields.
        await api.post(`/api/providers/${encodeURIComponent(p.id)}/accounts`,
          {
            label: label.value || null,
            identity: identity.value || null,
            tags: parseTagList(tags.value),
            secret: keyless.checked ? null : key,
          });
        // Clear the plaintext from the DOM immediately — it must not linger on screen.
        secret.value = '';
        ctx.toast.success('Account added.');
        if (onConnected) {
          // The caller owns navigation: it closes this drawer, refreshes, and opens the detail
          // view exactly once. Wrapped so a navigation failure can never turn a successful
          // account create into a failure toast.
          try { await onConnected(); } catch {
            ctx.toast.warning('Account added. Could not open the provider drawer — open it from the list.');
          }
          return;
        }
      } catch (err) {
        applyFieldErrors(form, err);
        const msg = err.message ? err.message : 'Could not add the account';
        if (!err.details?.length) ctx.toast.error(msg, err);
        add.disabled = false;
        add.textContent = 'Add account';
      }
    });
    return form;
  }

  // ------------------------------------------------------------------ OAuth

  /// The one-click sign-in for an OAuth provider.
  ///
  /// Two flows are offered where the platform supports both. The browser redirect is the default
  /// because on the usual deployment — router and browser on one machine — it is a single click.
  /// The device code exists for the cases where a redirect cannot work at all: a headless server, a
  /// container, or a browser on a different host. Offering only the redirect would make those
  /// deployments impossible to log in from the panel.
  function buildModelForm(p) {
    const id = h('input', { class: 'input mono', type: 'text', required: true, 'data-field': 'id', placeholder: 'upstream-model-id' });
    const ctxLen = h('input', { class: 'input', type: 'number', min: '0', 'data-field': 'context_length' });
    const add = h('button', { class: 'btn', type: 'submit' }, 'Add model');
    const form = h('form', { novalidate: true },
      h('div', { class: 'grid-fields' },
        field('Model ID (manual)', id),
        field('Context length (optional)', ctxLen)),
      add);
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      clearFieldErrors(form);
      add.disabled = true;
      try {
        await api.post(`/api/providers/${encodeURIComponent(p.id)}/models`, {
          id: id.value.trim(),
          source: 'manual', // manual entries are authoritative over later syncs
          context_length: ctxLen.value ? Number(ctxLen.value) : undefined,
        });
        ctx.toast.success(`Model ${id.value.trim()} added.`);
        await reload(true);
      } catch (err) {
        applyFieldErrors(form, err);
        if (!err.details?.length) ctx.toast.error('Could not add the model', err);
        add.disabled = false;
      }
    });
    return form;
  }

  /// The single place that leaves the account drawer after a successful connect.
  ///
  /// Exactly one close, one refresh, one detail render — the flow used to do all three twice,
  /// which showed the drawer flicker and cost an extra round trip. A refresh failure is reported
  /// as a warning and never contradicts the success the operator just saw.
  async function afterAccountConnected(providerId) {
    closeDrawer();
    let refreshed = true;
    try {
      await reload(true);
    } catch {
      refreshed = false;
      ctx.toast.warning('Account added, but the provider list could not be refreshed. Reload the page to see it.');
    }
    const fresh = providers.find((x) => x.id === providerId);
    if (fresh) await openDetailDrawer(fresh);
    else if (refreshed) ctx.toast.warning(`Account added, but provider ${providerId} was not in the refreshed list.`);
  }

  // After creating a provider, go straight to adding its first account so the operator can paste
  // the API key right away instead of hunting for the field in another drawer.
  async function openAccountDrawer(providerId) {
    if (!(await confirmDiscardForm('This provider has unsaved changes. Adding an account will discard them.'))) return;
    const p = providers.find((x) => x.id === providerId);
    if (!p) {
      // Provider not in the local snapshot (a concurrent reload can clobber it). Never fail
      // silently: tell the operator how to get to the drawer by hand.
      ctx.toast.warning(`Provider ${providerId} created. Open its drawer to add an account.`);
      return;
    }
    closeDrawer();
    // Keep drawerId null so the account form's own reload does not re-open the detail drawer
    // mid-flow; navigation after account creation is handled explicitly below.
    drawerId = null;
    const isOAuth = p.auth?.type === 'oauth';
    const { head, body, drawer } = drawerShell(`Add your first account to ${providerId}`, `Add account to ${providerId}`);
    head.append(closeButton());
    const connected = () => afterAccountConnected(providerId);
    if (!isOAuth) {
      const hintId = `account-hint-${providerId}`;
      body.append(h('p', { class: 'hint', id: hintId },
        'Enter the API key for this provider. It is stored in the encrypted vault as a secret '
        + 'reference — never as plaintext.'));
      body.append(buildAccountForm(p, connected, hintId));
    } else {
      body.append(buildAccountForm(p, connected));
    }
    // Move focus into the drawer so a keyboard/screen-reader user lands on the primary input.
    requestAnimationFrame(() => {
      const first = drawer.querySelector('input, button');
      if (first) first.focus();
    });
  }

  /** Create (p === null) or edit an existing provider. */
  async function openFormDrawer(p) {
    if (!(await confirmDiscardForm('This provider has unsaved changes. Opening another provider form will discard them.'))) return;
    closeDrawer();
    const formState = { dirty: false, guard: null, confirming: false };
    activeForm = formState;
    function markDirty() {
      if (formState.dirty) return;
      formState.dirty = true;
      const guard = () => ctx.confirm({
        title: 'Leave without saving?',
        message: 'This provider has unsaved changes. Leaving this page will discard them.',
        confirmLabel: 'Leave and discard',
        cancelLabel: 'Stay on this page',
        // A route change is the other way to lose the draft; keep the safe action focused.
        danger: true,
      });
      formState.guard = guard;
      ctx.leaveGuard = guard;
    }
    function clearDirty() {
      formState.dirty = false;
      // Only retract this form's guard: a late save from an old form must not disarm a newer one.
      if (formState.guard && ctx.leaveGuard === formState.guard) ctx.leaveGuard = null;
      formState.guard = null;
    }
    const { head, body } = drawerShell(p ? `Edit provider ${p.id}` : 'New provider');
    head.append(closeButton(p ? 'Close edit provider form' : 'Close new provider form'));

    const isEdit = Boolean(p);
    const idIn = h('input', { class: 'input mono', type: 'text', 'data-field': 'id', value: p?.id ?? '', disabled: isEdit });
    const labelIn = h('input', { class: 'input', type: 'text', 'data-field': 'label', value: p?.label ?? '' });
    const dialectIn = h('select', { class: 'input', 'data-field': 'dialect' },
      ...DIALECTS.map((d) => h('option', { value: d, selected: (p?.dialect ?? 'open-ai') === d }, d)));
    const presetSel = h('select', { class: 'input', 'data-field': 'preset', 'aria-label': 'Provider preset' },
      h('optgroup', { label: 'Standard & media APIs' },
        ...PROVIDER_PRESETS.providers.map((p) => h('option', { value: p.name, selected: isEdit ? false : p.name === 'Custom (manual)' }, p.name))),
      h('optgroup', { label: 'Platforms & enterprise' },
        ...PROVIDER_PRESETS.platforms.map((p) => h('option', { value: p.name, selected: isEdit ? false : false }, p.name))));
    const presetHint = h('p', { class: 'hint' });
    const baseUrl = h('input', { class: 'input mono', type: 'text', 'data-field': 'base_url', value: p?.base_url ?? '', placeholder: 'https://…' });
    const note = h('input', { class: 'input', type: 'text', 'data-field': 'note', value: p?.note ?? '' });

    const auth = p?.auth ?? { type: 'bearer' };
    const authType = typeof auth === 'string' ? 'bearer' : (auth.type ?? 'bearer');
    const authSel = h('select', { class: 'input', 'data-field': 'type' },
      ...AUTH_KINDS.map(([v, lbl]) => h('option', { value: v, selected: authType === v }, lbl)));
    const authHeader = h('input', { class: 'input mono', type: 'text', 'data-field': 'header', value: auth.header ?? 'x-api-key' });
    const authName = h('input', { class: 'input mono', type: 'text', 'data-field': 'name', value: auth.name ?? '' });
    const authPrefix = h('input', { class: 'input mono', type: 'text', 'data-field': 'prefix', value: auth.prefix ?? '' });
    const authParam = h('input', { class: 'input mono', type: 'text', 'data-field': 'param', value: auth.param ?? 'key' });
    const authPlatform = h('input', { class: 'input mono', type: 'text', 'data-field': 'platform', value: auth.platform ?? '' });
    const authBox = h('div', {});
    const authWarn = h('div', {});
    authSel.addEventListener('change', () => renderAuthParams());

    // A dialect with only an OAuth path must not be saved with any other auth kind. The value is
    // whatever an older form (or the CLI) stored, so this warns and offers the switch rather than
    // rewriting a field the operator is looking at: a silent change would be indistinguishable
    // from the panel losing the edit, and the warning names why it matters.
    function renderAuthWarn() {
      authWarn.replaceChildren();
      const platform = DIALECT_OAUTH_PLATFORMS[dialectIn.value];
      if (!platform || authSel.value === 'oauth') return;
      const fix = h('button', { class: 'btn', type: 'button' }, `Switch to the ${platform} OAuth login`);
      fix.addEventListener('click', () => {
        authSel.value = 'oauth';
        authPlatform.value = platform;
        renderAuthParams();
      });
      authWarn.append(
        h('p', { class: 'hint', style: 'color:var(--warn, #c98a00);' },
          `Dialect ${dialectIn.value} has no API-key path: it signs in through the ${platform} OAuth platform. ` +
          `With auth ${authSel.value} this provider saves, and then refuses every request for having no credential the endpoint will read.`),
        fix,
      );
    }

    function renderAuthParams() {
      const t = authSel.value;
      authBox.replaceChildren();
      if (t === 'api-key-header') authBox.append(field('Header', authHeader, { hint: 'Header that carries the key.' }));
      else if (t === 'header') authBox.append(h('div', { class: 'grid-fields' },
        field('Header name', authName), field('Prefix (optional)', authPrefix, { hint: 'e.g. “Bearer ”' })));
      else if (t === 'query-param') authBox.append(field('Query parameter', authParam));
      else if (t === 'oauth') authBox.append(field('Platform', authPlatform, {
        hint: 'OAuth platform id — codex, copilot, antigravity or cursor. The Connect button starts this platform’s login; an empty or unknown id leaves the provider with no way to obtain a credential.',
      }));
      renderAuthWarn();
    }
    renderAuthParams();

    // Preset behavior: applying a preset sets dialect, base URL, auth kind, the header fields
    // that auth kind reads, and the capability ticks. A preset that names `authHeader` also sets
    // the header, because an auth kind alone is not enough to be usable: `api-key-header`
    // defaults to `x-api-key`, and Gemini wants `x-goog-api-key`, so a preset that only picked the
    // kind would fill the form in with a header that is refused. The same is true of `header`
    // auth, which needs both the header name and its prefix (Deepgram: Authorization + `Token `).
    presetSel.addEventListener('change', () => {
      const preset = [
        ...PROVIDER_PRESETS.providers,
        ...PROVIDER_PRESETS.platforms,
      ].find((pr) => pr.name === presetSel.value);
      if (!preset) return;
      if (!isEdit) {
        dialectIn.value = preset.dialect;
        baseUrl.value = preset.baseUrl;
        authSel.value = preset.auth;
        // The platform is part of what the preset knows. Filling only the auth kind is what made
        // a preset-created Cursor provider unconnectable: `oauth` with an empty platform is not a
        // login, it is a promise the panel cannot keep.
        if (preset.platform) authPlatform.value = preset.platform;
        if (preset.authHeader) authHeader.value = preset.authHeader;
        // `header` presets carry both fields in the catalog. Reset when a preset does not name
        // them, so switching from Deepgram to ElevenLabs cannot leave Deepgram's Token prefix on
        // ElevenLabs' xi-api-key header.
        authName.value = preset.authName ?? '';
        authPrefix.value = preset.authPrefix ?? '';
        // Replace rather than add: switching from Fish Audio to OpenAI must drop audio-speech,
        // and a preset with no capability list keeps the server default the form already shows.
        applyPresetCapabilities(
          Array.isArray(preset.capabilities) && preset.capabilities.length > 0
            ? preset.capabilities
            : DEFAULT_CAPABILITIES,
        );
        renderAuthParams();
      }
      presetHint.textContent = preset.hint;
    });
    presetHint.textContent = [
      ...PROVIDER_PRESETS.providers,
      ...PROVIDER_PRESETS.platforms,
    ].find((pr) => pr.name === presetSel.value)?.hint ?? '';

    // On edit, tick exactly what the server sent; on create — and when an older provider arrives
    // without the key, or with the empty array a hand-edited config can carry — mirror the
    // server's `default_capabilities()` instead of showing an unsaveable empty form.
    const initialCapabilities = isEdit && Array.isArray(p.capabilities) && p.capabilities.length > 0
      ? new Set(p.capabilities)
      : new Set(DEFAULT_CAPABILITIES);
    const capabilityBoxes = new Map();
    // `field` is the class applyFieldErrors anchors to, so its message can be pinned directly
    // under this fieldset. `toggle` is the checkbox counterpart of `field()`: the hint sits on
    // its own line under the row.
    const capabilityFieldset = h('fieldset', { class: 'plain field', 'data-field': 'capabilities' },
      h('legend', {}, 'Capabilities'));
    for (const group of CAPABILITY_GROUPS) {
      capabilityFieldset.append(
        h('h4', { style: 'margin:12px 0 4px;font-size:11px;text-transform:uppercase;letter-spacing:0.06em;color:var(--tx3);' }, group.id),
        h('p', { class: 'hint', style: 'margin:0 0 6px;' }, group.hint));
      for (const capability of CAPABILITIES) {
        if (capability.group !== group.id) continue;
        const checkbox = toggle(capability.label, {
          hint: capability.hint,
          checked: initialCapabilities.has(capability.id),
        });
        checkbox.input.setAttribute('data-capability', capability.id);
        capabilityBoxes.set(capability.id, checkbox.input);
        capabilityFieldset.append(checkbox.row);
      }
    }

    // Defined after the boxes but called from the preset handler above, which only runs after the
    // form is built. A preset replaces the whole tick set: adding to it would leave the previous
    // provider's modalities ticked, and the resulting provider would claim routes it cannot serve.
    function applyPresetCapabilities(ids) {
      const wanted = new Set(ids);
      for (const [id, input] of capabilityBoxes) input.checked = wanted.has(id);
    }

    const timeouts = h('div', { class: 'grid-fields' });
    const tFields = {};
    for (const t of ['connect_timeout_ms', 'ttfb_timeout_ms', 'idle_timeout_ms', 'total_timeout_ms', 'cold_start_ttfb_timeout_ms']) {
      const input = h('input', { class: 'input', type: 'number', min: '0', 'data-field': t, value: p?.[t] ?? '' });
      tFields[t] = input;
      timeouts.append(field(t.replace('_timeout_ms', ' (ms)'), input));
    }
    const budgetFields = {};
    for (const key of ['kv_pool_tokens', 'max_context', 'max_concurrent', 'price_per_second']) {
      const input = h('input', {
        class: 'input',
        type: 'number',
        min: '0',
        step: key === 'price_per_second' ? 'any' : '1',
        value: p?.[key] ?? '',
        'data-field': key,
      });
      budgetFields[key] = input;
    }
    const budgetSettings = h('fieldset', { class: 'plain' }, h('legend', {}, 'Self-hosted capacity and cost'),
      h('div', { class: 'grid-fields' },
        field('KV pool tokens', budgetFields.kv_pool_tokens, { hint: 'Total token capacity reserved across in-flight prompts.' }),
        field('Max context tokens', budgetFields.max_context, { hint: 'Overrides discovered context length during model sync.' }),
        field('Max concurrent requests', budgetFields.max_concurrent),
        field('GPU price per second (USD)', budgetFields.price_per_second, { hint: 'Records elapsed request wall-clock as GPU seconds and estimates spend.' })));

    const keyStrat = h('select', { class: 'input', 'data-field': 'key_strategy' },
      ...KEY_STRATEGIES.map((k) => h('option', { value: k, selected: (p?.key_strategy ?? 'priority') === k }, k)));
    const cooldownScope = h('select', { class: 'input', 'data-field': 'cooldown_scope' },
      ...COOLDOWN_SCOPES.map((s) => h('option', { value: s, selected: (p?.cooldown_scope ?? 'account') === s }, s)));
    const prewarmToggle = toggle('Prewarm connections at startup', {
      checked: p?.prewarm ?? false,
      hint: 'Per-provider override; global latency.prewarm still applies server-side.',
    });
    const enabledToggle = toggle('Provider enabled', { checked: p?.enabled ?? true });

    // Track user edits rather than testing the current value: an operator who clears the URL
    // intentionally should not have the dialect handler silently fill it back in.
    let baseUrlDirty = false;
    let lastSuggestedBaseUrl = '';
    baseUrl.addEventListener('input', () => { baseUrlDirty = true; });
    const baseUrlSuggestion = h('p', { class: 'hint' });
    dialectIn.addEventListener('change', () => {
      // One-way implication, in both create and edit: a dialect with a single supported login
      // sets that kind and platform. Moving to a dialect that accepts an API key leaves the auth
      // as it stands, because `oauth` is still a valid choice there and rewriting it would throw
      // away a working sign-in.
      const platform = DIALECT_OAUTH_PLATFORMS[dialectIn.value];
      if (platform && authSel.value !== 'oauth') {
        authSel.value = 'oauth';
        authPlatform.value = platform;
      }
      renderAuthParams();
      const suggestion = DIALECT_BASE_URLS[dialectIn.value];
      const current = baseUrl.value.trim();
      if (!suggestion || baseUrlDirty || (current && current !== lastSuggestedBaseUrl)) return;
      baseUrl.value = suggestion;
      lastSuggestedBaseUrl = suggestion;
      baseUrlSuggestion.textContent = `Suggested documented endpoint: ${suggestion}. Change it if this deployment uses another endpoint.`;
    });
    if (!isEdit && DIALECT_BASE_URLS[dialectIn.value]) {
      baseUrlSuggestion.textContent = `Suggested documented endpoint for ${dialectIn.value}: ${DIALECT_BASE_URLS[dialectIn.value]}. Change it if this deployment uses another endpoint.`;
    }

    const save = h('button', { class: 'btn btn-primary', type: 'submit' }, isEdit ? 'Save changes' : 'Create provider');
    const cancel = h('button', { class: 'btn', type: 'button' }, 'Cancel');
    cancel.addEventListener('click', () => { void requestCloseDrawer(); });

    const form = h('form', { novalidate: true },
      h('div', { class: 'grid-fields' },
        field('Provider ID', idIn, { hint: 'Used in combos as the provider reference; immutable once created.' }),
        field('Provider Name', labelIn)),
      h('div', { class: 'grid-fields' },
        field('Dialect', dialectIn),
        field('Preset', presetSel, { hint: presetHint }),
        h('div', { class: 'field' }, h('label', {}, 'Key strategy'),
          h('div', { style: 'display:flex;gap:8px;align-items:center;' },
            keyStrat, h('span', { class: 'hint' }, 'Selects the account key rotation mode.')))),
      h('div', { class: 'grid-fields' }, field('Cooldown scope', cooldownScope, {
        hint: 'What a refusal takes out of rotation: the account for every model, that model on that account, or that model on all of the provider’s accounts. Use model when the upstream meters each model separately (per-model quotas); use account when a spent quota means the credential itself is spent.',
      })),
      field('Base URL', baseUrl),
      baseUrlSuggestion,
      capabilityFieldset,
      h('fieldset', { class: 'plain' }, h('legend', {}, 'Auth'), authSel, authBox, authWarn),
      h('fieldset', { class: 'plain' }, h('legend', {}, 'Timeouts'), timeouts),
      budgetSettings,
      prewarmToggle.row,
      enabledToggle.row,
      field('Note (optional)', note),
      h('div', { class: 'dialog-actions provider-form-actions' }, cancel, save));

    // Mark dirty only on operator input. Programmatic preset/initial values above must not arm a
    // guard before the form has anything worth keeping.
    form.addEventListener('input', markDirty);
    form.addEventListener('change', markDirty);

    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      clearFieldErrors(form);
      const selectedCapabilities = capabilityIds(
        [...capabilityBoxes].filter(([, input]) => input.checked).map(([id]) => id),
      );
      if (selectedCapabilities.length === 0) {
        applyFieldErrors(form, {
          details: [{
            field: 'capabilities',
            message: 'A provider with no capabilities can serve nothing. Tick at least one.',
          }],
        });
        return;
      }
      // Refused here as well as in the conductor, on purpose: the server's refusal is the rule,
      // and this is the version that can pin the reason under the field instead of a toast about
      // a provider that was never created.
      if (authSel.value === 'oauth' && !authPlatform.value.trim()) {
        applyFieldErrors(form, {
          details: [{
            field: 'platform',
            message: 'Name the OAuth platform this provider signs in through (codex, copilot, antigravity, cursor). ' +
              'Without it there is no login to start, so the provider can never hold a credential.',
          }],
        });
        return;
      }
      save.disabled = true;
      save.textContent = 'Saving…';
      const selectedPreset = [
        ...PROVIDER_PRESETS.providers,
        ...PROVIDER_PRESETS.platforms,
      ].find((preset) => preset.name === presetSel.value);
      const payload = isEdit
        ? { ...p } // preserve everything the form does not show (headers, models, extra…)
        : {};
      payload.id = idIn.value.trim();
      payload.label = labelIn.value || null;
      payload.dialect = dialectIn.value;
      payload.base_url = baseUrl.value.trim();
      // The create form has no route-override editor, but an OpenAI-shaped media preset is only
      // usable with the catalog's path override. Send it only on create: an edit payload began as
      // `{ ...p }` and already preserves whatever route_overrides the provider has.
      if (!isEdit && selectedPreset?.routeOverrides) {
        payload.route_overrides = { ...selectedPreset.routeOverrides };
      }
      // Always assign the key, including on edit where payload began as `{ ...p }`: assignment
      // replaces the old array instead of merging it, so an unticked capability really goes away.
      payload.capabilities = selectedCapabilities;
      payload.note = note.value || null;
      payload.key_strategy = keyStrat.value;
      payload.cooldown_scope = cooldownScope.value;
      payload.prewarm = prewarmToggle.input.checked;
      payload.enabled = enabledToggle.input.checked;
      const t = authSel.value;
      payload.auth = t === 'bearer' ? { type: 'bearer' }
        : t === 'api-key-header' ? { type: 'api-key-header', header: authHeader.value || 'x-api-key' }
          : t === 'header' ? { type: 'header', name: authName.value, prefix: authPrefix.value }
            : t === 'query-param' ? { type: 'query-param', param: authParam.value || 'key' }
              : t === 'modal' ? { type: 'modal' }
                : { type: 'oauth', platform: authPlatform.value };
      // Timeouts: only send a value when the operator typed one. Assigning undefined would
      // stringify the key away and silently reset the server-side value on an edit.
      for (const [k, input] of Object.entries({ ...tFields, ...budgetFields })) {
        if (input.value !== '') payload[k] = Number(input.value);
        else delete payload[k];
      }

      try {
        if (isEdit) {
          await api.put(`/api/providers/${encodeURIComponent(p.id)}`, payload);
          ctx.toast.success(`Provider ${payload.id} saved.`);
        } else {
          await api.post('/api/providers', payload);
          ctx.toast.success(`Provider ${payload.id} created.`);
        }

        // The draft is committed; retract the guard before the panel is replaced by the detail
        // or the account drawer. Refresh/navigation failures below are not unsaved form edits.
        clearDirty();

        // The write above is committed once it succeeds. Refresh failures and post-create
        // navigation failures are reported separately — they never mask a successful create.
        if (!isEdit) {
          closeDrawer();
          try { await reload(true); } catch (reloadErr) {
            ctx.toast.warning(`Provider ${payload.id} created, but the provider list could not be refreshed.`);
          }
          try { await openAccountDrawer(payload.id); } catch (navErr) {
            ctx.toast.error(`Provider ${payload.id} created. Could not open the account drawer — open it from the provider list.`);
          }
          return;
        }
        drawerId = payload.id;
        await reload(true);
      } catch (err) {
        // A failure here means the provider write itself failed — only then do we treat it as
        // an edit/create error and re-enable the button.
        applyFieldErrors(form, err);
        if (!err.details?.length) ctx.toast.error('Could not save the provider', err);
        save.disabled = false;
        save.textContent = isEdit ? 'Save changes' : 'Create provider';
      }
    });

    body.append(form);
    if (!isEdit) idIn.focus();
    // On a short window the form is taller than the visible page. Pull its bottom edge into view
    // once it exists so Cancel and Create provider are reachable without hunting for them.
    requestAnimationFrame(() => {
      const panel = workWrap.querySelector('.work-panel');
      if (panel?.isConnected) panel.scrollIntoView({ block: 'nearest' });
    });
  }

  // Always a fresh read on mount. The shared cache is filled once per page load, and a provider
  // written after that (the CLI, another tab, a migration) would otherwise never appear here.
  await reload(true);
}
