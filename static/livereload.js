(function () {
  let hadConnection = false;
  let isReconnecting = false;
  function connect() {
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    const ws = new WebSocket(proto + "//" + location.host + "/livereload");
    ws.onopen = () => {
      if (isReconnecting) {
        location.reload();
        return;
      }
      hadConnection = true;
    };
    ws.onclose = () => {
      if (hadConnection) isReconnecting = true;
      setTimeout(connect, 400);
    };
    ws.onerror = () => ws.close();
  }
  connect();
})();
