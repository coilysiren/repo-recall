// Tick the "next refresh" countdown once a second.
//
// Server renders #next-refresh-countdown with data-deadline-unix=<seconds
// since epoch>; this script subtracts wall-clock now() and re-renders the
// text. Once the deadline hits, the periodic refresh is firing on the
// server — switch to "scanning…" until dashboard-reload.js reloads the
// page on completion (which re-renders this element with a fresh deadline).
(function () {
  const el = document.getElementById("next-refresh-countdown");
  if (!el) return;
  const deadline = parseInt(el.dataset.deadlineUnix, 10);
  if (!Number.isFinite(deadline)) return;
  function fmt(s) {
    if (s <= 0) return "scanning…";
    if (s < 60) return s + "s";
    const m = Math.floor(s / 60);
    const r = s % 60;
    return r === 0 ? m + "m" : m + "m " + r + "s";
  }
  function tick() {
    const now = Math.floor(Date.now() / 1000);
    el.textContent = fmt(deadline - now);
  }
  tick();
  setInterval(tick, 1000);
})();
