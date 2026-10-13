// poll.js — the health poll scheduler.
//
// WHY polling stops while the tab is hidden: the Overview stays live by polling GET /api/health
// every ~5 s. An admin panel left open in a background tab for days must not keep waking a
// loopback socket that nobody is looking at — that is precisely the kind of thoughtless waste
// this project exists to avoid. visibilitychange pauses the timer entirely (no fetches while
// hidden); returning to the tab fires one immediate tick so the operator never stares at stale
// numbers.
//
// Dependencies are injected (doc, win, tick) so the scheduler can be exercised without a real
// browser — see panel/README.md.

export function createPoller({ tick, intervalMs = 5000, doc = document, win = window } = {}) {
  let timer = null;
  let stopped = true;

  async function run() {
    if (stopped) return;
    if (doc.visibilityState !== 'visible') { timer = null; return; } // hidden: stay stopped
    try {
      await tick();
    } catch {
      // tick() reports its own failures to the UI; the poll loop itself never throws.
    }
    if (!stopped) schedule();
  }

  function schedule() {
    // Chained setTimeout, not setInterval: a slow tick can never overlap itself.
    clearTimeout(timer);
    timer = setTimeout(() => { timer = null; run(); }, intervalMs);
  }

  function onVisibility() {
    if (stopped) return;
    if (doc.visibilityState === 'visible') run(); // immediate refresh on return
    else { clearTimeout(timer); timer = null; }   // full stop while hidden
  }

  return {
    start() {
      if (!stopped) return;
      stopped = false;
      win.addEventListener('visibilitychange', onVisibility);
      run();
    },
    stop() {
      stopped = true;
      clearTimeout(timer);
      timer = null;
      win.removeEventListener('visibilitychange', onVisibility);
    },
    get running() { return !stopped; },
  };
}
