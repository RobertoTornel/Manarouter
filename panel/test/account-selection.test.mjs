// Account tags, and the per-step account selection that uses them.
//
// These helpers are the panel's half of a rule the relay owns: an empty selection means *every*
// account, and a selector is an account id or a tag, compared case-insensitively. The panel has to
// agree with the server exactly, because a disagreement is silent — the step would look restricted
// in the drawer and route to everybody on the wire.
import assert from 'node:assert/strict';
import test from 'node:test';

import {
  parseTagList, tagListText, selectorNames, stepAccountSelection, describeAccountSelection,
} from '../format.js';

const accounts = [
  { id: 'acc_1', label: 'Alice laptop', tags: ['alice', 'work'] },
  { id: 'acc_2', label: 'Team box', tags: ['team'] },
  { id: 'acc_3', label: '', identity: 'bob@example', tags: [] },
];

test('a tag field accepts commas and newlines, and collapses case duplicates', () => {
  assert.deepEqual(parseTagList(' alice , work\nAlice '), ['alice', 'work']);
  assert.deepEqual(parseTagList(''), []);
  assert.deepEqual(parseTagList(null), []);
  // Order is the operator's, not sorted: the first tag naming a person wins, so reordering tags
  // is a real change and the panel must not quietly undo it.
  assert.deepEqual(parseTagList('z, a'), ['z', 'a']);
});

test('a tag list round-trips through its field text', () => {
  assert.equal(tagListText(['alice', 'work']), 'alice, work');
  assert.equal(tagListText([]), '');
  assert.equal(tagListText(undefined), '');
  assert.deepEqual(parseTagList(tagListText(['alice', 'work'])), ['alice', 'work']);
});

test('a selector names an account by id, by tag, or by tag: prefix', () => {
  assert.equal(selectorNames('acc_1', accounts[0]), true);
  assert.equal(selectorNames('ACC_1', accounts[0]), true);
  assert.equal(selectorNames('alice', accounts[0]), true);
  assert.equal(selectorNames('tag:alice', accounts[0]), true);
  assert.equal(selectorNames('TAG:Alice', accounts[0]), true);
  assert.equal(selectorNames('  work  ', accounts[0]), true);
  assert.equal(selectorNames('team', accounts[0]), false);
  assert.equal(selectorNames('tag:', accounts[0]), false);
  assert.equal(selectorNames('', accounts[0]), false);
});

test('the id wins over a tag of the same name', () => {
  // An account tagged with another account's id still yields to the id, exactly as the server does.
  const tagged = { id: 'acc_9', tags: ['acc_1'] };
  assert.equal(selectorNames('acc_1', tagged), true); // matched as a tag on this account
  assert.equal(selectorNames('acc_1', accounts[0]), true); // matched as its own id
});

test('no selection means every account, not no accounts', () => {
  const empty = stepAccountSelection([], accounts);
  assert.equal(empty.all, true);
  assert.deepEqual(empty.selected.map((a) => a.id), ['acc_1', 'acc_2', 'acc_3']);
  assert.deepEqual(empty.missing, []);
});

test('a selection narrows to the named accounts and reports the ones that name nothing', () => {
  const picked = stepAccountSelection(['alice', 'acc_3'], accounts);
  assert.equal(picked.all, false);
  assert.deepEqual(picked.selected.map((a) => a.id), ['acc_1', 'acc_3']);
  assert.deepEqual(picked.missing, []);

  const typo = stepAccountSelection(['alice', 'tema'], accounts);
  assert.deepEqual(typo.selected.map((a) => a.id), ['acc_1']);
  assert.deepEqual(typo.missing, ['tema']);
});

test('a selection describes itself in the operator\u2019s words', () => {
  assert.equal(describeAccountSelection([], accounts), 'every account');
  assert.equal(describeAccountSelection(['alice'], accounts), 'Alice laptop');
  assert.equal(describeAccountSelection(['acc_3'], accounts), 'bob@example');
  assert.equal(describeAccountSelection(['alice', 'team'], accounts), 'Alice laptop, Team box');
  assert.equal(
    describeAccountSelection(['alice', 'team', 'acc_3'], accounts, { max: 2 }),
    'Alice laptop, Team box +1');
});

test('a selection that matches nothing never widens back to every account', () => {
  // The one failure mode that must not happen: a typo silently routing to all accounts. It reads
  // as a gap, and the server refuses to compile the combo because of it.
  const none = describeAccountSelection(['nobody'], accounts);
  assert.equal(none, 'no account (unknown: nobody)');
  assert.equal(stepAccountSelection(['nobody'], accounts).all, false);
});
