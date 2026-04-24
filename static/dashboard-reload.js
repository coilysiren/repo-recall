// Reload the dashboard when the server signals that a periodic refresh
// completed. Opens a direct WebSocket to /ws (in parallel to the htmx-ext-ws
// subscription) and watches for the `data-reload-trigger` sentinel the
// server sends as the last message of every successful run_refresh.
//
// A parallel socket is simpler and more robust than tapping the htmx-ext-ws
// swap pipeline — we don't care whether the OOB swap lands on the DOM,
// we only care that the server said "fresh data is ready."
//
// Scoped to the dashboard only: detail pages don't include this script,
// so they won't reload mid-read. Reconnects on drop so a restart or tab
// sleep doesn't silently disable the loop.
(function () {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const url = proto + "//" + location.host + "/ws";
  function connect() {
    const ws = new WebSocket(url);
    ws.addEventListener("message", (e) => {
      if (typeof e.data === "string" && e.data.indexOf("data-reload-trigger") !== -1) {
        location.reload();
      }
    });
    ws.addEventListener("close", () => {
      // Reconnect after a short delay so a server restart or transient
      // drop doesn't leave the dashboard stranded without auto-reload.
      setTimeout(connect, 1000);
    });
    ws.addEventListener("error", () => ws.close());
  }
  connect();
})();
