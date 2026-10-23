// dialog.js — modal dialogs: generic confirm, and the show-once client-key reveal.
//
// Accessibility contract for every dialog here: role="dialog" + aria-modal, labelled by its
// title; focus moves in on open and returns to the opener on close; Tab is trapped inside;
// Escape cancels. Built with DOM APIs only (no innerHTML) — dialog text can contain server data.

import { h, icon } from './dom.js';

const FOCUSABLE = 'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])';

let dialogSeq = 0;

/** Append a uniquely-identified title heading and name the dialog with it. */
function addTitle(card, text, cls) {
  const id = `dlg-title-${++dialogSeq}`;
  card.setAttribute('aria-labelledby', id);
  return h('h2', { id, class: cls }, text);
}

/**
 * Open a modal. build(content, close) fills the card; close(result) resolves and tears down.
 * Every caller gets focus management and the Escape/Tab handling for free.
 */
function openDialog(root, build, { dismissable = true } = {}) {
  const opener = document.activeElement;
  const backdrop = h('div', { class: 'dialog-backdrop' });
  const card = h('div', {
    class: 'dialog',
    role: 'dialog',
    'aria-modal': 'true',
  });
  backdrop.append(card);

  function close(result) {
    document.removeEventListener('keydown', onKey, true);
    backdrop.remove();
    if (opener && typeof opener.focus === 'function') opener.focus();
    if (result === undefined) result = false; // confirm dialogs resolve false on dismissal
    resolve(result);
  }

  let resolve;
  const done = new Promise((res) => { resolve = res; });

  function onKey(e) {
    if (e.key === 'Escape' && dismissable) {
      e.stopPropagation();
      close(false);
      return;
    }
    if (e.key === 'Tab') {
      // Focus trap: cycle within the dialog so keyboard users cannot fall behind the modal.
      const items = [...card.querySelectorAll(FOCUSABLE)].filter((el) => !el.disabled);
      if (!items.length) return;
      // Focus can sit on the body after a backdrop click; without this the next Tab would
      // walk the page behind the modal instead of staying in it.
      if (!card.contains(document.activeElement)) {
        items[0].focus();
        e.preventDefault();
        return;
      }
      const first = items[0];
      const last = items[items.length - 1];
      if (e.shiftKey && document.activeElement === first) { last.focus(); e.preventDefault(); }
      else if (!e.shiftKey && document.activeElement === last) { first.focus(); e.preventDefault(); }
    }
  }

  document.addEventListener('keydown', onKey, true);
  build(card, close);
  root.append(backdrop);

  // Initial focus: on a danger confirm the cancel button is pre-focused — the safe choice.
  const initial = card.querySelector('[data-autofocus]') ?? card.querySelector(FOCUSABLE);
  if (initial) initial.focus();

  return { close, done };
}

/**
 * Confirm a (usually destructive) action. Resolves true only if the confirm button was pressed.
 *
 * WHY this exists as a shared primitive: every destructive action — deleting a provider with its
 * accounts' credentials, deleting a key clients authenticate with, restarting the process — must
 * pass through an explicit, keyboard-operable confirmation. `warnLines` carries the specific
 * consequence (e.g. "removes 2 accounts and their credentials") in amber.
 */
export function confirmDialog(root, {
  title,
  message,
  warnLines = [],
  confirmLabel = 'Confirm',
  cancelLabel = 'Cancel',
  danger = true,
} = {}) {
  const { done, close } = openDialog(root, (card, close) => {
    card.append(
      addTitle(card, title),
      ...(message ? [h('p', { class: 'dialog-msg' }, message)] : []),
      ...warnLines.map((w) => h('p', { class: 'dialog-warn' }, w)),
      h('div', { class: 'dialog-actions' },
        h('button', {
          class: 'btn',
          type: 'button',
          'data-autofocus': danger ? '' : null,
          onclick: () => close(false),
        }, cancelLabel),
        h('button', {
          class: `btn ${danger ? 'btn-danger' : 'btn-primary'}`,
          type: 'button',
          'data-autofocus': danger ? null : '',
          onclick: () => close(true),
        }, confirmLabel),
      ),
    );
  }, { dismissable: true });
  return done;
}

/**
 * The one-time client-key reveal (POST /api/keys returns the plaintext exactly once).
 *
 * The backdrop is deliberately NOT click-to-dismiss: an accidental click must not destroy the
 * only copy of the secret. Escape still works — that is a deliberate keystroke. The key sits in
 * a readonly <input> so Copy/select is trivial; the value travels via .value, never markup.
 */
export function secretKeyDialog(root, { key, label = 'Client key' }) {
  return openDialog(root, (card, close) => {
    const input = h('input', {
      class: 'input mono',
      type: 'text',
      readonly: true,
      value: String(key ?? ''),
      'aria-label': `${label} — shown only once`,
      spellcheck: 'false',
    });
    const copyBtn = h('button', { class: 'btn', type: 'button' }, 'Copy');
    copyBtn.addEventListener('click', async () => {
      let ok = false;
      try {
        await navigator.clipboard.writeText(input.value);
        ok = true;
      } catch {
        // Clipboard API can be unavailable (permissions/older builds); fall back to select+execCommand.
        input.focus();
        input.select();
        try { ok = document.execCommand('copy'); } catch { ok = false; }
      }
      copyBtn.textContent = ok ? 'Copied' : 'Select & copy manually';
      setTimeout(() => { copyBtn.textContent = 'Copy'; }, 2000);
    });

    card.append(
      addTitle(card, label),
      h('p', { class: 'dialog-warn' },
        'This is the only time the full key is shown. Copy it and store it now — it cannot be retrieved again.'),
      h('div', { class: 'key-reveal' }, input, copyBtn),
      h('div', { class: 'dialog-actions' },
        h('button', {
          class: 'btn btn-primary',
          type: 'button',
          'data-autofocus': '',
          onclick: () => close(true),
        }, 'I\u2019ve stored it — close'),
      ),
    );
  }, { dismissable: true }).done;
}

/** Small close (✕) button used by drawers; aria-label required because it has no text. */
export function closeButton(onClick, label = 'Close') {
  const b = h('button', { class: 'icon-btn', type: 'button', 'aria-label': label }, icon('x'));
  b.addEventListener('click', onClick);
  return b;
}

/**
 * A modal whose body the caller builds: `build(body, close)` fills it, and the returned promise
 * resolves with whatever `close` was called with (dismissal resolves `false`).
 *
 * This exists so a view can own its own modal *content* while the focus trap, Escape handling,
 * backdrop teardown and title plumbing stay in one place. A second `openDialog` inside a view
 * would be a second copy of the accessibility contract this file's header promises to keep.
 */
export function contentDialog(root, title, build, { dismissable = true } = {}) {
  return openDialog(root, (card, close) => {
    const body = h('div', { class: 'dialog-body' });
    card.append(
      h('div', { class: 'dialog-head' },
        addTitle(card, title, 'dialog-title'),
        closeButton(() => close(false), 'Close')),
      body,
    );
    build(body, close);
  }, { dismissable }).done;
}
