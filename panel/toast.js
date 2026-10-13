// toast.js — global notification area (#toasts in index.html).
//
// Success/error/warning/info map 1:1 onto the semantic colours; nothing here is decorative.
// All message text — including server error messages — is inserted via text nodes/props only
// (h does this), never innerHTML: error strings come from the server.

import { h, icon, detailsList } from './dom.js';

const TTL_MS = { success: 6000, info: 6000, warning: 9000, error: 10000 };

export function createToasts(container) {
  function show(kind, message, err = null) {
    const box = h('div', {
      class: `toast toast-${kind}`,
      // Errors are announced assertively; the rest politely.
      role: kind === 'error' ? 'alert' : 'status',
    });
    box.append(h('div', { class: 'toast-msg' }, message));
    if (err && err.details && err.details.length) {
      // Validation problems are listed in full, per the backend's all-problems-at-once contract.
      box.append(detailsList(err.details));
    }
    const close = h('button', {
      class: 'icon-btn toast-close',
      type: 'button',
      'aria-label': 'Dismiss notification',
    }, icon('x'));
    close.addEventListener('click', () => dismiss());
    box.append(close);
    container.append(box);
    // Cap the stack so a burst of errors during an outage cannot bury the screen.
    while (container.children.length > 6) container.firstElementChild.remove();

    const timer = setTimeout(dismiss, TTL_MS[kind] ?? 6000);
    function dismiss() {
      clearTimeout(timer);
      box.classList.add('toast-out');
      setTimeout(() => box.remove(), 160);
    }
  }

  return {
    success: (m, e) => show('success', m, e),
    error: (m, e) => show('error', m, e),
    warning: (m, e) => show('warning', m, e),
    info: (m, e) => show('info', m, e),
  };
}
