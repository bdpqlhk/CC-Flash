// 版本更新检查: GitHub Releases(公开仓库免鉴权, api.github.com 支持 CORS, 纯前端即可)。
// 设计约定: 任何网络失败一律返回 null 由调用方静默处理 —— 更新检查绝不能影响正常使用。
import { GITHUB_REPO, RELEASES_API, RELEASES_PAGE, UPDATE_CHECK_TIMEOUT_MS } from "./config";

/* 版本号解析: 剥 v 前缀与预发布后缀(-beta), 按 "." 分段取数值 */
function verParts(v) {
  return String(v || "").trim().replace(/^v/i, "").split("-")[0]
    .split(".").map(n => parseInt(n, 10) || 0);
}

/* 预发布后缀(无则空串): 语义化版本中 2.0.0-beta < 2.0.0 */
function verPre(v) {
  const s = String(v || "").trim().replace(/^v/i, "");
  const i = s.indexOf("-");
  return i >= 0 ? s.slice(i + 1) : "";
}

/* 语义化比较: latest 是否比 current 新(分段数值比, 短方补 0, 1.10.0 > 1.9.9) */
export function isNewer(latest, current) {
  const a = verParts(latest), b = verParts(current);
  const len = Math.max(a.length, b.length);
  for (let i = 0; i < len; i++) {
    const x = a[i] || 0, y = b[i] || 0;
    if (x !== y) return x > y;
  }
  // 段值相等: 预发布后缀视为更旧 —— 否则分发过 2.0.0-beta 的内测用户
  // 在 2.0.0 正式版发布后永远收不到升级提示
  const ap = verPre(latest), bp = verPre(current);
  if (ap === bp) return false;
  if (!ap) return true;    // latest 是正式版, current 带后缀 → 更新
  if (!bp) return false;   // latest 带后缀, current 是正式版 → 不更新
  // 双方都带后缀: 按 semver 逐段比较 —— 纯数字段按数值(beta.10 > beta.2, 旧的整串
  // 字序会判反), 数字段优先级低于字母段, 同类字母段按 ASCII
  const pa = ap.split("."), pb = bp.split(".");
  const n = Math.max(pa.length, pb.length);
  for (let i = 0; i < n; i++) {
    const x = pa[i] || "", y = pb[i] || "";
    if (x === y) continue;
    if (x === "") return false;   // 短前缀更旧: 2.0.0-rc < 2.0.0-rc.1
    if (y === "") return true;
    const nx = /^\d+$/.test(x), ny = /^\d+$/.test(y);
    if (nx && ny) return Number(x) > Number(y);
    if (nx !== ny) return !nx;   // 数字段 < 字母段(semver 规则)
    return x > y;
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
    const list = Array.isArray(d) ? d : [d];
    // 列表按发布时间倒序: 跳过草稿与 prerelease。未签名时 publish 会失败并保持
    // 草稿(见 release.yml), 不会再出现"把未签名/打不开的包当更新推给用户"。
    const rel = list.find((x) => x && !x.draft && !x.prerelease && x.tag_name);
    if (!rel) return null;
    // 不信任 API 返回的任意跳转地址: 只接受本仓库 Releases 前缀, 否则回落到固定页面,
    // 防止上游/镜像被接管后把用户导向任意下载站
    const expected = `https://github.com/${GITHUB_REPO}/releases/`;
    const rawUrl = typeof rel.html_url === "string" ? rel.html_url : "";
    return {
      latest: String(rel.tag_name),
      notes: String(rel.body || "").trim(),   // Release 说明, 弹框摘要展示
      url: rawUrl.startsWith(expected) ? rawUrl : RELEASES_PAGE,
    };
  } catch (e) {
    return null;   // 超时/断网/DNS 失败: 静默
  }
}
