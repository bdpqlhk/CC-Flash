/* CC-Flash PWA 壳缓存: 只缓存静态页面与图标, API/WS 永远走网络 */
const CACHE = "ccflash-shell-v1";
self.addEventListener("install", (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(["/", "/logo.png", "/icon-192.png", "/icon-512.png", "/manifest.webmanifest"])));
  self.skipWaiting();
});
self.addEventListener("activate", (e) => {
  e.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim())
  );
});
self.addEventListener("fetch", (e) => {
  const url = new URL(e.request.url);
  if (e.request.method !== "GET" || url.pathname.startsWith("/api/") || url.pathname === "/ws") return;
  e.respondWith(
    fetch(e.request)
      .then((r) => {
        const copy = r.clone();
        caches.open(CACHE).then((c) => c.put(e.request, copy)).catch(() => {});
        return r;
      })
      .catch(() => caches.match(e.request))
  );
});
