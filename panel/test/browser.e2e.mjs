// Lightweight browser smoke test for the panel's guarded login and client-key lifecycle.
// Uses an isolated mock API server; never contacts the user's Manarouter instance.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';

const repoRoot = new URL('../../', import.meta.url);
const require = createRequire(import.meta.url);
let browserType;
const browserName = process.env.BROWSER || 'chromium';
try {
  const playwright = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
  browserType = browserName === 'firefox' ? playwright.firefox : playwright.chromium;
  if (!browserType) throw new Error(`Unsupported browser: ${browserName}`);
} catch (error) {
  console.error(`Playwright unavailable: ${error.message}`);
  console.error(`Install Playwright and its ${browserName} browser; set PLAYWRIGHT_MODULE only if it is not resolvable from this checkout.`);
  process.exit(2);
}

const panelRoot = fileURLToPath(repoRoot);
const browserExecutable = process.env[browserName.toUpperCase() + '_EXECUTABLE'];
const mime = { '.css': 'text/css; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.html': 'text/html; charset=utf-8' };
const sessionCookie = 'manarouter_session=isolated-e2e-session';
let authenticated = false;
let keyRecords = [];
let nextKeyId = 1;
let nextSecret = 1;
let resetCalls = 0;
const server = createServer(async (req, res) => {
  const url = new URL(req.url, 'http://127.0.0.1');
  if (url.pathname === '/panel' || url.pathname === '/panel/') {
    res.writeHead(302, { location: '/panel/index.html' });
    return res.end();
  }
  if (url.pathname.startsWith('/panel/')) {
    const rel = decodeURIComponent(url.pathname.slice('/panel/'.length));
    const filePath = join(panelRoot, 'panel', rel);
    if (!filePath.startsWith(join(panelRoot, 'panel') + '/') || filePath.includes('..')) {
      res.writeHead(400); return res.end();
    }
    try {
      const content = await readFile(filePath);
      res.writeHead(200, { 'content-type': mime[extname(filePath)] ?? 'application/octet-stream' });
      return res.end(content);
    } catch {
      res.writeHead(404); return res.end();
    }
  }
  const send = (status, body, headers = {}) => {
    res.writeHead(status, { 'content-type': 'application/json', ...headers });
    res.end(JSON.stringify(body));
  };
  const readJson = async () => {
    let text = '';
    for await (const chunk of req) text += chunk;
    return text ? JSON.parse(text) : {};
  };
  if (url.pathname === '/api/session' && req.method === 'GET') {
    const ok = req.headers.cookie?.includes(sessionCookie) && authenticated;
    return send(200, { authenticated: Boolean(ok), password_required: true });
  }
  if (url.pathname === '/api/login' && req.method === 'POST') {
    const body = await readJson();
    if (body.password !== 'test-only-password') {
      return send(401, { error: { code: 'invalid_password', message: 'Incorrect password.' } });
    }
    authenticated = true;
    return send(200, { authenticated: true }, { 'set-cookie': `${sessionCookie}; HttpOnly; Path=/; SameSite=Strict` });
  }
  if (!req.headers.cookie?.includes(sessionCookie) || !authenticated) {
    return send(401, { error: { code: 'unauthorized', message: 'Sign in required.' } });
  }
  if (url.pathname === '/api/health') return send(200, { version: 'e2e', uptime_secs: 1, snapshot: { digest: 'test' }, counts: { providers: 0, combos: 0, aliases: 0, keys: keyRecords.length, accounts: 0 } });
  if (url.pathname === '/api/ports') return send(200, { relay: { host: '127.0.0.1', port: 20900, running_port: 20900 }, control: { host: '127.0.0.1', port: 20901, running_port: 20901 } });
  if (url.pathname === '/api/keys' && req.method === 'GET') return send(200, { keys: keyRecords });
  if (url.pathname === '/api/keys' && req.method === 'POST') {
    const body = await readJson();
    const secret = `mr-e2e-secret-${nextSecret++}`;
    const key = { id: `key-${nextKeyId++}`, label: body.label ?? 'E2E key', enabled: true, prefix: secret.slice(0, 7), allow_models: [] };
    keyRecords.push(key);
    return send(201, { ...key, key: secret });
  }
  const rotate = url.pathname.match(/^\/api\/keys\/([^/]+)\/(?:rotate|regenerate)$/);
  if (rotate && req.method === 'POST') {
    resetCalls++;
    const key = keyRecords.find((item) => item.id === decodeURIComponent(rotate[1]));
    if (!key) return send(404, { error: { code: 'not_found', message: 'Key not found.' } });
    const secret = `mr-e2e-secret-${nextSecret++}`;
    key.prefix = secret.slice(0, 7);
    return send(200, { ...key, key: secret });
  }
  return send(404, { error: { code: 'not_found', message: `No mock route: ${req.method} ${url.pathname}` } });
});

const address = await new Promise((resolve, reject) => {
  server.once('error', reject);
  server.listen(0, '127.0.0.1', () => resolve(server.address()));
});
const browser = await browserType.launch({ headless: true, ...(browserExecutable ? { executablePath: browserExecutable } : {}) });
const context = await browser.newContext();
const page = await context.newPage();
page.on('console', (message) => { if (message.type() === 'error') console.error(`browser console: ${message.text()}`); });
page.on('pageerror', (error) => console.error(`browser page error: ${error.message}`));
try {
  await page.route('**/api/**', async (route) => {
    // Requests go to this test's ephemeral HTTP origin, not a live configured application.
    await route.continue();
  });
  await page.goto(`http://127.0.0.1:${address.port}/panel/index.html`);
  await page.getByRole('textbox', { name: 'Panel password' }).waitFor();
  await page.getByRole('textbox', { name: 'Panel password' }).fill('test-only-password');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await assertEventually(() => page.locator('#shell').evaluate((node) => !node.hidden));
  await page.getByRole('link', { name: 'Keys' }).click();
  await page.getByRole('button', { name: 'New key' }).click();
  await page.getByLabel('Label').fill('browser e2e key');
  await page.getByRole('button', { name: 'Create key' }).click();
  const createdDialog = page.getByRole('dialog');
  await assertEventually(() => createdDialog.locator('input[readonly]').inputValue().then((value) => value === 'mr-e2e-secret-1'));
  await createdDialog.getByRole('button', { name: 'I’ve stored it — close' }).click();

  const keyRow = page.getByRole('row').filter({ hasText: 'browser e2e key' });
  await keyRow.getByRole('button', { name: 'Rotate' }).click();
  const confirm = page.getByRole('dialog');
  await confirm.getByRole('button', { name: 'Rotate key' }).click();
  const rotatedDialog = page.getByRole('dialog');
  await assertEventually(() => rotatedDialog.locator('input[readonly]').inputValue().then((value) => value === 'mr-e2e-secret-2'));
  assert.equal(resetCalls, 1, 'rotation should call the mock API once');
  await rotatedDialog.getByRole('button', { name: 'I’ve stored it — close' }).click();
  assert.equal((await page.locator('body').innerText()).includes('mr-e2e-secret-2'), false, 'the plaintext key must not remain rendered after its reveal dialog closes');
  console.log('PASS: login, create one-time client key, confirm rotation, and one-time rotated-key reveal');
} finally {
  await browser.close();
  await new Promise((resolve) => server.close(resolve));
}

async function assertEventually(predicate, timeoutMs = 5000) {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    if (await predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  assert.fail('Timed out waiting for browser UI state');
}
