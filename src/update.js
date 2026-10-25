// 版本更新检查: GitHub Releases(公开仓库免鉴权, api.github.com 支持 CORS, 纯前端即可)。
// 设计约定: 任何网络失败一律返回 null 由调用方静默处理 —— 更新检查绝不能影响正常使用。
import { RELEASES_API, RELEASES_PAGE, UPDATE_CHECK_TIMEOUT_MS } from "./config";

/* 版本号解析: 剥 v 前缀与预发布后缀(-beta), 按 "." 分段取数值 */
function verParts(v) {
  return String(v || "").trim().replace(/^v/i, "").split("-")[0]
    .split(".").map(n => parseInt(n, 10) || 0);
}

/* 语义化比较: latest 是否比 current 新(分段数值比, 短方补 0, 1.10.0 > 1.9.9) */
export function isNewer(latest, current) {
  const a = verParts(latest), b = verParts(current);
  const len = Math.max(a.length, b.length);
  for (let i = 0; i < len; i++) {
    const x = a[i] || 0, y = b[i] || 0;
    if (x !== y) return x > y;
  }
  return false;
}

/* 拉取最新 Release: 成功返回 { latest, notes, url }; 失败/无 Release 返回 null */
export async function fetchLatestRelease() {
  try {
    const ctl = new AbortController();
    const timer = setTimeout(() => ctl.abort(), UPDATE_CHECK_TIMEOUT_MS);
    const r = await fetch(RELEASES_API, {
      signal: ctl.signal,
      headers: { "Accept": "application/vnd.github+json" },
    });
    clearTimeout(timer);
    if (!r.ok) return null;   // 404(还没发过 Release)/403(限流)等 → 视为无结果
    const d = await r.json();
    if (!d || !d.tag_name) return null;
    return {
      latest: String(d.tag_name),
      notes: String(d.body || "").trim(),   // Release 说明, 弹框摘要展示
      url: d.html_url || RELEASES_PAGE,     // Release 页面(浏览器打开即可下载安装包)
    };
  } catch (e) {
    return null;   // 超时/断网/DNS 失败: 静默
  }
}
