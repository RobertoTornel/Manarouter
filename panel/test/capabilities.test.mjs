// Guards the capability editor in panel/view-providers.js.
//
// Three failures have happened here and nothing else catches them:
//
//   * a checkbox whose wire id the backend does not deserialise, which turns every provider
//     create/edit into a save-time error (and `Capability` has gained variants repeatedly),
//   * a save that drops capabilities the operator ticked, because the edit payload is a spread of
//     the old provider and a merge would keep an array element that was just unticked,
//   * a provider saved with no capabilities, which can serve nothing and is refused by the server
//     with a message that is easy to miss in a toast.
//
// The table is pinned to `crates/mr-core/src/dialect.rs` rather than to a hand-copied list: the
// Rust enum is what serde deserialises, so it is the only spelling that matters. `CAPABILITIES`
// and `capabilityIds` are pure, so they are read out of the source and evaluated rather than
// imported — view-providers.js pulls in the DOM layer at module scope and cannot load in Node.
//
// Run: node --test panel/test/capabilities.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import vm from 'node:vm';

const here = dirname(fileURLToPath(import.meta.url));
const panel = join(here, '..');
const repo = join(panel, '..');

const source = readFileSync(join(panel, 'view-providers.js'), 'utf8');

/// Slice out the balanced `{...}` or `[...]` that follows `opener`, skipping string literals.
function literal(text, opener) {
  const start = text.indexOf(opener);
  assert.notEqual(start, -1, `${opener} not found in view-providers.js`);
  const open = opener[opener.length - 1];
  const close = open === '{' ? '}' : ']';
  let depth = 0;
  for (let i = start + opener.length - 1; i < text.length; i += 1) {
    const ch = text[i];
    if (ch === "'" || ch === '"' || ch === '`') {
      const quote = ch;
      i += 1;
      while (i < text.length && text[i] !== quote) {
        if (text[i] === '\\') i += 1;
        i += 1;
      }
      continue;
    }
    if (ch === open) depth += 1;
    else if (ch === close) {
      depth -= 1;
      if (depth === 0) return text.slice(start + opener.length - 1, i + 1);
    }
  }
  throw new Error(`unterminated literal after ${opener}`);
}

const capabilities = vm.runInNewContext(literal(source, 'const CAPABILITIES = ['));
const groups = vm.runInNewContext(literal(source, 'const CAPABILITY_GROUPS = ['));

/// The source of a top-level function declaration, up to its column-zero closing brace.
function declaration(name) {
  const start = source.indexOf(`function ${name}(`);
  assert.notEqual(start, -1, `function ${name} not found in view-providers.js`);
  const end = source.indexOf('\n}', start);
  assert.notEqual(end, -1, `unterminated function ${name} in view-providers.js`);
  return source.slice(start, end + 2);
}

const helperContext = vm.createContext({ CAPABILITIES: capabilities });
vm.runInContext(declaration('capabilityIds'), helperContext);

function idsFor(selected) {
  helperContext.selected = selected;
  return vm.runInContext('capabilityIds(selected)', helperContext);
}

/// `AudioTranscription` -> `audio-transcription`, the serde kebab-case wire spelling.
function kebab(variant) {
  return variant.replace(/([a-z0-9])([A-Z])/g, '$1-$2').toLowerCase();
}

test('capability ids match the Rust enum, in order', () => {
  const dialect = readFileSync(join(repo, 'crates/mr-core/src/dialect.rs'), 'utf8');
  const start = dialect.indexOf('pub enum Capability {');
  assert.notEqual(start, -1, 'pub enum Capability not found in crates/mr-core/src/dialect.rs');
  const end = dialect.indexOf('\n}', start);
  assert.notEqual(end, -1, 'unterminated Capability enum in crates/mr-core/src/dialect.rs');
  const variants = [...dialect.slice(start, end).matchAll(
    /^\s{4}([A-Z][A-Za-z0-9]*)\s*(?:=\s*\d+)?,/gm,
  )].map((match) => match[1]);
  assert.ok(variants.length >= 2, 'no Capability variants parsed from crates/mr-core/src/dialect.rs');

  assert.deepEqual(
    [...capabilities.map((capability) => capability.id)],
    variants.map(kebab),
    'CAPABILITIES must list exactly the Capability variants in enum order. A new variant needs '
      + 'a row (in the same position); a row the enum does not have would be refused on save.',
  );
});

test('the table has unique, complete entries in non-empty groups', () => {
  assert.deepEqual(
    [...groups.map((group) => group.id)],
    ['Text', 'Audio', 'Images and video', 'Other'],
    'CAPABILITY_GROUPS must render the four headings in this order',
  );
  const groupIds = new Set(groups.map((group) => group.id));
  const seen = new Set();
  for (const capability of capabilities) {
    assert.ok(!seen.has(capability.id), `duplicate capability id ${capability.id}`);
    seen.add(capability.id);
    for (const key of ['label', 'group', 'hint']) {
      assert.equal(typeof capability[key], 'string', `${capability.id} is missing ${key}`);
    }
    assert.ok(capability.label.length > 0, `${capability.id} has an empty label`);
    assert.ok(capability.hint.length > 0, `${capability.id} has no hint`);
    assert.ok(groupIds.has(capability.group), `${capability.id} uses unknown group ${capability.group}`);
  }
  for (const group of groups) {
    assert.ok(group.hint.length > 0, `${group.id} has no group hint`);
    assert.ok(
      capabilities.some((capability) => capability.group === group.id),
      `${group.id} would render as an empty heading`,
    );
  }
});

test('capabilityIds keeps table order and drops unknown ids', () => {
  assert.deepEqual([...idsFor([])], []);
  assert.deepEqual([...idsFor(['models', 'chat'])], ['chat', 'models']);
  assert.deepEqual([...idsFor(['models', 'chat', 'models'])], ['chat', 'models'], 'duplicates must collapse');
  assert.deepEqual([...idsFor(new Set(['models', 'chat']))], ['chat', 'models'], 'a Set must be accepted');
  // Deliberately a drop, not a throw: a stale panel must not send a spelling this binary's server
  // cannot deserialise, but one stale id is no reason to lose the operator's other ticks.
  assert.deepEqual([...idsFor(['teleport'])], [], 'an unknown id must be dropped, not kept or thrown');
  assert.deepEqual([...idsFor(['models', 'teleport', 'chat'])], ['chat', 'models']);
});

test('the provider form saves capabilities and defaults to chat + models', () => {
  assert.match(source, /checkbox\.input\.setAttribute\('data-capability', capability\.id\)/);
  assert.match(source, /payload\.capabilities = selectedCapabilities;/);
  assert.match(source, /const DEFAULT_CAPABILITIES = \['chat', 'models'\]/);
  assert.match(
    source,
    /isEdit && Array\.isArray\(p\.capabilities\) && p\.capabilities\.length > 0\s*\? new Set\(p\.capabilities\)\s*: new Set\(DEFAULT_CAPABILITIES\)/,
    'edit should tick exactly the server array, while create and an absent/empty array default to chat + models',
  );
});

test('the form refuses a provider with nothing ticked', () => {
  assert.match(
    source,
    /field: 'capabilities',\s*message: 'A provider with no capabilities can serve nothing\. Tick at least one\.'/,
  );
});

test('the detail drawer has a capabilities row', () => {
  assert.match(
    source,
    /\['capabilities', h\('span', \{ class: 'mono' \}, declaredCapabilities\.join\(', '\)\)\]/,
  );
  assert.match(
    source,
    /Array\.isArray\(p\.capabilities\) && p\.capabilities\.length\s*\? p\.capabilities\s*: DEFAULT_CAPABILITIES/,
    'an absent or empty array should display the server default, chat and models',
  );
});
