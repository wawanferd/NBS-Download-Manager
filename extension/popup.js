'use strict';

const api = globalThis.browser ?? globalThis.chrome;
const $ = (id) => document.getElementById(id);
const ALL_URLS = { origins: ['<all_urls>'] };

function send(cmd, extra = {}) {
  return Promise.resolve(api.runtime.sendMessage({ cmd, ...extra }))
    .then((r) => r || {})
    .catch((e) => ({ error: String(e) }));
}

function notice(text, ok = false) {
  const el = $('notice');
  el.hidden = !text;
  el.textContent = text || '';
  el.className = `notice${ok ? ' ok' : ''}`;
}

function renderStatus(s) {
  const status = $('status');
  if (!s.enabled) {
    status.className = 'status off';
    $('status-text').textContent = 'Capture disabled';
  } else if (s.connected) {
    status.className = 'status ok';
    $('status-text').textContent = s.mode === 'native' ? 'Connected via native messaging' : 'Connected to the app';
  } else {
    status.className = 'status err';
    $('status-text').textContent = 'App not running';
  }
}

async function refresh() {
  const s = await send('status');
  $('enabled').checked = s.enabled;
  $('port').value = s.port;
  $('min-size').value = s.minSizeKB || '';
  $('use-native').checked = s.useNative;
  $('version').textContent = `v${s.version}`;
  renderStatus(s);
  if (s.enabled && !s.connected) notice('Start NBS Download Manager to capture downloads. Until then your browser downloads normally.');
  else notice('');
  if (api.permissions?.contains) {
    $('grant').hidden = await api.permissions.contains(ALL_URLS).catch(() => true);
  }
}

async function saveConfig(config) {
  await send('setConfig', { config });
  await refresh();
}

$('enabled').addEventListener('change', (e) => saveConfig({ enabled: e.target.checked }));
$('use-native').addEventListener('change', (e) => saveConfig({ useNative: e.target.checked }));
$('port').addEventListener('change', (e) => {
  const port = Number(e.target.value);
  if (port >= 1024 && port <= 65535) saveConfig({ port });
});
$('min-size').addEventListener('change', (e) => saveConfig({ minSizeKB: Math.max(0, Number(e.target.value) || 0) }));

$('reconnect').addEventListener('click', async () => {
  $('status-text').textContent = 'Connecting…';
  const r = await send('reconnect');
  await refresh();
  if (!r.connected && r.error) notice(r.error);
});

$('grant').addEventListener('click', async () => {
  // Must be called directly from the click for Firefox to show the prompt.
  const granted = await api.permissions.request(ALL_URLS).catch(() => false);
  $('grant').hidden = granted;
});

$('send-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const url = $('send-url').value.trim();
  if (!/^(https?:\/\/\S+|magnet:\?\S+)/i.test(url)) {
    notice('Enter a valid http(s) or magnet link.');
    return;
  }
  const button = e.submitter || $('send-form').querySelector('button');
  button.disabled = true;
  const r = await send('send', { url });
  button.disabled = false;
  if (r.ok) {
    $('send-url').value = '';
    notice('Sent to NBS Download Manager.', true);
  } else {
    notice(r.error || 'Could not reach the app.');
  }
});

refresh();
