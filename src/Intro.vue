<script setup>
import { ref, onMounted, onUnmounted, nextTick } from "vue";
import { GITHUB_REPO, RELEASES_PAGE, FOOTER_TEXT } from "./config";

const props = defineProps({
  running: Boolean,
  shareUrl: String,
  qrData: String,
  copyText: Function,
  openExternal: Function,
});
const emit = defineEmits(["close", "service", "transfer"]);

const INTRO_FAQS = [
  { q: "手机需要安装 App 吗？", a: "不用。同一 Wi-Fi 下扫码或输入电脑地址，浏览器即可上传、下载、聊天。Android 也可安装 APK，作为独立本机节点；安装包已修复 versionCode/versionName，可正常覆盖升级。" },
  { q: "文件会上传到外网吗？", a: "不会。服务只监听你的电脑/手机，所有传输都在局域网内，不经过第三方服务器；断外网也能用。" },
  { q: "关闭窗口后服务会停吗？", a: "不会。关窗只是收进托盘，服务继续运行；真正退出请走托盘菜单。Android 用 connectedDevice 前台服务 + Wi-Fi/CPU 锁保活，并在首次启动申请忽略电池优化。" },
  { q: "支持哪些平台？", a: "macOS、Windows、Android App，以及电脑/手机浏览器。Windows 安装包使用 WebView2 引导器，Windows 10/11 通常已自带运行时；iOS 暂无原生包，用 Safari 打开电脑地址即可。" },
  { q: "能同时发给多台设备吗？", a: "可以。设备页/设备栏点「多选」，勾选多台后发送文本或拖入文件，会依次投递到每台设备；某台失败会停下并提示，避免配对框互相覆盖。" },
  { q: "文件消息刷新后会丢吗？", a: "不会。上传成功的文件会记录为本机服务的文件消息（最近 200 条），刷新浏览器或重启 App 后聊天时间线仍保留；删除文件时卡片同步消失。" },
  { q: "文件列表在哪里？", a: "桌面点房间头文件夹图标，手机 App 点底部「文件」，手机浏览器点顶部文件夹。上传的文件也会同时出现在聊天里。" },
  { q: "能直接预览文件吗？", a: "图片、PDF、文本和音视频可在应用内预览；Office、压缩包等需下载后查看。html/svg/xml 为安全起见不以网页打开。" },
  { q: "设备发现不到怎么办？", a: "确认两台设备在同一 Wi-Fi/子网；macOS 允许「本地网络」，Windows 防火墙放行 UDP 53318-53321；手机熄屏后要能被发现，请允许忽略电池优化并保持 App 后台运行；仍不行可在故障诊断里按 IP 单播探测，或直接手动发送。" },
  { q: "共享剪贴板怎么用？", a: "默认关闭；开启后才与打开本机地址的页面/设备共享文本，聊天框粘贴才会回传。关闭时粘贴只填充输入框，不会把未发送内容写进共享槽。电脑 App 与手机 App 独立，跨 App 请用发送面板「剪贴板」投递。" },
  { q: "访问令牌可以关闭吗？", a: "可以。手机设置页可随时关闭/重新开启访问令牌，关闭前会二次确认风险；关闭后同 Wi-Fi 设备无需配对码即可读写，公共网络不建议关闭。" },
  { q: "更新检查会提示未签名版本吗？", a: "未签名/未公证的 Release 会公开发布但标记为 prerelease，应用内更新检查会跳过，避免把 Gatekeeper/SmartScreen 拦截的包当正式更新推送。" },
  { q: "访问令牌和分享链接安全吗？", a: "令牌是 6 位 CSPRNG 配对码，失败按来源 IP 退避；分享链接一次性、10 分钟有效，文件删除或重传后立即失效。缩略图/预览使用短时媒体令牌。" },
  { q: "一条消息最多多长？", a: "2 万字符（按 Unicode 字符计）。聊天保留最近 200 条，超出时从最旧消息开始淘汰。" },
];
const introSec = ref("feat");
const faqOpen = ref(-1);
let introSpy = null;
function toggleFaq(i) { faqOpen.value = faqOpen.value === i ? -1 : i; }
function introScrollTo(sec) {
  const root = document.getElementById("introOverlay");
  const el = document.getElementById("sec-" + sec);
  if (root && el) root.scrollTo({ top: el.offsetTop - 56, behavior: "smooth" });
}
function introScrollTop() {
  const root = document.getElementById("introOverlay");
  if (root) root.scrollTo({ top: 0, behavior: "smooth" });
  introSec.value = "feat";
}
function startIntroSpy() {
  stopIntroSpy();
  const root = document.getElementById("introOverlay");
  if (!root || typeof IntersectionObserver === "undefined") return;
  introSpy = new IntersectionObserver((entries) => {
    for (const en of entries) if (en.isIntersecting) introSec.value = en.target.dataset.sec;
  }, { root, rootMargin: "-40% 0px -55% 0px" });
  root.querySelectorAll(".sec[data-sec]").forEach((el) => introSpy.observe(el));
}
function stopIntroSpy() { if (introSpy) { introSpy.disconnect(); introSpy = null; } }
onMounted(() => {
  document.body.style.overflow = "hidden";
  nextTick(startIntroSpy);
});
onUnmounted(() => {
  document.body.style.overflow = "";
  stopIntroSpy();
});
</script>

<template>
<div id="introOverlay" class="intro-overlay" role="dialog" aria-label="CC Flash 软件介绍">
    <!-- 粘性顶栏 -->
    <header class="ihead">
      <div class="ihead-in">
        <span class="ihead-brand" title="回到顶部" @click="introScrollTop">
          <img class="ib-lg" src="/logo.png" alt="logo" /> CC Flash
        </span>
        <nav class="inav">
          <a href="#sec-feat" :class="{ active: introSec === 'feat' }" @click.prevent="introScrollTo('feat')">功能简介</a>
          <a href="#sec-howto" :class="{ active: introSec === 'howto' }" @click.prevent="introScrollTo('howto')">如何使用</a>
          <a href="#sec-safe" :class="{ active: introSec === 'safe' }" @click.prevent="introScrollTo('safe')">技术实现</a>
          <a href="#sec-faq" :class="{ active: introSec === 'faq' }" @click.prevent="introScrollTo('faq')">常见问题</a>
        </nav>
        <div class="ihead-act">
          <button class="btn btn-primary" @click="emit('service')">
            {{ running ? "打开服务中心" : "去启动服务" }}
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><line x1="7" y1="17" x2="17" y2="7"/><polyline points="7 7 17 7 17 17"/></svg>
          </button>
          <button class="icon-x" title="关闭 (Esc)" aria-label="关闭" @click="emit('close')">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>
          </button>
        </div>
      </div>
    </header>

    <!-- Hero -->
    <section class="hero">
      <div>
        <h1>文件传过去，<br /><span class="hl">同一个 Wi-Fi</span> 就够了。</h1>
        <p class="lead">
          电脑启动服务，手机、平板、同事的电脑扫码即用。
          数据只在局域网流动，不上云、无账号，断外网也能传。
        </p>
        <div class="hero-cta">
          <button class="btn btn-primary" @click="emit('transfer')">
            立即开始传输
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><line x1="5" y1="12" x2="19" y2="12"/><polyline points="12 5 19 12 12 19"/></svg>
          </button>
          <button class="btn btn-ghost" title="查看使用说明" @click="introScrollTo('howto')">
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" style="opacity:.85"><path d="M4 19.5A2.5 2.5 0 016.5 17H20"/><path d="M6.5 2H20v20H6.5A2.5 2.5 0 014 19.5v-15A2.5 2.5 0 016.5 2z"/></svg>
            查看使用说明
          </button>
        </div>
        <div class="hero-feats">
          <span class="feat-chip"><span class="tick">✓</span>手机免装 App</span>
          <span class="feat-chip"><span class="tick">✓</span>拖拽 / 粘贴上传</span>
          <span class="feat-chip"><span class="tick">✓</span>多台设备同时发送</span>
          <span class="feat-chip"><span class="tick">✓</span>文件消息刷新不丢</span>
          <span class="feat-chip"><span class="tick">✓</span>可选访问令牌</span>
        </div>
      </div>
      <!-- 应用窗口示意(二维码为真实地址, 服务未启动时占位) -->
      <div class="hero-visual" aria-hidden="true">
        <div class="hv-bar">
          <i></i><i></i><i></i>
          <span class="hv-url">{{ running ? shareUrl : "http://192.168.1.5:1688" }}</span>
          <span class="hv-run"><i></i>{{ running ? "运行中" : "示例" }}</span>
        </div>
        <div class="hv-body">
          <div class="hv-qr">
            <img v-if="qrData" class="hv-qr-img" :src="qrData" alt="二维码，扫码访问本机" />
            <div v-else class="hv-qr-ph"><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="7" y="2.5" width="10" height="19" rx="2.2"/><path d="M10.5 5h3"/><circle cx="12" cy="18.2" r=".8" fill="currentColor" stroke="none"/></svg>扫码打开</div>
          </div>
          <div class="hv-files">
            <div class="hv-file">
              <span class="fi" style="background:linear-gradient(135deg,#c73d3d,#a82d2d)">PDF</span>
              <div style="min-width:0"><div class="fn">需求评审纪要.pdf</div><div class="fm">2.4 MB · 电脑 → 手机</div></div>
              <span class="fp">100%</span>
            </div>
            <div class="hv-file">
              <span class="fi" style="background:linear-gradient(135deg,#3468c9,#244f9e)">JPG</span>
              <div style="min-width:0"><div class="fn">现场照片_01.jpg</div><div class="fm">5.1 MB · 手机 → 电脑</div></div>
              <span class="fp">100%</span>
            </div>
            <div class="hv-file">
              <span class="fi" style="background:linear-gradient(135deg,#1e8449,#196f3d)">ZIP</span>
              <div style="min-width:0"><div class="fn">release-build.zip</div><div class="fm">128 MB · 传输中</div></div>
              <span class="fp" style="color:var(--accent-text)">76%</span>
            </div>
          </div>
        </div>
        <div class="hv-cap">手机扫码打开即用 · 与电脑端实时互通</div>
      </div>
    </section>

    <!-- 功能 -->
    <section class="sec" id="sec-feat" data-sec="feat">
      <h2>同一 Wi-Fi，一个入口。</h2>
      <p class="lead">文件、消息、剪贴板，一个页面完成；手机浏览器打开就能用。</p>
      <div class="cards">
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 01-2 2H7l-4 4V5a2 2 0 012-2h14a2 2 0 012 2z"/></svg></div>
          <h3>文件与消息</h3>
          <p>文件和文字在同一条对话里：拖入即上传、自动生成文件卡片，支持 Markdown 与图片/PDF/音视频预览。文件消息会持久化，刷新浏览器或重启 App 后仍保留；共享文件统一收进文件页，排序、分页、批量删除都在。</p>
          <div class="tags"><span class="tag">文件即消息</span><span class="tag">持久化历史</span><span class="tag">Markdown</span><span class="tag">应用内预览</span><span class="tag">共享文件</span></div>
        </div>
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg></div>
          <h3>服务中心</h3>
          <p>启停服务、端口自动顺延、共享目录与磁盘余量集中管理。桌面常驻/悬浮二维码随时扫码，实时日志与六套配色一键切换。</p>
          <div class="tags"><span class="tag">服务控制</span><span class="tag">悬浮扫码</span><span class="tag">多主题配色</span></div>
        </div>
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 00.33 1.82l.06.06a2 2 0 01-2.83 2.83l-.06-.06a1.65 1.65 0 00-1.82-.33 1.65 1.65 0 00-1 1.51V21a2 2 0 01-4 0v-.09A1.65 1.65 0 009 19.4a1.65 1.65 0 00-1.82.33l-.06.06a2 2 0 01-2.83-2.83l.06-.06a1.65 1.65 0 00.33-1.82 1.65 1.65 0 00-1.51-1H3a2 2 0 010-4h.09A1.65 1.65 0 004.6 9a1.65 1.65 0 00-.33-1.82l-.06-.06a2 2 0 012.83-2.83l.06.06a1.65 1.65 0 001.82.33H9a1.65 1.65 0 001-1.51V3a2 2 0 014 0v.09a1.65 1.65 0 001 1.51 1.65 1.65 0 001.82-.33l.06-.06a2 2 0 012.83 2.83l-.06.06a1.65 1.65 0 00-.33 1.82V9a1.65 1.65 0 001.51 1H21a2 2 0 010 4h-.09a1.65 1.65 0 00-1.51 1z"/></svg></div>
          <h3>CC设置</h3>
          <p>开机自启、托盘常驻、一键检查更新；访问令牌可随时开启/关闭，手机设置页用 dir-5 信号台排版，令牌大号展示、点按即复制。关窗不关服务，手机端始终可用。</p>
          <div class="tags"><span class="tag">开机自启</span><span class="tag">令牌开关</span><span class="tag">dir-5 设置</span><span class="tag">检查更新</span></div>
        </div>
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="4" width="20" height="12" rx="2"/><line x1="8" y1="20" x2="16" y2="20"/><line x1="12" y1="16" x2="12" y2="20"/></svg></div>
          <h3>消息工作台</h3>
          <p>桌面左侧是周边设备栏，手机 App 四页切换。点设备只选中目标，不再自动弹面板；支持「多选」同时发给多台设备，文本与拖入文件依次投递。状态实时同步，窄屏自动收起，浏览器保持简洁聊天室。</p>
          <div class="tags"><span class="tag">设备即目标</span><span class="tag">多选直发</span><span class="tag">直发回执</span><span class="tag">浏览器零打扰</span></div>
        </div>
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="7" y="2.5" width="10" height="19" rx="2.2"/><path d="M10.5 5h3"/><circle cx="12" cy="18.2" r=".8" fill="currentColor" stroke="none"/></svg></div>
          <h3>手机 App · 信号台</h3>
          <p>发送 / 设备 / 文件三页按 dir-2 信号台重排：等宽读数、刻度雷达盘、RSSI 设备行、manifest 序号与进度条。Wi-Fi/CPU 锁 + 忽略电池优化，熄屏后仍可被找到。</p>
          <div class="tags"><span class="tag">dir-2 三页</span><span class="tag">多选发送</span><span class="tag">熄屏保活</span><span class="tag">本机节点</span></div>
        </div>
        <div class="card">
          <div class="cico"><svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="8" y="2" width="8" height="4" rx="1"/><path d="M16 4h2a2 2 0 012 2v14a2 2 0 01-2 2H6a2 2 0 01-2-2V6a2 2 0 012-2h2"/><path d="M9 12h6"/><path d="M9 16h4"/></svg></div>
          <h3>共享剪贴板</h3>
          <p><b>默认关闭</b>；开启后才与打开本机地址的浏览器/设备共享文本，聊天框粘贴才会回传。关闭时粘贴只填充输入框，不会把未发送内容写进共享槽。电脑 App 与手机 App 独立，跨 App 请用发送面板「剪贴板」投递。</p>
          <div class="tags"><span class="tag">默认关闭</span><span class="tag">开启后才回传</span><span class="tag">跨 App 用发送面板</span><span class="tag">浏览器兜底</span></div>
        </div>
      </div>
    </section>

    <!-- 如何使用 -->
    <section class="sec" id="sec-howto" data-sec="howto">
      <h2>三步，把内容交给对方。</h2>
      <p class="lead">无需配置网络、无需注册账号，装好即用。</p>
      <div class="steps">
        <div class="step">
          <div class="no">01</div>
          <h3>电脑启动服务</h3>
          <p>打开 CC Flash，点「启动服务」。默认端口 <code>1688</code>，被占用自动顺延。</p>
        </div>
        <div class="step">
          <div class="no">02</div>
          <h3>扫码或输地址</h3>
          <p>手机连同一 Wi-Fi，扫码或输入 <code>http://电脑IP:端口</code>，立即进入共享页。</p>
        </div>
        <div class="step">
          <div class="no">03</div>
          <h3>直接开始互传</h3>
          <p>拖文件或发消息；在设备栏/设备页选中目标即可直发，支持多选同时发多台。文件会同时进入对方共享目录与聊天并持久化，电脑关窗/手机熄屏服务也不停。</p>
        </div>
      </div>
    </section>

    <!-- 技术实现 / 可靠性 -->
    <section class="sec" id="sec-safe" data-sec="safe">
      <h2>传得快，也要传得安心。</h2>
      <p class="lead">传输边界清清楚楚：数据只在你的路由器内部流动，电脑在哪里，数据就到哪里。</p>
      <div class="points">
        <div class="pt">
          <div class="pico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/></svg></div>
          <h3>数据只在局域网</h3>
          <p>服务只在局域网直达，不上云、不注册，断外网可用。移动端自动生成本机令牌，桌面可开启 6 位访问令牌；分享链接一次性有效，预览使用短时媒体令牌，长期 PIN 不进入 URL。</p>
        </div>
        <div class="pt">
          <div class="pico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg></div>
          <h3>大文件不占内存</h3>
          <p>上传下载全程流式处理，大文件不占内存；失败/超时自动回滚半截文件。</p>
        </div>
        <div class="pt">
          <div class="pico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="7" width="20" height="14" rx="2"/><path d="M16 21V5a2 2 0 00-2-2h-4a2 2 0 00-2 2v16"/></svg></div>
          <h3>服务常驻不断</h3>
          <p>关窗进托盘，服务继续运行；Android 使用 connectedDevice 前台服务 + Wi-Fi/CPU 锁，熄屏后仍可被发现；开机自启可手动开启。</p>
        </div>
        <div class="pt">
          <div class="pico"><svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="5" y="2" width="14" height="20" rx="2"/><line x1="12" y1="18" x2="12.01" y2="18"/></svg></div>
          <h3>多端覆盖</h3>
          <p>macOS、Windows、Android App，外加电脑与手机浏览器。Windows 安装包使用 WebView2 引导器（Win10/11 通常已自带）；iOS 没有原生包，用 Safari 打开电脑地址即可。</p>
        </div>
      </div>
      <div class="codes">
        <div class="code-card">
          <h3>
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="var(--accent-text)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg>
            桌面端安装
          </h3>
          <p>前往 GitHub Release 下载对应平台安装包，安装即用 —— 前端已嵌入二进制，无需额外部署。未签名版本会标记为 prerelease，应用内更新检查会跳过，建议签名版发布后再升级。</p>
          <div class="code-box"><button class="code-copy" @click="copyText(RELEASES_PAGE, '已复制')">复制</button><span class="cm"># 打开最新版发布页</span><br />{{ RELEASES_PAGE }}</div>
        </div>
        <div class="code-card">
          <h3>
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="var(--accent-text)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="16 18 22 12 16 6"/><polyline points="8 6 2 12 8 18"/></svg>
            从源码构建
          </h3>
          <p>Rust + Vue 3 + Tauri 技术栈，两条命令构建当前平台的安装包。</p>
          <div class="code-box"><button class="code-copy" @click="copyText(`npm install\nnpm run tauri build`, '已复制')">复制</button>npm install<br />npm run tauri build</div>
        </div>
      </div>
      <p class="code-note">升级前请保留原共享目录（默认 <code>~/CC-Flash</code>）。问题反馈与建议欢迎提交 <a :href="'https://github.com/' + GITHUB_REPO + '/issues'" @click.prevent="openExternal('https://github.com/' + GITHUB_REPO + '/issues')">GitHub Issue</a>。</p>
    </section>

    <!-- 常见问题 -->
    <section class="sec" id="sec-faq" data-sec="faq">
      <h2>常见问题，一次说清。</h2>
      <div class="faq">
        <div v-for="(f, i) in INTRO_FAQS" :key="i" class="faq-item" :class="{ open: faqOpen === i }">
          <button class="faq-q" @click="toggleFaq(i)">{{ f.q }}<span class="pm">+</span></button>
          <div class="faq-a" :style="{ maxHeight: faqOpen === i ? '900px' : '0' }"><p>{{ f.a }}</p></div>
        </div>
      </div>
    </section>

    <!-- 页脚 -->
    <footer class="ifoot">
      <div class="ifoot-in">
        <div class="ifoot-brand">
          <img class="ib-lg" src="/logo.png" alt="logo" /> CC Flash
        </div>
        <div class="ifoot-links">
          <a :href="'https://github.com/' + GITHUB_REPO" @click.prevent="openExternal('https://github.com/' + GITHUB_REPO)">GitHub</a>
          <a :href="'https://github.com/' + GITHUB_REPO + '/issues'" @click.prevent="openExternal('https://github.com/' + GITHUB_REPO + '/issues')">反馈问题</a>
          <a :href="RELEASES_PAGE" @click.prevent="openExternal(RELEASES_PAGE)">下载安装</a>
        </div>
        <div class="ifoot-copy">CC Flash（曹操是快😂）—— 局域网文件互传工具 · {{ FOOTER_TEXT }}</div>
      </div>
    </footer>
    </div>
</template>
