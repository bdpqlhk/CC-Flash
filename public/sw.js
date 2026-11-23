/* CC-Flash PWA 壳缓存
 * - 只缓存界面壳与静态资源; /api、/ws 永远走网络(文件/聊天必须实时)
 * - 导航请求 network-first: 服务可用时永远拿最新页面, 断网时回退缓存的壳
 * - 带内容 hash 的 /assets/* cache-first; 其余静态资源 stale-while-revalidate
 * 注意: Service Worker 只在 https 或 localhost 下可用。http://局域网IP
 * 不是安全上下文, 浏览器会直接拒绝注册 —— 这属于浏览器策略, 不是应用 bug。
 */
const CACHE = "ccflash-shell-v3";
const SHELL = ["/", "/logo.png", "/icon-192.png", "/icon-512.png", "/manifest.webmanifest"];

self.addEventListener("install", (e) => {
  e.waitUntil(
    caches.open(CACHE)
      .then((c) => c.addAll(SHELL))
      .catch(() => {})
      .then(() => self.skipWaiting())
  );
});

self.addEventListener("activate", (e) => {
  e.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim())
  );
});

self.addEventListener("message", (e) => {
  if (e.data && e.data.type === "SKIP_WAITING") self.skipWaiting();
});

function isApi(url) {
  return url.pathname.startsWith("/api/") || url.pathname === "/ws";
}

self.addEventListener("fetch", (e) => {
  const req = e.request;
  if (req.method !== "GET") return;
  const url = new URL(req.url);
  if (url.origin !== self.location.origin || isApi(url)) return;
  // 下载走浏览器原生流式下载, 不能被 SW 转成内存响应
  if (url.pathname.startsWith("/api/download/")) return;

  // 页面导航: network-first, 失败时回退缓存的 index
  if (req.mode === "navigate") {
    e.respondWith(
      fetch(req)
        .then((r) => {
          if (r && r.ok) {
            const copy = r.clone();
            e.waitUntil(caches.open(CACHE).then((c) => c.put("/", copy)).catch(() => {}));
          }
          return r;
        })
        .catch(() => caches.match(req).then((r) => r || caches.match("/")))
    );
    return;
  }

  // 构建产物带内容 hash: cache-first 最快且安全
  if (url.pathname.startsWith("/assets/")) {
    e.respondWith(
      caches.match(req).then((hit) => hit || fetch(req).then((r) => {
        if (r && r.ok) {
          const copy = r.clone();
          e.waitUntil(caches.open(CACHE).then((c) => c.put(req, copy)).catch(() => {}));
        }
        return r;
      }))
    );
    return;
  }

  // 图标/静态文件: 先给缓存, 后台更新
  e.respondWith(
    caches.match(req).then((hit) => {
      const net = fetch(req).then((r) => {
        if (r && r.ok) {
          const copy = r.clone();
          e.waitUntil(caches.open(CACHE).then((c) => c.put(req, copy)).catch(() => {}));
        }
        return r;
      }).catch(() => hit);
      return hit || net;
    })
  );
});
