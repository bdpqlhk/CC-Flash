// 老内核守门(必须 ES5 且不依赖 eval —— 生产 CSP script-src 'self' 无 unsafe-eval,
// new Function 探针在打包版会抛 EvalError 误伤现代内核)。modern.js 未置位 = 内核不支持 ?.。
(function () {
  if (window.__ccModern) return;
  function showGate() {
    var app = document.getElementById("app");
    if (!app || app.firstChild) return;   // 已有内容(模块意外执行)不覆盖
    app.innerHTML =
      '<div style="max-width:420px;margin:15vh auto;padding:26px 24px;background:#161b26;' +
      'border:1px solid rgba(255,255,255,.08);border-radius:14px;color:#e1e4ed;' +
      'font:14px/1.8 -apple-system,BlinkMacSystemFont,PingFang SC,Microsoft YaHei,sans-serif;text-align:center">' +
      '<div style="font-size:40px;margin-bottom:10px">🧩</div>' +
      '<h1 style="font-size:18px;margin:0 0 8px">浏览器内核过旧</h1>' +
      '<p style="color:#9ca3b8;margin:0">CC-Flash 需要较新的浏览器内核（约 2020 年起的 Chrome / Edge / Safari / Firefox 及等价国产套壳）。' +
      '请升级浏览器，或改用桌面版客户端。</p></div>';
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", showGate);
  else showGate();
})();
