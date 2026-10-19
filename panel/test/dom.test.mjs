// Shape checks for the shared widget layer in panel/dom.js (F8): `field()` and `toggle()`.
//
// dom.js builds real DOM and this panel has no dependencies, so there is no jsdom here. What
// follows is a ~40-line element stub that records attributes and children — enough to assert the
// contract the accessibility work rides on, which is structure, not layout:
//
//   * the row's visible text is a <label for> pointing at the input (I2: clicking the words ticks
//     the box),
//   * the "?" dot is a real <button>, carries the explanation in data-tip, and is NOT nested
//     inside the label (a <label> may not contain labelable elements besides its control),
//   * the control's aria-describedby names the .sr-only node that holds that same explanation,
//   * field() and toggle() produce identical wiring because both come through helpAffordance().
//
// Not covered by these Node structure checks: the tooltip bubble is positioned by dom.js against
// the live viewport, so it needs a browser. Manual verification is still needed: hover a "?" and
// Tab to one; check it is not clipped near the bottom of a scrolling drawer, stays inside a
// 360px-wide window, follows scroll/resize and works when the setting is RTL.
//
// Run: node --test panel/test/dom.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';

function makeEl(tag) {
  return {
    nodeType: 1,
    tagName: String(tag).toUpperCase(),
    attrs: {},
    children: [],
    dataset: {},
    listeners: {},
    className: '',
    id: '',
    textContent: '',
    style: {},
    checked: false,
    disabled: false,
    setAttribute(k, v) {
      this.attrs[k] = String(v);
      if (k === 'class') this.className = String(v);
      if (k === 'id') this.id = String(v);
    },
    getAttribute(k) {
      return Object.hasOwn(this.attrs, k) ? this.attrs[k] : null;
    },
    addEventListener(type, fn) {
      (this.listeners[type] ||= []).push(fn);
    },
    append(...kids) {
      for (const kid of kids) this.children.push(kid);
    },
  };
}

// Installed before dom.js is imported: dom.js touches no document at module scope, but the help
// affordance appends its tooltip layer to <body> the first time a field or toggle asks for help.
globalThis.document = {
  createElement: makeEl,
  createTextNode: (text) => ({ nodeType: 3, textContent: String(text) }),
  body: makeEl('body'),
  documentElement: makeEl('html'),
  activeElement: null,
  addEventListener() {},
};
globalThis.window = { addEventListener() {} };

const { field, toggle } = await import('../dom.js');

function walk(node, out = []) {
  out.push(node);
  for (const kid of node.children ?? []) if (kid.nodeType === 1) walk(kid, out);
  return out;
}

const byClass = (root, cls) => walk(root).filter((n) => n.className === cls);
const text = (node) =>
  node.nodeType === 3
    ? node.textContent
    : (node.children ?? []).map(text).join('');

test('toggle() links its visible text to the input with a real label', () => {
  const { input, row } = toggle('Prewarm connections');
  assert.equal(row.className, 'check-row');
  const label = walk(row).find((n) => n.tagName === 'LABEL');
  assert.equal(label.getAttribute('for'), input.id);
  assert.ok(input.id, 'input needs an id for the label to point at');
  assert.equal(text(label), 'Prewarm connections');
  // The label is a sibling of the input: clicking either one reaches the same control.
  assert.ok(row.children.includes(input));
  assert.ok(row.children.includes(label));
});

test('toggle() without help renders no dot and no description', () => {
  const { row } = toggle('Disabled by default');
  assert.equal(byClass(row, 'help-dot').length, 0);
  assert.equal(byClass(row, 'sr-only').length, 0);
});

test('toggle() help: one button, the text in data-tip, and the input described by the sr-only span', () => {
  const help = 'Opening a connection at startup makes the first request faster but costs an idle socket.';
  const { input, row } = toggle('Prewarm connections', { help });
  const dots = byClass(row, 'help-dot');
  assert.equal(dots.length, 1, 'exactly one affordance, built once');
  const dot = dots[0];
  assert.equal(dot.tagName, 'BUTTON');
  assert.equal(dot.getAttribute('type'), 'button');
  assert.equal(dot.getAttribute('data-tip'), help);
  assert.equal(dot.getAttribute('aria-label'), 'Help: Prewarm connections');
  // The button must not be labelable content inside the label.
  const label = walk(row).find((n) => n.tagName === 'LABEL');
  assert.ok(!walk(label).includes(dot), 'help dot must not be nested in the label');
  // The keyboard/screen-reader half: the control points at the same text.
  const desc = byClass(row, 'sr-only');
  assert.equal(desc.length, 1);
  assert.equal(input.getAttribute('aria-describedby'), desc[0].getAttribute('id'));
  assert.equal(text(desc[0]), help);
  assert.ok(row.children.includes(dot));
  assert.ok(row.children.includes(desc[0]));
});

test('field() and toggle() share one help implementation', () => {
  const help = 'Shared explanation text.';
  const { input: toggleInput, row } = toggle('Toggle text', { help });
  const input = makeEl('input');
  const box = field('Field text', input, { help });
  const [toggleDot] = byClass(row, 'help-dot');
  const [fieldDot] = byClass(box, 'help-dot');
  for (const dot of [toggleDot, fieldDot]) {
    assert.ok(dot, 'both helpers must render the dot');
    assert.equal(dot.className, 'help-dot');
    assert.equal(dot.getAttribute('data-tip'), help);
    assert.equal(dot.tagName, 'BUTTON');
  }
  // Same wiring, different placement: the field's dot sits on a label line of its own.
  assert.equal(byClass(box, 'label-line').length, 1);
  assert.equal(byClass(row, 'label-line').length, 0);
  assert.equal(input.getAttribute('aria-describedby'), byClass(box, 'sr-only')[0].getAttribute('id'));
  assert.equal(toggleInput.getAttribute('aria-describedby'), byClass(row, 'sr-only')[0].getAttribute('id'));
  assert.notEqual(
    byClass(box, 'sr-only')[0].getAttribute('id'),
    byClass(row, 'sr-only')[0].getAttribute('id'),
    'each field gets its own description id',
  );
});

test('field() without help keeps the plain label structure', () => {
  const input = makeEl('input');
  const box = field('Model id', input, { hint: 'provider/model' });
  assert.equal(box.className, 'field');
  assert.equal(byClass(box, 'help-dot').length, 0);
  assert.equal(byClass(box, 'label-line').length, 0);
  const label = box.children.find((n) => n.tagName === 'LABEL');
  assert.equal(label.getAttribute('for'), input.id);
  assert.equal(byClass(box, 'hint').length, 1);
});

test('toggle() honours checked, disabled, id, onChange and unique ids', () => {
  let changes = 0;
  const { input, row } = toggle('Enabled', {
    checked: true,
    disabled: true,
    id: 'models-hidden-only',
    onChange: () => { changes += 1; },
  });
  assert.equal(input.id, 'models-hidden-only');
  assert.equal(input.checked, true);
  assert.equal(input.disabled, true);
  assert.equal(input.getAttribute('id'), 'models-hidden-only');
  assert.equal(walk(row).find((n) => n.tagName === 'LABEL').getAttribute('for'), 'models-hidden-only');
  input.listeners.change[0]();
  assert.equal(changes, 1);

  const a = toggle('A');
  const b = toggle('B');
  assert.notEqual(a.input.id, b.input.id);
  assert.equal(a.input.checked, false);
  assert.equal(a.input.disabled, false);
});

test('toggle() hint goes on its own line inside the row', () => {
  const { row } = toggle('Sticky window', { hint: 'Combo-wide; 0 disables.' });
  const hints = byClass(row, 'hint');
  assert.equal(hints.length, 1);
  assert.equal(text(hints[0]), 'Combo-wide; 0 disables.');
  assert.ok(row.children.includes(hints[0]));
});
