// api.js — every HTTP call the panel makes goes through request().
//
// Centralised because three policies must be uniform:
//  1. Relative URLs only — the panel is served by the control plane itself; an absolute host
//     would break offline installs and leak the operator's IP.
//  2. Errors are always { error: { code, message, details? } }; details is the backend's
//     full list of validation problems and must survive to the UI untouched.
//  3. A 401 on any authenticated call means the session died: bounce to the login screen once,
//     centrally, instead of every view improvising. (The login call itself is exempt — a wrong
//     password is a normal, displayed error, not a session event.)

export class ApiError extends Error {
  constructor(status, code, message, details) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
    this.details = Array.isArray(details) ? details : details != null ? [details] : [];
  }
}

function envelope(status, payload) {
  const e = payload && typeof payload === 'object' && payload.error ? payload.error : {};
  const code = typeof e.code === 'string' ? e.code : `http-${status}`;
  const message = typeof e.message === 'string' && e.message
    ? e.message
    : `Request failed (${status}).`; // textContent-bound; no sanitisation needed
  return new ApiError(status, code, message, e.details);
}

async function safeJson(res) {
  try { return await res.json(); } catch { return undefined; }
}

let unauthorizedHandler = null;

/** Register the single 401 → login-screen transition. Called once by app.js. */
export function onUnauthorized(fn) {
  unauthorizedHandler = typeof fn === 'function' ? fn : null;
}

export async function request(method, path, { body, contentType = 'application/json', rawText = false, skipAuthRedirect = false } = {}) {
  let res;
  try {
    res = await fetch(path, {
      method,
      credentials: 'same-origin', // the session cookie must ride along; it never leaves this origin
      headers: body !== undefined ? { 'content-type': contentType } : undefined,
      body: typeof body === 'string' || body === undefined ? body : JSON.stringify(body),
    });
  } catch {
    // Network-level failure (control plane down). A typed error keeps view code simple.
    throw new ApiError(0, 'network', `Cannot reach the control plane (${path}). Is manarouter running?`, []);
  }

  if (res.status === 401 && !skipAuthRedirect && unauthorizedHandler) {
    unauthorizedHandler(); // switches the whole UI to the login screen
  }

  if (rawText) {
    const text = await res.text();
    if (!res.ok) throw envelope(res.status, safeJsonSync(text));
    return text;
  }

  const payload = await safeJson(res);
  if (!res.ok) throw envelope(res.status, payload);
  return payload;
}

function safeJsonSync(text) {
  try { return JSON.parse(text); } catch { return undefined; }
}

export const api = {
  request,
  get: (p) => request('GET', p),
  getRaw: (p) => request('GET', p, { rawText: true }),
  post: (p, body = {}) => request('POST', p, { body }),
  put: (p, body = {}) => request('PUT', p, { body }),
  putRaw: (p, text) => request('PUT', p, { body: String(text), contentType: 'text/plain' }),
  del: (p) => request('DELETE', p),
};
