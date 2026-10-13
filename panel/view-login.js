// view-login.js — full-screen login, shown when /api/session says password_required && !authenticated.
// Server error messages (including the backend's details list) are rendered via textContent only.

import { api } from './api.js';
import { h, field, errorBlock, clearFieldErrors } from './dom.js';

export function renderLogin(root, { onSuccess }) {
  const form = h('form', { class: 'card', style: 'max-width:360px;margin:18vh auto 0;', novalidate: true });
  const pwd = h('input', { class: 'input', type: 'password', autocomplete: 'current-password', required: true, 'aria-label': 'Panel password' });
  const submit = h('button', { class: 'btn btn-primary', type: 'submit' }, 'Sign in');

  form.append(
    h('h2', { style: 'margin:0 0 2px;font-size:16px;' }, 'Manarouter'),
    h('p', { class: 'hint', style: 'margin:0 0 12px;' }, 'This control plane is password-protected.'),
    field('Password', pwd),
    submit,
  );

  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    clearFieldErrors(form);
    submit.disabled = true;
    submit.textContent = 'Signing in…';
    try {
      // skipAuthRedirect: a wrong password is a normal error to display, not a session event
      // that should trigger the global 401 → login transition.
      await api.request('POST', '/api/login', { body: { password: pwd.value }, skipAuthRedirect: true });
      pwd.value = '';
      onSuccess();
    } catch (err) {
      const box = errorBlock(err);
      box.style.marginTop = '10px';
      form.insertBefore(box, submit);
      submit.disabled = false;
      submit.textContent = 'Sign in';
      pwd.focus();
    }
  });

  root.append(form);
  pwd.focus();
}
