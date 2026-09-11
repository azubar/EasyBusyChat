// Regression harness: executes the actual inline application script with a minimal
// DOM/Tauri adapter and a deterministic clock. No remote scripts or API calls.
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const html = fs.readFileSync(path.join(__dirname, '../component/ui/frontend/index.html'), 'utf8');
const source = [...html.matchAll(/<script\b[^>]*>([\s\S]*?)<\/script>/g)].map(m => m[1]).join('\n');

async function setup(options = {}) {
  let now = 0, nextTimer = 0, hash = '#provider=telegram&accountid=account-1&chatid=77';
  const timers = new Map(), listeners = {}, nodes = {}, events = [], renders = [];
  const message = { textContent: '', style: {} };
  const config = { companyId: 'audit-company', accessToken: 'synthetic-token', generation: 1 };
  const location = { href: 'https://tauri.localhost/' + hash };
  Object.defineProperty(location, 'hash', {
    get: () => hash,
    set(value) {
      const next = value.startsWith('#') ? value : '#' + value;
      if (next !== hash) {
        hash = next;
        queueMicrotask(() => (listeners.hashchange || []).forEach(fn => fn()));
      }
    },
  });
  const schedule = (fn, delay, repeat) => {
    const id = ++nextTimer;
    timers.set(id, { fn, at: now + delay, delay, repeat });
    return id;
  };
  const window = {
    location,
    addEventListener(type, fn) { (listeners[type] ||= []).push(fn); },
    localStorage: { setItem() {} },
    __TAURI__: { core: { async invoke(command, args) {
      if (command === 'get_config') return options.getConfig ? options.getConfig(config) : config;
      if (command === 'on_webview_message') events.push(JSON.parse(args.payload));
    } } },
    renderChatsApp(id, options) { renders.push(options); },
    unmountChatsApp() {},
  };
  const document = {
    readyState: 'complete',
    querySelector() { return message; },
    getElementById(id) { return nodes[id]; },
    createElement() { return {}; },
    head: { appendChild(node) { nodes[node.id] = node; queueMicrotask(() => node.onload?.()); } },
    addEventListener() {},
  };
  const context = vm.createContext({ window, document, URLSearchParams, Map, Promise,
    console: { log() {}, warn() {}, error() {} },
    setInterval: (fn, delay) => schedule(fn, delay, true),
    clearInterval: id => timers.delete(id),
    setTimeout: (fn, delay) => schedule(fn, delay, false),
    clearTimeout: id => timers.delete(id),
  });
  vm.runInContext(source, context, { filename: 'ui/frontend/index.html' });
  if (options.startupSignal) window.onConfigReady().catch(() => {});
  async function flush() { for (let i = 0; i < 8; i++) await Promise.resolve(); }
  async function advance(ms) {
    const end = now + ms;
    for (;;) {
      const entry = [...timers].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
      if (!entry) break;
      const [id, timer] = entry;
      now = timer.at;
      if (timer.repeat) timer.at += timer.delay; else timers.delete(id);
      timer.fn();
      await flush();
    }
    now = end;
    await flush();
  }
  await flush();
  await advance(100);
  if (!options.startupSignal) assert.equal(renders.length, 1, 'harness must render initial app');
  return { window, events, renders, advance, flush, timers };
}

const cases = [
  ['config-ready concurrent with an empty startup read must initialize', async () => {
    let reads = 0;
    const h = await setup({ startupSignal: true, getConfig: config => ++reads === 1 ? null : config });
    await h.advance(200);
    assert.equal(h.renders.length, 1);
    assert.equal(reads, 2);
  }],
  ['zero account and chat IDs survive callback normalization', async () => {
    const h = await setup();
    await h.renders[0].callbacks.onOpenChat({ provider: 'telegram', accountId: 0, chat: { id: 0 } });
    const event = h.events.find(e => e.event === 'chatOpened');
    assert.equal(event.payload.accountId, '0');
    assert.equal(event.payload.chatId, '0');
  }],
  ['navigation rejects outstanding BAS actions and releases timers', async () => {
    const h = await setup();
    const actions = await h.renders[0].callbacks.onOpenChat({ chatId: '77' });
    let rejected = false;
    actions[0].callback({}).catch(() => { rejected = true; });
    const navigation = h.window.navigateChat('#provider=telegram&accountid=account-1&chatid=88');
    await h.advance(200);
    await navigation;
    assert.equal(rejected, true);
    assert.equal(h.timers.size, 0);
  }],
  ['failed IPC invocation rejects action without waiting for timeout', async () => {
    const h = await setup();
    const actions = await h.renders[0].callbacks.onOpenChat({ chatId: '77' });
    h.window.__TAURI__.core.invoke = () => Promise.reject(new Error('transport unavailable'));
    await assert.rejects(actions[0].callback({}), /Не вдалося передати дію BAS/);
    assert.equal(h.timers.size, 0);
  }],
  ['BAS pending action capacity is bounded', async () => {
    const h = await setup();
    const actions = await h.renders[0].callbacks.onOpenChat({ chatId: '77' });
    for (let i = 0; i < 128; i++) actions[0].callback({}).catch(() => {});
    await assert.rejects(actions[0].callback({}));
    assert.equal(h.events.filter(e => e.event === 'chat_action').length, 128);
    await h.advance(31000);
    assert.equal(h.timers.size, 0);
  }],
  ['reopening the same hash must cause fresh selection confirmation', async () => {
    const h = await setup();
    await h.renders[0].callbacks.onOpenChat({ provider: 'telegram', accountId: 'account-1', chatId: '77' });
    h.events.length = 0;
    h.window.navigateChat(h.window.location.hash);
    await h.advance(31000);
    assert.ok(h.renders.length > 1 || h.events.some(e => e.event === 'chatOpened'),
      'same hash causes neither render nor chatOpened; DLL wait expires');
  }],
  ['numeric chat IDs must be serialized as strings for DLL selection', async () => {
    const h = await setup();
    await h.renders[0].callbacks.onOpenChat({ provider: 'telegram', accountId: 'account-1', chat: { id: 77 } });
    const event = h.events.find(e => e.event === 'chatOpened');
    assert.equal(typeof event.payload.chatId, 'string', 'DLL reads chatId through Value::as_str');
  }],
  ['unanswered BAS action must settle after a bounded wait', async () => {
    const h = await setup();
    const actions = await h.renders[0].callbacks.onOpenChat({ chatId: '77' });
    let settled = false;
    actions[0].callback({}).then(() => { settled = true; }, () => { settled = true; });
    await h.advance(60000);
    assert.equal(settled, true, 'no timeout is registered; action stays pending');
  }],
  ['successful BAS reply settles the action (control)', async () => {
    const h = await setup();
    const actions = await h.renders[0].callbacks.onOpenChat({ chatId: '77' });
    const pending = actions[0].callback({});
    const request = h.events.find(e => e.event === 'chat_action');
    h.window.easybusychat.__dispatchMessage({ event: 'action_result', reqId: request.reqId, success: true, payload: { ok: true } });
    assert.equal((await pending).ok, true);
  }],
];
(async () => {
  let failures = 0;
  for (const [name, test] of cases) {
    try { await test(); console.log('PASS ' + name); }
    catch (error) { failures++; console.log('FAIL ' + name + ': ' + error.message); }
  }
  console.log(`${cases.length - failures} passed; ${failures} failed`);
  process.exitCode = failures ? 1 : 0;
})();
