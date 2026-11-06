// Run against a Vite dev server. Tauri IPC and phone HTTP/WS events are simulated;
// no personal files, messages or live desktop app state are touched.
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

async function main() {
  const browser = await chromium.launch({ headless: true, channel: 'chrome' });
  const context = await browser.newContext({ viewport: { width: 96, height: 96 }, deviceScaleFactor: 3 });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.addInitScript(() => {
    let callbackId = 0;
    const callbacks = new Map();
    const listeners = new Map();
    const monitor = { name: 'Test', scaleFactor: 1, position: { x: 0, y: 0 }, size: { width: 1280, height: 900 }, workArea: { position: { x: 0, y: 0 }, size: { width: 1280, height: 900 } } };
    window.petTest = {
      main: { visible: false, minimized: false, focused: false },
      messages: [], files: [], failLists: false, listRequests: 0,
      size: { width: 96, height: 96 }, pos: { x: -48, y: 402 }, cursor: { x: 1000, y: 800 },
      emit(name, payload) {
        for (const id of listeners.get(name) || []) callbacks.get(id)?.({ event: name, payload, id });
      },
      state(next, emit = true) {
        Object.assign(this.main, next);
        const active = this.main.visible && !this.main.minimized && this.main.focused;
        if (emit) this.emit('main-vis://changed', { ...this.main, active });
      },
      ws(event) { this.socket.onmessage?.({ data: JSON.stringify(event) }); },
    };
    const t = window.petTest;
    window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'qr-pet' }, currentWebview: { label: 'qr-pet' } },
      transformCallback(fn) { callbacks.set(++callbackId, fn); return callbackId; },
      async invoke(cmd, args = {}) {
        if (cmd === 'plugin:event|listen') {
          if (!listeners.has(args.event)) listeners.set(args.event, []);
          listeners.get(args.event).push(args.handler);
          return args.handler;
        }
        if (cmd === 'main_visible_cmd') return { ok: true, ...t.main, active: t.main.visible && !t.main.minimized && t.main.focused };
        if (cmd === 'show_main_window_cmd') { t.state({ visible: true, minimized: false, focused: true }); return { ok: true }; }
        if (cmd === 'control_status_cmd') return { ok: true, running: true, port: 1688, ip: '192.168.1.8', version: '1.1.8' };
        if (cmd === 'load_text_pref_cmd') return { ok: true, value: null };
        if (cmd === 'load_pet_dock_cmd') return { ok: true, dock: JSON.stringify({ edge: 'left', cross: 450 }) };
        if (cmd.endsWith('|current_monitor')) return monitor;
        if (cmd.endsWith('|available_monitors')) return [monitor];
        if (cmd.endsWith('|cursor_position')) return t.cursor;
        if (cmd.endsWith('|outer_position')) return t.pos;
        if (cmd.endsWith('|outer_size')) return t.size;
        if (cmd.endsWith('|set_position')) { t.pos = args.value.toJSON().Logical; return; }
        if (cmd.endsWith('|set_size')) { t.size = args.value.toJSON().Logical; return; }
        if (cmd.endsWith('|is_visible')) return true;
        if (cmd.endsWith('|is_enabled')) return false;
        return { ok: true };
      },
    };
    class FakeWebSocket {
      static OPEN = 1;
      constructor() {
        this.readyState = 0;
        t.socket = this;
        setTimeout(() => { this.readyState = 1; this.onopen?.(); }, 10);
      }
      close() { this.readyState = 3; this.onclose?.(); }
    }
    window.WebSocket = FakeWebSocket;
    const fetchOriginal = window.fetch.bind(window);
    window.fetch = async (url, opts) => {
      const u = String(url);
      if (!u.includes('/api/')) return fetchOriginal(url, opts);
      if (u.includes('/files') || u.includes('/chat')) t.listRequests++;
      if (t.failLists && (u.includes('/files') || u.includes('/chat'))) return new Response('{}', { status: 503 });
      const body = u.includes('/files') ? { files: t.files } : u.includes('/chat') ? { messages: t.messages } : {};
      return new Response(JSON.stringify(body), { headers: { 'Content-Type': 'application/json' } });
    };
  });
  try {
    await page.goto((process.env.PET_TEST_URL || 'http://127.0.0.1:5178') + '/?view=qr-pet');
    await page.waitForLoadState('networkidle');
    await page.waitForFunction(() => window.petTest?.listRequests >= 2 && window.petTest.socket?.readyState === 1);
    // 绿点已移入 SVG 环组: 以 .pet-ring-dot 的存在与否表达 on 态
    const badge = page.locator('.pet-ring-dot');
    const waitBadge = on => page.waitForFunction(on => !!document.querySelector('.pet-ring-dot') === on, on);
    assert.equal(await page.locator('.qr-pet-avatar svg path').count(), 4);
    assert.equal((await page.locator('.qr-pet-avatar svg text').textContent()).trim(), 'CC');
    assert.equal(await page.locator('.qr-pet-c-outer').count(), 1);
    assert.equal(await page.locator('.qr-pet-avatar').evaluate(el => getComputedStyle(el).backgroundColor), 'rgb(7, 193, 96)');
    await waitBadge(false);

    // Direct WS notifications work even when the corresponding HTTP read fails.
    await page.evaluate(() => { petTest.failLists = true; petTest.ws({ type: 'chat:new' }); });
    await waitBadge(true);
    assert.match(await page.locator('.qr-pet').getAttribute('aria-label'), /有新文件或消息/);
    await page.evaluate(() => petTest.state({ visible: true, focused: true }));
    await waitBadge(false);
    await page.evaluate(() => petTest.ws({ type: 'chat:new' }));
    await page.waitForTimeout(100);
    assert.equal(await badge.evaluate(el => el.classList.contains('on')), false);

    // A visible but unfocused main window must not suppress phone events.
    await page.evaluate(() => { petTest.state({ focused: false }); petTest.ws({ type: 'files:changed', action: 'uploaded' }); });
    await waitBadge(true);
    await page.evaluate(() => petTest.state({ focused: true }));
    await waitBadge(false);

    // Minimize without broadcasting: arrival-time IPC query recovers the state.
    await page.evaluate(() => { petTest.state({ minimized: true }, false); petTest.ws({ type: 'chat:new' }); });
    await waitBadge(true);
    await page.evaluate(() => petTest.state({ minimized: false, focused: true }));
    await waitBadge(false);
    // 纯删除不提醒: 先让 old.txt 真实入基线(会提醒一次, 顺带确认基线机制),
    // 再移除它并触发列表刷新 —— 收缩必须被观测到, 否则断言是空跑(假绿)
    await page.evaluate(() => { petTest.files = [{ name: 'old.txt', size: 1, modified: 1 }]; petTest.state({ visible: false, focused: false }); petTest.failLists = false; petTest.ws({ type: 'files:changed', action: 'uploaded' }); });
    await waitBadge(true);
    await page.evaluate(() => { petTest.files = []; petTest.ws({ type: 'files:changed', action: 'deleted' }); });
    await waitBadge(false);

    // Fallback snapshot detection for older servers/reconnection with no action.
    await page.evaluate(() => { petTest.files = [{ name: 'phone.txt', size: 5, modified: 1234 }]; petTest.ws({ type: 'files:changed' }); });
    await waitBadge(true);
    await page.evaluate(() => petTest.state({ visible: true, focused: true }));
    await waitBadge(false);
    await page.evaluate(() => { petTest.state({ focused: false }); petTest.ws({ type: 'files:changed', action: 'uploaded' }); });
    await waitBadge(true);
    await page.emulateMedia({ reducedMotion: 'reduce' });
    assert.equal(await page.locator('.pet-ring-breath').evaluate(el => getComputedStyle(el).animationName), 'none');
    const output = process.env.PET_TEST_OUTPUT || path.resolve('test-results/pet');
    fs.mkdirSync(output, { recursive: true });
    await page.screenshot({ path: path.join(output, 'cc-orb-unread.png'), omitBackground: true });

    // All four half-hidden dock positions leave the badge on the visible half.
    for (const edge of ['left', 'right', 'top', 'bottom']) {
      const box = await page.locator('.qr-pet').evaluate((el, edge) => {
        el.classList.remove('edge-left', 'edge-right', 'edge-top', 'edge-bottom');
        el.classList.add('edge-' + edge);
        const r = el.querySelector('.qr-pet-st').getBoundingClientRect();
        return { x: r.x, y: r.y, right: r.right, bottom: r.bottom };
      }, edge);
      assert.ok(edge === 'left' ? box.x > 48 : edge === 'right' ? box.right < 48 : edge === 'top' ? box.y > 48 : box.bottom < 48, edge);
    }
    await page.locator('.qr-pet').evaluate(el => { el.classList.remove('edge-bottom'); el.classList.add('edge-left'); });
    // Hover previews QR while preserving unread. Opening the main window clears it.
    await page.evaluate(() => { petTest.cursor = { x: petTest.pos.x + 60, y: petTest.pos.y + 48 }; });
    await page.waitForSelector('.qr-pet.expanded');
    await page.setViewportSize({ width: 136, height: 190 });
    assert.equal(await page.locator('.qr-pet-panel-dot').count(), 1);
    await page.screenshot({ path: path.join(output, 'cc-orb-expanded.png'), omitBackground: true });
    await page.locator('.qr-pet').click({ position: { x: 68, y: 20 } });
    await waitBadge(false);
    await page.waitForFunction(() => !document.querySelector('.qr-pet').classList.contains('expanded'));
    assert.equal(await page.evaluate(() => petTest.main.focused), true);
    await page.setViewportSize({ width: 96, height: 96 });
    await page.screenshot({ path: path.join(output, 'cc-orb-idle.png'), omitBackground: true });
    assert.deepEqual(errors, []);
    console.log('PASS: preserved viewfinder + WeChat green + CC rings; first message; foreground/background/minimized/hidden; upload vs deletion; HTTP failure; snapshot fallback; four dock edges; reduced motion; QR hover; click acknowledgement.');
  } finally {
    await browser.close();
  }
}
main().catch(error => { console.error(error); process.exitCode = 1; });
