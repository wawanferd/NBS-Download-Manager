'use strict';

// Works as a Chrome MV3 service worker and as a Firefox background script.
const api = globalThis.browser ?? globalThis.chrome;

const NATIVE_HOST = 'com.nbs.downloadmanager';
const VERSION = api.runtime.getManifest().version;
const DEFAULTS = { enabled: true, port: 18765, minSizeKB: 0, useNative: true };

async function getConfig() {
  return { ...DEFAULTS, ...(await api.storage.local.get(DEFAULTS)) };
}

/**
 * Connection to the desktop app. Prefers the local WebSocket server and
 * falls back to Native Messaging (the host relays to the same server and
 * starts the app when needed).
 */
class Bridge {
  constructor() {
    this.ws = null;
    this.native = null;
    this.mode = 'none';
    this.pending = new Map();
    this.connecting = null;
    this.keepalive = null;
    this.retryTimer = null;
    this.retryDelay = 3000;
    this.lastError = null;
  }

  get connected() {
    return (this.ws && this.ws.readyState === WebSocket.OPEN) || this.native !== null;
  }

  /**
   * Resolves to true once a transport is ready. Only with `launch` may the
   * native host be used, since it starts the app when it is not running.
   */
  async ensure(timeoutMs = 1500, launch = false) {
    if (this.connected) return true;
    if (this.connecting) {
      const ok = await this.connecting;
      if (ok || !launch) return ok;
    }
    if (!this.connecting) {
      this.connecting = this.connect(timeoutMs, launch).finally(() => {
        this.connecting = null;
      });
    }
    return this.connecting;
  }

  async connect(timeoutMs, launch) {
    const cfg = await getConfig();
    if (await this.connectWebSocket(cfg.port, timeoutMs)) return true;
    if (launch && cfg.useNative && (await this.connectNative(10000))) return true;
    this.scheduleRetry();
    return false;
  }

  connectWebSocket(port, timeoutMs) {
    return new Promise((resolve) => {
      let ws;
      try {
        ws = new WebSocket(`ws://127.0.0.1:${port}/extension`);
      } catch (e) {
        this.lastError = String(e);
        resolve(false);
        return;
      }
      const timer = setTimeout(() => {
        ws.close();
        resolve(false);
      }, timeoutMs);
      ws.onopen = () => {
        clearTimeout(timer);
        this.ws = ws;
        this.setMode('websocket');
        this.post({ type: 'hello', client: navigator.userAgent, version: VERSION });
        resolve(true);
      };
      ws.onerror = () => {
        clearTimeout(timer);
        this.lastError = `Cannot reach the app on port ${port}`;
        resolve(false);
      };
      ws.onclose = () => {
        if (this.ws !== ws) return;
        this.ws = null;
        this.disconnected();
      };
      ws.onmessage = (e) => this.onMessage(e.data);
    });
  }

  connectNative(timeoutMs) {
    return new Promise((resolve) => {
      let port;
      try {
        port = api.runtime.connectNative(NATIVE_HOST);
      } catch (e) {
        this.lastError = String(e);
        resolve(false);
        return;
      }
      let settled = false;
      const settle = (ok) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        resolve(ok);
      };
      const timer = setTimeout(() => {
        port.disconnect();
        settle(false);
      }, timeoutMs);
      port.onMessage.addListener((msg) => {
        if (msg?.type === 'welcome' && !settled) {
          this.native = port;
          this.setMode('native');
          settle(true);
        } else if (msg?.type === 'error' && !settled) {
          this.lastError = msg.message;
          port.disconnect();
          settle(false);
        }
        this.onMessage(msg);
      });
      port.onDisconnect.addListener(() => {
        const err = api.runtime.lastError?.message;
        if (err) this.lastError = err;
        settle(false);
        if (this.native === port) {
          this.native = null;
          this.disconnected();
        }
      });
    });
  }

  setMode(mode) {
    this.mode = mode;
    this.lastError = null;
    this.retryDelay = 3000;
    clearTimeout(this.retryTimer);
    clearInterval(this.keepalive);
    // Regular traffic also keeps the MV3 service worker alive while connected.
    this.keepalive = setInterval(() => this.post({ type: 'ping' }), 20000);
    updateBadge();
  }

  disconnected() {
    this.mode = 'none';
    clearInterval(this.keepalive);
    for (const { resolve, timer } of this.pending.values()) {
      clearTimeout(timer);
      resolve({ accepted: false, error: 'connection lost' });
    }
    this.pending.clear();
    updateBadge();
    this.scheduleRetry();
  }

  scheduleRetry() {
    clearTimeout(this.retryTimer);
    this.retryTimer = setTimeout(async () => {
      const cfg = await getConfig();
      if (!cfg.enabled || this.connected) return;
      if (!(await this.connectWebSocket(cfg.port, 1500))) {
        this.retryDelay = Math.min(this.retryDelay * 2, 60000);
        this.scheduleRetry();
      }
    }, this.retryDelay);
  }

  close() {
    clearTimeout(this.retryTimer);
    this.ws?.close();
    this.native?.disconnect();
    this.ws = null;
    this.native = null;
    this.mode = 'none';
    updateBadge();
  }

  post(msg) {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(msg));
    else if (this.native) this.native.postMessage(msg);
    else return false;
    return true;
  }

  onMessage(raw) {
    let msg = raw;
    if (typeof raw === 'string') {
      try {
        msg = JSON.parse(raw);
      } catch {
        return;
      }
    }
    if (msg?.type === 'ack' && this.pending.has(msg.requestId)) {
      const { resolve, timer } = this.pending.get(msg.requestId);
      clearTimeout(timer);
      this.pending.delete(msg.requestId);
      resolve(msg);
    }
  }

  /** Sends a request and waits for the app's acknowledgement. */
  request(msg, timeoutMs = 5000) {
    const requestId = crypto.randomUUID();
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        this.pending.delete(requestId);
        resolve({ accepted: false, error: 'timed out' });
      }, timeoutMs);
      this.pending.set(requestId, { resolve, timer });
      if (!this.post({ ...msg, requestId })) {
        clearTimeout(timer);
        this.pending.delete(requestId);
        resolve({ accepted: false, error: 'not connected' });
      }
    });
  }
}

const bridge = new Bridge();

async function updateBadge() {
  const cfg = await getConfig();
  const text = !cfg.enabled ? 'OFF' : bridge.connected ? '' : '!';
  const color = !cfg.enabled ? '#7a82a0' : '#e5484d';
  await api.action.setBadgeText({ text });
  await api.action.setBadgeBackgroundColor({ color });
  await api.action.setTitle({
    title: !cfg.enabled ? 'NBS Download Manager — capture disabled' : bridge.connected ? `NBS Download Manager — connected (${bridge.mode})` : 'NBS Download Manager — app not running',
  });
}

/* ---------- Capturing downloads ---------- */

// URLs we hand back to the browser must not be captured again.
const bypass = new Set();

function baseName(path) {
  return (path || '').split(/[\\/]/).pop() || null;
}

async function cookieHeader(url) {
  try {
    const cookies = await api.cookies.getAll({ url });
    return cookies.map((c) => `${c.name}=${c.value}`).join('; ') || null;
  } catch {
    return null;
  }
}

async function sendToApp({ url, filename = null, referrer = null, fileSize = null, mime = null }) {
  const message = {
    type: 'download',
    url,
    filename,
    referrer,
    fileSize,
    mime,
    cookies: await cookieHeader(url),
    userAgent: navigator.userAgent,
  };
  const reply = await bridge.request(message);
  return reply.accepted === true;
}

function downloadInBrowser(url) {
  bypass.add(url);
  return api.downloads.download({ url }).catch(() => bypass.delete(url));
}

async function onDownloadCreated(item) {
  const url = item.finalUrl || item.url;
  if (!/^https?:/i.test(url)) return; // blob:, data:, file: stay in the browser
  if (item.byExtensionId && item.byExtensionId === api.runtime.id) return;
  if (bypass.delete(url) || bypass.delete(item.url)) return;
  if (item.state && item.state !== 'in_progress') return;

  const cfg = await getConfig();
  if (!cfg.enabled) return;
  const size = item.totalBytes > 0 ? item.totalBytes : item.fileSize > 0 ? item.fileSize : null;
  if (cfg.minSizeKB > 0 && size && size < cfg.minSizeKB * 1024) return;
  if (!(await bridge.ensure(1500, true))) return; // app unavailable: let the browser handle it

  try {
    await api.downloads.cancel(item.id);
    await api.downloads.erase({ id: item.id });
  } catch {
    // The download may already have finished or been removed.
  }
  const accepted = await sendToApp({ url, filename: baseName(item.filename), referrer: item.referrer || null, fileSize: size, mime: item.mime || null });
  if (!accepted) await downloadInBrowser(url);
}

api.downloads.onCreated.addListener((item) => {
  onDownloadCreated(item).catch((e) => console.error('NBS: capture failed', e));
});

/* ---------- Context menu ---------- */

function createMenus() {
  Promise.resolve(api.contextMenus.removeAll()).then(() => {
    api.contextMenus.create({ id: 'nbs-link', title: 'Download with NBS Download Manager', contexts: ['link'] });
    api.contextMenus.create({ id: 'nbs-media', title: 'Download media with NBS Download Manager', contexts: ['image', 'video', 'audio'] });
  });
}

api.runtime.onInstalled.addListener(createMenus);
api.runtime.onStartup?.addListener(() => {
  bridge.ensure();
});

api.contextMenus.onClicked.addListener(async (info, tab) => {
  const url = info.linkUrl || info.srcUrl;
  if (!url) return;
  // Magnet links go to the app's torrent engine; the browser cannot fetch them.
  const magnet = /^magnet:/i.test(url);
  if (!magnet && !/^https?:/i.test(url)) return;
  const referrer = info.pageUrl || tab?.url || null;
  if ((await bridge.ensure(2000, true)) && (await sendToApp({ url, referrer }))) return;
  if (!magnet) await downloadInBrowser(url);
});

/* ---------- Popup messages ---------- */

async function handleMessage(msg) {
  switch (msg?.cmd) {
    case 'status': {
      const cfg = await getConfig();
      if (cfg.enabled) await bridge.ensure(1200);
      return { ...cfg, connected: bridge.connected, mode: bridge.mode, error: bridge.lastError, version: VERSION };
    }
    case 'setConfig': {
      const before = await getConfig();
      await api.storage.local.set(msg.config);
      const after = await getConfig();
      if (before.port !== after.port || before.useNative !== after.useNative) bridge.close();
      if (after.enabled) await bridge.ensure(1500);
      await updateBadge();
      return { ok: true };
    }
    case 'reconnect':
      bridge.close();
      return { connected: await bridge.ensure(2000, true), error: bridge.lastError };
    case 'send': {
      if (!(await bridge.ensure(2000, true))) return { ok: false, error: bridge.lastError || 'App is not running' };
      const ok = await sendToApp({ url: msg.url });
      return { ok, error: ok ? null : 'The app rejected the link' };
    }
    default:
      return { error: 'unknown command' };
  }
}

api.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  handleMessage(msg).then(sendResponse, (e) => sendResponse({ error: String(e) }));
  return true;
});

updateBadge();
bridge.ensure();
