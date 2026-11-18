// Where the panel aims its relay reachability probe.
//
// This is a small pure function with an outsized failure mode: aimed at 127.0.0.1 while the panel
// is open on another device, it probes the *browser's* loopback and reports a perfectly healthy
// relay as unreachable — which is exactly the bug that made "the panel is up but the router is not
// reachable" when the panel was reached over the tailnet.
import test from 'node:test';
import assert from 'node:assert/strict';
import { relayProbeUrl } from '../format.js';

const relay = (host, port = 20900) => ({ host, port });

test('a loopback relay is probed on loopback when the panel is local', () => {
  assert.equal(relayProbeUrl(relay('127.0.0.1'), '127.0.0.1'), 'http://127.0.0.1:20900/healthz');
});

test('a loopback relay is probed through the page host when the panel is remote', () => {
  assert.equal(relayProbeUrl(relay('127.0.0.1'), '100.104.1.69'), 'http://100.104.1.69:20900/healthz');
});

test('a wildcard listen address follows the page host rather than a placeholder', () => {
  assert.equal(relayProbeUrl(relay('0.0.0.0'), '100.104.1.69'), 'http://100.104.1.69:20900/healthz');
  assert.equal(relayProbeUrl(relay('0.0.0.0'), '127.0.0.1'), 'http://127.0.0.1:20900/healthz');
  assert.equal(relayProbeUrl(relay('::'), '127.0.0.1'), 'http://127.0.0.1:20900/healthz');
});

test('an explicit non-loopback address is probed as configured', () => {
  assert.equal(relayProbeUrl(relay('100.104.1.69'), '127.0.0.1'), 'http://100.104.1.69:20900/healthz');
});

test('an HTTPS panel probes the relay through the same host on 443', () => {
  // A tailnet panel is HTTPS; a plain-http probe from that page would be blocked as mixed content.
  assert.equal(relayProbeUrl(relay('0.0.0.0'), 'pc.nutria-ordinal.ts.net', 'https:'),
    'https://pc.nutria-ordinal.ts.net/healthz');
  assert.equal(relayProbeUrl(relay('127.0.0.1'), 'pc.nutria-ordinal.ts.net', 'https:'),
    'https://pc.nutria-ordinal.ts.net/healthz');
  // A local HTTP page keeps probing the loopback listener with its raw port.
  assert.equal(relayProbeUrl(relay('127.0.0.1'), '127.0.0.1', 'http:'),
    'http://127.0.0.1:20900/healthz');
});

test('an IPv6 literal stays bracketed, and a missing port yields null', () => {
  assert.equal(relayProbeUrl(relay('fd7a:115c:a1e0::1'), '127.0.0.1'), 'http://[fd7a:115c:a1e0::1]:20900/healthz');
  assert.equal(relayProbeUrl({ host: '127.0.0.1' }), null);
  assert.equal(relayProbeUrl(null), null);
});

test('a running port wins over the configured one', () => {
  assert.equal(
    relayProbeUrl({ host: '127.0.0.1', port: 20900, running_port: 21900 }, '127.0.0.1'),
    'http://127.0.0.1:21900/healthz',
  );
});
