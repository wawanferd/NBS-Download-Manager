'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

const ACTIVE = new Set(['connecting', 'downloading', 'merging']);
const FILTERS = {
  all: { title: 'All downloads', match: () => true },
  active: { title: 'Downloading', match: (d) => ACTIVE.has(d.status) },
  queued: { title: 'Queued', match: (d) => d.status === 'queued' },
  paused: { title: 'Paused', match: (d) => d.status === 'paused' },
  completed: { title: 'Completed', match: (d) => d.status === 'completed' },
  failed: { title: 'Failed', match: (d) => d.status === 'failed' },
};
const STATUS_LABEL = {
  queued: 'Queued',
  connecting: 'Connecting',
  downloading: 'Downloading',
  paused: 'Paused',
  merging: 'Merging',
  completed: 'Completed',
  failed: 'Failed',
};

const state = {
  downloads: new Map(),
  elements: new Map(),
  expanded: new Set(),
  filter: 'all',
  search: '',
  settings: null,
  bridge: null,
  appInfo: null,
  speedHistory: new Array(60).fill(0),
};

/* ---------- Formatting ---------- */

function fmtBytes(n) {
  if (n == null || Number.isNaN(n)) return '—';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  const digits = i === 0 ? 0 : n < 10 ? 2 : n < 100 ? 1 : 0;
  return `${n.toFixed(digits)} ${units[i]}`;
}

const fmtSpeed = (n) => `${fmtBytes(n || 0)}/s`;

function fmtEta(s) {
  if (s == null) return '—';
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  const h = Math.floor(s / 3600);
  if (h >= 48) return `${Math.floor(h / 24)}d ${h % 24}h`;
  return `${h}h ${Math.floor((s % 3600) / 60)}m`;
}

function fmtDate(ms) {
  if (!ms) return '—';
  return new Date(ms).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });
}

const limitLabel = (bytes) => (bytes ? fmtSpeed(bytes) : 'Unlimited');

function escapeHtml(s) {
  return String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
}

const FILE_KINDS = {
  archive: ['zip', 'rar', '7z', 'tar', 'gz', 'tgz', 'bz2', 'xz', 'zst'],
  video: ['mp4', 'mkv', 'avi', 'mov', 'webm', 'flv', 'wmv', 'm4v', 'ts'],
  audio: ['mp3', 'flac', 'wav', 'ogg', 'm4a', 'aac', 'opus', 'wma'],
  image: ['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp', 'tiff', 'heic', 'avif'],
  doc: ['pdf', 'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx', 'txt', 'odt', 'epub', 'csv', 'md'],
  app: ['exe', 'msi', 'dmg', 'pkg', 'deb', 'rpm', 'appimage', 'apk', 'iso', 'img', 'bin'],
  code: ['js', 'json', 'py', 'rs', 'html', 'css', 'xml', 'sh'],
};

function fileKind(name) {
  const ext = (name.includes('.') ? name.split('.').pop() : '').toLowerCase();
  const kind = Object.keys(FILE_KINDS).find((k) => FILE_KINDS[k].includes(ext));
  return { ext: ext ? ext.slice(0, 4).toUpperCase() : 'FILE', kind: kind || 'other' };
}

function setBadge(el, name) {
  const { ext, kind } = fileKind(name || '');
  el.textContent = ext;
  el.className = `${el.classList.contains('probe-badge') ? 'probe-badge ' : ''}file-badge badge-${kind}`;
}

/* ---------- Toasts, menu, dialogs ---------- */

function toast(message, type = 'info', timeout = 3800) {
  const el = document.createElement('div');
  el.className = `toast ${type}`;
  const name = type === 'success' ? 'checkCircle' : type === 'error' ? 'alert' : 'info';
  el.innerHTML = `${icon(name)}<span>${escapeHtml(message)}</span>`;
  $('#toasts').append(el);
  setTimeout(() => {
    el.classList.add('hide');
    setTimeout(() => el.remove(), 250);
  }, timeout);
}

async function call(cmd, args) {
  try {
    return await invoke(cmd, args);
  } catch (e) {
    toast(String(e), 'error', 6000);
    throw e;
  }
}

function closeMenu() {
  $('#menu').hidden = true;
}

function openMenu(anchor, entries) {
  const menu = $('#menu');
  menu.innerHTML = '';
  for (const entry of entries) {
    if (entry === '-') {
      menu.append(document.createElement('hr'));
      continue;
    }
    const btn = document.createElement('button');
    btn.type = 'button';
    if (entry.danger) btn.className = 'danger';
    btn.innerHTML = `${icon(entry.icon)}<span>${escapeHtml(entry.label)}</span>`;
    btn.addEventListener('click', () => {
      closeMenu();
      entry.action();
    });
    menu.append(btn);
  }
  menu.hidden = false;
  const r = anchor.getBoundingClientRect();
  const w = menu.offsetWidth;
  const h = menu.offsetHeight;
  menu.style.left = `${Math.max(8, Math.min(r.right - w, innerWidth - w - 8))}px`;
  menu.style.top = `${r.bottom + h + 6 > innerHeight ? r.top - h - 6 : r.bottom + 6}px`;
}

function setupDialog(dlg) {
  $$('[data-close]', dlg).forEach((b) => b.addEventListener('click', () => dlg.close('cancel')));
  dlg.addEventListener('mousedown', (e) => {
    if (e.target === dlg) dlg.close('cancel');
  });
}

/** Small confirm/prompt dialog. Resolves to true/false, or the number entered. */
function ask({ title, text = '', ok = 'OK', danger = false, input = null }) {
  const dlg = $('#dlg-confirm');
  $('#confirm-title').textContent = title;
  $('#confirm-text').textContent = text;
  const okBtn = $('#confirm-ok');
  okBtn.textContent = ok;
  okBtn.className = `btn ${danger ? 'btn-danger' : 'btn-primary'}`;
  $('#confirm-field').hidden = !input;
  if (input) {
    $('#confirm-input').value = input.value ?? '';
    $('#confirm-input').placeholder = input.placeholder ?? '';
    $('#confirm-suffix').textContent = input.suffix ?? '';
  }
  dlg.returnValue = '';
  dlg.showModal();
  if (input) $('#confirm-input').select();
  return new Promise((resolve) => {
    dlg.addEventListener(
      'close',
      () => {
        if (dlg.returnValue !== 'ok') return resolve(input ? null : false);
        resolve(input ? Number($('#confirm-input').value || 0) : true);
      },
      { once: true },
    );
  });
}

function setRangeFill(range) {
  const pct = ((range.value - range.min) / (range.max - range.min)) * 100;
  range.style.setProperty('--fill', `${pct}%`);
}

/* ---------- Download items ---------- */

function createItem(d) {
  const el = document.createElement('article');
  el.className = 'item';
  el.dataset.id = d.id;
  el.innerHTML = `
    <div class="item-row">
      <div class="file-badge"></div>
      <div class="item-body">
        <div class="item-head">
          <div class="item-name"></div>
          <span class="pill"></span>
          <span class="pill pill-browser" hidden>${icon('globe')} Browser</span>
        </div>
        <div class="segbar"></div>
        <div class="item-meta">
          <span class="meta-size"></span>
          <span class="meta-pct"></span>
          <span class="meta-speed">${icon('zap')}<b></b></span>
          <span class="meta-eta">${icon('clock')}<b></b></span>
          <span class="meta-conn">${icon('network')}<b></b></span>
          <span class="meta-limit">${icon('gauge')}<b></b></span>
          <span class="meta-error"></span>
        </div>
      </div>
      <div class="item-actions">
        <button class="icon-btn act-toggle"></button>
        <button class="icon-btn act-folder" title="Show in folder">${icon('folder')}</button>
        <button class="icon-btn act-more" title="More">${icon('more')}</button>
      </div>
    </div>
    <div class="item-details" hidden></div>`;
  el.refs = {
    badge: $('.file-badge', el),
    name: $('.item-name', el),
    pill: $('.pill', el),
    browser: $('.pill-browser', el),
    bar: $('.segbar', el),
    size: $('.meta-size', el),
    pct: $('.meta-pct', el),
    speed: $('.meta-speed', el),
    eta: $('.meta-eta', el),
    conn: $('.meta-conn', el),
    limit: $('.meta-limit', el),
    error: $('.meta-error', el),
    toggle: $('.act-toggle', el),
    details: $('.item-details', el),
  };

  $('.item-row', el).addEventListener('click', (e) => {
    if (e.target.closest('button')) return;
    toggleDetails(el.dataset.id);
  });
  $('.item-row', el).addEventListener('dblclick', (e) => {
    if (e.target.closest('button')) return;
    const cur = state.downloads.get(el.dataset.id);
    if (cur?.status === 'completed') call('open_download', { id: cur.id });
  });
  el.refs.toggle.addEventListener('click', () => primaryAction(state.downloads.get(el.dataset.id)));
  $('.act-folder', el).addEventListener('click', () => call('reveal_download', { id: el.dataset.id }));
  $('.act-more', el).addEventListener('click', (e) => {
    e.stopPropagation();
    openMenu(e.currentTarget, menuEntries(state.downloads.get(el.dataset.id)));
  });
  state.elements.set(d.id, el);
  return el;
}

function primaryAction(d) {
  if (!d) return;
  if (ACTIVE.has(d.status) || d.status === 'queued') call('pause_download', { id: d.id });
  else if (d.status === 'paused' || d.status === 'failed') call('resume_download', { id: d.id });
  else if (d.status === 'completed') call('open_download', { id: d.id });
}

function menuEntries(d) {
  if (!d) return [];
  const entries = [];
  if (d.status === 'completed') entries.push({ icon: 'external', label: 'Open file', action: () => call('open_download', { id: d.id }) });
  entries.push({ icon: 'folder', label: 'Show in folder', action: () => call('reveal_download', { id: d.id }) });
  entries.push({ icon: 'copy', label: 'Copy link', action: () => copyText(d.url) });
  if (d.status !== 'completed') {
    entries.push({ icon: 'gauge', label: 'Set speed limit…', action: () => promptDownloadLimit(d) });
  }
  if (!ACTIVE.has(d.status)) {
    entries.push({ icon: 'refresh', label: d.status === 'completed' ? 'Download again' : 'Restart from scratch', action: () => restart(d) });
  }
  entries.push('-');
  entries.push({ icon: 'x', label: 'Remove from list', action: () => remove(d, false) });
  entries.push({
    icon: 'trash',
    label: d.status === 'completed' ? 'Delete file' : 'Cancel & delete',
    danger: true,
    action: () => remove(d, true),
  });
  return entries;
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const ta = document.createElement('textarea');
    ta.value = text;
    document.body.append(ta);
    ta.select();
    document.execCommand('copy');
    ta.remove();
  }
  toast('Link copied to clipboard');
}

async function remove(d, deleteFile) {
  if (deleteFile) {
    const ok = await ask({
      title: d.status === 'completed' ? 'Delete file?' : 'Cancel and delete?',
      text: `“${d.fileName}” will be removed from the list and deleted from disk.`,
      ok: 'Delete',
      danger: true,
    });
    if (!ok) return;
  }
  await call('remove_download', { id: d.id, deleteFile });
}

async function restart(d) {
  if (d.downloaded > 0 && d.status !== 'completed') {
    const ok = await ask({ title: 'Restart download?', text: 'Downloaded progress will be discarded.', ok: 'Restart' });
    if (!ok) return;
  }
  await call('restart_download', { id: d.id });
}

async function promptDownloadLimit(d) {
  const kb = await ask({
    title: 'Speed limit for this download',
    text: 'Enter 0 for unlimited.',
    ok: 'Apply',
    input: { value: d.speedLimit ? Math.round(d.speedLimit / 1024) : '', placeholder: 'Unlimited', suffix: 'KB/s' },
  });
  if (kb == null) return;
  await call('set_download_speed_limit', { id: d.id, limit: kb > 0 ? Math.round(kb * 1024) : null });
}

function renderSegments(bar, d) {
  const done = d.status === 'completed';
  bar.classList.toggle('done', done);
  bar.classList.toggle('failed', d.status === 'failed');
  bar.classList.toggle('paused', d.status === 'paused' || d.status === 'queued');
  const unknown = !done && !d.totalSize && ACTIVE.has(d.status);
  bar.classList.toggle('indeterminate', unknown);

  const total = d.totalSize || 0;
  const segs = done ? [{ start: 0, len: 1, downloaded: 1, merged: false, active: false }] : total ? d.segments : [];
  const size = done ? 1 : total;
  while (bar.children.length > segs.length) bar.lastChild.remove();
  while (bar.children.length < segs.length) {
    const seg = document.createElement('div');
    seg.className = 'seg';
    seg.innerHTML = '<div class="seg-fill"></div>';
    bar.append(seg);
  }
  segs.forEach((s, i) => {
    const el = bar.children[i];
    const len = s.len ?? 0;
    el.style.left = `${(s.start / size) * 100}%`;
    el.style.width = `${(len / size) * 100}%`;
    el.firstChild.style.width = `${len ? Math.min(100, (s.downloaded / len) * 100) : 100}%`;
    el.classList.toggle('active', !!s.active && d.status === 'downloading');
    el.classList.toggle('merged', !!s.merged);
  });
}

function updateItem(el, d) {
  const r = el.refs;
  if (r.name.textContent !== d.fileName) {
    r.name.textContent = d.fileName;
    r.name.title = d.fileName;
    setBadge(r.badge, d.fileName);
  }
  r.pill.className = `pill pill-${d.status}`;
  r.pill.textContent = STATUS_LABEL[d.status] || d.status;
  r.browser.hidden = d.source !== 'browser';

  renderSegments(r.bar, d);

  const total = d.totalSize;
  const running = d.status === 'downloading';
  r.size.textContent = d.status === 'completed' ? fmtBytes(total ?? d.downloaded) : total != null ? `${fmtBytes(d.downloaded)} of ${fmtBytes(total)}` : fmtBytes(d.downloaded);
  r.pct.hidden = !total || d.status === 'completed';
  r.pct.textContent = total ? `${Math.floor((d.downloaded / total) * 100)}%` : '';
  r.speed.hidden = !running;
  r.speed.lastChild.textContent = fmtSpeed(d.speed);
  r.eta.hidden = !running || d.eta == null;
  r.eta.lastChild.textContent = `${fmtEta(d.eta)} left`;
  r.conn.hidden = !(running || d.status === 'connecting');
  r.conn.lastChild.textContent = `${d.activeConnections}/${d.resumable ? d.connections : 1}`;
  r.limit.hidden = !d.speedLimit || d.status === 'completed';
  r.limit.lastChild.textContent = fmtSpeed(d.speedLimit);
  r.error.hidden = d.status !== 'failed';
  r.error.textContent = d.error || '';
  if (d.status === 'completed') r.size.textContent += ` · ${fmtDate(d.completedAt)}`;
  else if (d.downloaded === 0 && d.status === 'queued') r.size.textContent = 'Waiting for a free slot…';
  else if (d.downloaded === 0 && d.status === 'connecting') r.size.textContent = 'Connecting…';

  const toggle = ACTIVE.has(d.status) || d.status === 'queued' ? ['pause', 'Pause'] : d.status === 'completed' ? ['external', 'Open'] : ['play', d.status === 'failed' ? 'Retry' : 'Resume'];
  if (r.toggle.dataset.kind !== toggle[0]) {
    r.toggle.dataset.kind = toggle[0];
    r.toggle.innerHTML = icon(toggle[0]);
  }
  r.toggle.title = toggle[1];

  el.classList.toggle('expanded', state.expanded.has(d.id));
  r.details.hidden = !state.expanded.has(d.id);
  if (state.expanded.has(d.id)) renderDetails(el, d);
}

function toggleDetails(id) {
  if (state.expanded.has(id)) state.expanded.delete(id);
  else state.expanded.add(id);
  const el = state.elements.get(id);
  if (el) {
    el.refs.details.innerHTML = '';
    updateItem(el, state.downloads.get(id));
  }
}

function renderDetails(el, d) {
  const box = el.refs.details;
  if (!box.firstChild) {
    box.innerHTML = `
      <dl class="details-info">
        <dt>Address</dt><dd class="mono d-url"></dd>
        <dt>Saved to</dt><dd class="d-path"></dd>
        <dt>Size</dt><dd class="d-size"></dd>
        <dt>Resume</dt><dd class="d-resume"></dd>
        <dt>Added</dt><dd class="d-added"></dd>
      </dl>
      <div class="details-controls">
        <div class="field">
          <label>Connections <output class="pill-count d-conn-out"></output></label>
          <input type="range" min="1" max="8" class="d-conn" />
        </div>
        <div class="field">
          <label>Speed limit</label>
          <div class="input-suffix"><input type="number" min="0" placeholder="Unlimited" class="d-limit" /><span>KB/s</span></div>
        </div>
      </div>
      <div class="details-error" hidden></div>
      <div class="details-segments">
        <table class="seg-table">
          <thead><tr><th>#</th><th>Range</th><th>Downloaded</th><th style="width:40%">Progress</th><th>State</th></tr></thead>
          <tbody></tbody>
        </table>
      </div>`;
    const conn = $('.d-conn', box);
    conn.addEventListener('input', () => {
      $('.d-conn-out', box).textContent = conn.value;
      setRangeFill(conn);
    });
    conn.addEventListener('change', () => call('set_download_connections', { id: d.id, connections: Number(conn.value) }));
    const limit = $('.d-limit', box);
    limit.addEventListener('change', () => {
      const kb = Number(limit.value || 0);
      call('set_download_speed_limit', { id: d.id, limit: kb > 0 ? Math.round(kb * 1024) : null });
    });
  }

  $('.d-url', box).textContent = d.url;
  $('.d-path', box).textContent = d.finalPath || d.saveDir;
  $('.d-size', box).textContent = d.totalSize != null ? `${fmtBytes(d.totalSize)} (${d.totalSize.toLocaleString()} bytes)` : 'Unknown';
  $('.d-resume', box).textContent = d.totalSize == null && d.status === 'queued' ? '—' : d.resumable ? 'Supported — segmented download' : 'Not supported — single connection';
  $('.d-added', box).textContent = fmtDate(d.createdAt);

  const conn = $('.d-conn', box);
  if (document.activeElement !== conn) {
    conn.value = d.connections;
    $('.d-conn-out', box).textContent = d.connections;
    setRangeFill(conn);
  }
  conn.disabled = d.status === 'completed';
  const limit = $('.d-limit', box);
  if (document.activeElement !== limit) limit.value = d.speedLimit ? Math.round(d.speedLimit / 1024) : '';
  limit.disabled = d.status === 'completed';

  const err = $('.details-error', box);
  err.hidden = !d.error;
  if (d.error) err.innerHTML = `${icon('alert')}<span>${escapeHtml(d.error)}</span>`;

  const tbody = $('tbody', box);
  $('.details-segments', box).hidden = d.segments.length === 0 || d.status === 'completed';
  const rows = d.segments
    .map((s, i) => {
      const end = s.len != null ? s.start + s.len - 1 : null;
      const pct = s.len ? Math.min(100, (s.downloaded / s.len) * 100) : s.len === 0 ? 100 : 0;
      const st = s.merged ? ['merged', 'Merged'] : s.active ? ['active', 'Receiving'] : s.len != null && s.downloaded >= s.len ? ['merged', 'Done'] : ['', 'Waiting'];
      return `<tr><td>${i + 1}</td><td>${fmtBytes(s.start)} – ${end != null ? fmtBytes(end) : '?'}</td><td>${fmtBytes(s.downloaded)}</td>
        <td><div class="mini-bar"><div style="width:${pct}%"></div></div></td><td><span class="seg-state ${st[0]}">${st[1]}</span></td></tr>`;
    })
    .join('');
  if (tbody.innerHTML !== rows) tbody.innerHTML = rows;
}

/* ---------- List & stats ---------- */

function visibleDownloads() {
  const f = FILTERS[state.filter].match;
  const q = state.search.trim().toLowerCase();
  return [...state.downloads.values()]
    .filter(f)
    .filter((d) => !q || d.fileName.toLowerCase().includes(q) || d.url.toLowerCase().includes(q))
    .sort((a, b) => b.createdAt - a.createdAt);
}

function renderList() {
  const list = $('#list');
  const items = visibleDownloads();
  const keep = new Set(items.map((d) => d.id));
  for (const el of [...list.children]) if (!keep.has(el.dataset.id)) el.remove();
  items.forEach((d, i) => {
    const el = state.elements.get(d.id) || createItem(d);
    updateItem(el, d);
    if (list.children[i] !== el) list.insertBefore(el, list.children[i] || null);
  });

  const all = [...state.downloads.values()];
  for (const key of Object.keys(FILTERS)) {
    const n = all.filter(FILTERS[key].match).length;
    const badge = $(`[data-count="${key}"]`);
    if (badge) badge.textContent = n;
  }
  $('#view-title').textContent = FILTERS[state.filter].title;
  $('#view-subtitle').textContent = `${items.length} ${items.length === 1 ? 'item' : 'items'}${state.search ? ` matching “${state.search}”` : ''}`;

  const empty = items.length === 0;
  $('#empty').hidden = !empty;
  list.hidden = empty;
  if (empty) {
    const searching = state.search || state.filter !== 'all';
    $('#empty-title').textContent = searching ? 'Nothing here' : 'No downloads yet';
    $('#empty-text').hidden = !!searching;
    $('#btn-empty-add').hidden = !!searching;
  }
  renderStats();
}

function renderStats() {
  const all = [...state.downloads.values()];
  const active = all.filter((d) => ACTIVE.has(d.status));
  const speed = active.reduce((s, d) => s + (d.speed || 0), 0);
  $('#stat-speed').textContent = fmtSpeed(speed);
  $('#stat-active').textContent = active.length;
  const slots = state.settings?.maxConcurrent ?? 3;
  const queued = all.filter((d) => d.status === 'queued').length;
  $('#stat-active-hint').textContent = `of ${slots} slots${queued ? ` · ${queued} queued` : ''}`;

  const pending = all.filter((d) => d.status !== 'completed' && d.totalSize != null);
  const remaining = pending.reduce((s, d) => s + Math.max(0, d.totalSize - d.downloaded), 0);
  $('#stat-remaining').textContent = pending.length ? fmtBytes(remaining) : '—';
  const eta = speed > 0 ? Math.round(active.reduce((s, d) => s + Math.max(0, (d.totalSize || 0) - d.downloaded), 0) / speed) : null;
  $('#stat-remaining-hint').textContent = pending.length ? (eta != null ? `about ${fmtEta(eta)} at current speed` : `in ${pending.length} download${pending.length > 1 ? 's' : ''}`) : 'nothing pending';

  const completed = all.filter((d) => d.status === 'completed');
  $('#stat-completed').textContent = completed.length;
  $('#stat-completed-hint').textContent = `${fmtBytes(completed.reduce((s, d) => s + (d.totalSize || 0), 0))} total`;
  $('#stat-limit-hint').textContent = state.settings?.globalSpeedLimit ? `Limited to ${fmtSpeed(state.settings.globalSpeedLimit)}` : 'No limit';
  return speed;
}

function drawSparkline() {
  const canvas = $('#sparkline');
  const dpr = devicePixelRatio || 1;
  const w = canvas.clientWidth;
  const h = canvas.clientHeight;
  if (!w || !h) return;
  if (canvas.width !== w * dpr || canvas.height !== h * dpr) {
    canvas.width = w * dpr;
    canvas.height = h * dpr;
  }
  const ctx = canvas.getContext('2d');
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
  const data = state.speedHistory;
  const max = Math.max(...data, 1);
  const step = w / (data.length - 1);
  const y = (v) => h - 3 - (v / max) * (h - 8);

  ctx.beginPath();
  ctx.moveTo(0, y(data[0]));
  for (let i = 1; i < data.length; i++) {
    const x0 = (i - 1) * step;
    const x1 = i * step;
    const xm = (x0 + x1) / 2;
    ctx.bezierCurveTo(xm, y(data[i - 1]), xm, y(data[i]), x1, y(data[i]));
  }
  const stroke = ctx.createLinearGradient(0, 0, w, 0);
  stroke.addColorStop(0, '#4f6bff');
  stroke.addColorStop(1, '#8b5cf6');
  ctx.strokeStyle = stroke;
  ctx.lineWidth = 2;
  ctx.stroke();
  ctx.lineTo(w, h);
  ctx.lineTo(0, h);
  ctx.closePath();
  const fill = ctx.createLinearGradient(0, 0, 0, h);
  fill.addColorStop(0, 'rgba(110, 110, 255, 0.28)');
  fill.addColorStop(1, 'rgba(110, 110, 255, 0)');
  ctx.fillStyle = fill;
  ctx.fill();
}

/* ---------- Event handling ---------- */

function upsert(d) {
  const prev = state.downloads.get(d.id);
  state.downloads.set(d.id, d);
  return !prev || prev.status !== d.status || prev.fileName !== d.fileName;
}

function onProgress(views) {
  let structural = false;
  for (const d of views) {
    if (upsert(d)) structural = true;
    else {
      const el = state.elements.get(d.id);
      if (el && el.isConnected) updateItem(el, d);
    }
  }
  if (structural) renderList();
  else renderStats();
}

/* ---------- Theme ---------- */

function effectiveTheme() {
  const t = state.settings?.theme || 'system';
  if (t !== 'system') return t;
  return matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
}

function applyTheme() {
  const t = state.settings?.theme || 'system';
  if (t === 'system') document.documentElement.removeAttribute('data-theme');
  else document.documentElement.dataset.theme = t;
  $('#btn-theme').innerHTML = icon(effectiveTheme() === 'dark' ? 'sun' : 'moon');
}

async function saveSettings(patch) {
  const next = { ...state.settings, ...patch };
  state.settings = await call('save_settings', { settings: next });
  applyTheme();
  renderGlobalLimit();
  renderStats();
  return state.settings;
}

/* ---------- Global speed limit ---------- */

function renderGlobalLimit() {
  const select = $('#global-limit');
  const value = String(state.settings?.globalSpeedLimit || 0);
  $$('option[data-custom]', select).forEach((o) => o.remove());
  if (![...select.options].some((o) => o.value === value)) {
    const opt = new Option(fmtSpeed(Number(value)), value);
    opt.dataset.custom = '1';
    select.insertBefore(opt, select.querySelector('option[value="custom"]'));
  }
  select.value = value;
}

async function onGlobalLimitChange() {
  const select = $('#global-limit');
  let bytes;
  if (select.value === 'custom') {
    const kb = await ask({
      title: 'Global speed limit',
      text: 'Maximum combined speed of all downloads. Enter 0 for unlimited.',
      ok: 'Apply',
      input: { value: state.settings.globalSpeedLimit ? Math.round(state.settings.globalSpeedLimit / 1024) : '', placeholder: 'Unlimited', suffix: 'KB/s' },
    });
    if (kb == null) return renderGlobalLimit();
    bytes = kb > 0 ? Math.round(kb * 1024) : 0;
  } else {
    bytes = Number(select.value);
  }
  await call('set_global_speed_limit', { limit: bytes || null });
  state.settings.globalSpeedLimit = bytes || null;
  renderGlobalLimit();
  renderStats();
  toast(bytes ? `Speed limited to ${fmtSpeed(bytes)}` : 'Speed limit removed');
}

/* ---------- Browser bridge ---------- */

function renderBridge() {
  const b = state.bridge;
  const dot = $('#bridge-dot');
  const sub = $('#bridge-sub');
  if (!b) return;
  if (b.error) {
    dot.className = 'dot err';
    sub.textContent = 'Port unavailable';
  } else if (!b.listening) {
    dot.className = 'dot';
    sub.textContent = 'Starting…';
  } else if (b.clients > 0) {
    dot.className = 'dot ok';
    sub.textContent = `Connected · ${b.clients} browser${b.clients > 1 ? 's' : ''}`;
  } else {
    dot.className = 'dot idle';
    sub.textContent = `Waiting for extension · :${b.port}`;
  }
  const status = $('#set-bridge-status');
  if (status) {
    status.innerHTML = `<span class="${dot.className}"></span><span>${escapeHtml(
      b.error ? b.error : b.listening ? `Listening on ws://127.0.0.1:${b.port} — ${b.clients} extension connection${b.clients === 1 ? '' : 's'}` : 'Starting…',
    )}</span>`;
  }
}

/* ---------- Add dialog ---------- */

const addState = { headers: [], nameEdited: false, probeSeq: 0, probeTimer: null, probed: null, source: null };

function renderConnViz(container, n) {
  if (container.children.length !== 8) container.innerHTML = '<span></span>'.repeat(8);
  [...container.children].forEach((s, i) => s.classList.toggle('on', i < n));
}

function openAddDialog({ url = '', fileName = '', headers = [], fileSize = null, source = null } = {}) {
  const dlg = $('#dlg-add');
  addState.headers = headers;
  addState.nameEdited = !!fileName;
  addState.probed = null;
  addState.source = source;
  $('#add-url').value = url;
  $('#add-name').value = fileName || '';
  $('#add-dir').value = state.settings?.downloadDir || '';
  const conn = $('#add-conn');
  conn.value = state.settings?.defaultConnections ?? 8;
  $('#add-conn-out').textContent = conn.value;
  setRangeFill(conn);
  renderConnViz($('#add-conn-viz'), Number(conn.value));
  $('#add-limit').value = '';
  $('#add-start').checked = true;
  $('#add-source').textContent = source === 'browser' ? 'Captured from your browser' : 'Enter a link to download';
  setProbe('idle');
  if (fileSize) setProbe('ok', { fileName: fileName || 'download', totalSize: fileSize, resumable: null });
  if (!dlg.open) dlg.showModal();
  if (url) scheduleProbe(0);
  else $('#add-url').focus();
}

function setProbe(kind, info) {
  const card = $('#probe');
  card.dataset.state = kind;
  const meta = $('#probe-meta');
  if (kind === 'loading') {
    $('#probe-title').textContent = 'Checking link…';
    meta.innerHTML = '';
    setBadge($('#probe-badge'), '');
  } else if (kind === 'error') {
    $('#probe-title').textContent = info;
    meta.innerHTML = '<span>You can still try to download it.</span>';
  } else if (kind === 'ok') {
    $('#probe-title').textContent = info.fileName;
    setBadge($('#probe-badge'), info.fileName);
    const tags = [`<span class="tag">${info.totalSize != null ? fmtBytes(info.totalSize) : 'Unknown size'}</span>`];
    if (info.resumable === true) tags.push('<span class="tag ok">Resumable · up to 8 connections</span>');
    if (info.resumable === false) tags.push('<span class="tag warn">No resume · single connection</span>');
    if (info.contentType) tags.push(`<span class="tag">${escapeHtml(info.contentType.split(';')[0])}</span>`);
    meta.innerHTML = tags.join('');
  }
}

function scheduleProbe(delay = 450) {
  clearTimeout(addState.probeTimer);
  addState.probeTimer = setTimeout(runProbe, delay);
}

async function runProbe() {
  const url = $('#add-url').value.trim();
  if (!/^https?:\/\/\S+/i.test(url)) {
    setProbe('idle');
    return;
  }
  const seq = ++addState.probeSeq;
  setProbe('loading');
  try {
    const info = await invoke('probe_url', { url, headers: addState.headers });
    if (seq !== addState.probeSeq) return;
    addState.probed = info;
    if (!addState.nameEdited) $('#add-name').value = info.fileName;
    setProbe('ok', info);
  } catch (e) {
    if (seq !== addState.probeSeq) return;
    setProbe('error', String(e));
  }
}

async function submitAdd(e) {
  e.preventDefault();
  const url = $('#add-url').value.trim();
  if (!/^https?:\/\/\S+/i.test(url)) {
    toast('Please enter a valid http(s) link', 'error');
    $('#add-url').focus();
    return;
  }
  const kb = Number($('#add-limit').value || 0);
  const request = {
    url,
    fileName: $('#add-name').value.trim() || null,
    saveDir: $('#add-dir').value.trim() || null,
    connections: Number($('#add-conn').value),
    speedLimit: kb > 0 ? Math.round(kb * 1024) : null,
    headers: addState.headers,
    start: $('#add-start').checked,
    source: addState.source,
  };
  const btn = $('#add-submit');
  btn.disabled = true;
  try {
    const d = await call('add_download', { request });
    $('#dlg-add').close('ok');
    toast(`Added “${d.fileName}”`, 'success');
    if (state.filter !== 'all' && !FILTERS[state.filter].match(d)) selectFilter('all');
  } finally {
    btn.disabled = false;
  }
}

/* ---------- Settings dialog ---------- */

function openSettings() {
  const s = state.settings;
  $('#set-dir').value = s.downloadDir;
  const conn = $('#set-conn');
  conn.value = s.defaultConnections;
  $('#set-conn-out').textContent = conn.value;
  setRangeFill(conn);
  $('#set-concurrent').value = s.maxConcurrent;
  $('#set-limit').value = s.globalSpeedLimit ? Math.round(s.globalSpeedLimit / 1024) : '';
  $('#set-port').value = s.bridgePort;
  $('#set-autostart').checked = s.autoStartBrowser;
  $('#set-tray').checked = s.closeToTray;
  $('#set-notify').checked = s.notifyOnComplete;
  $$('#set-theme button').forEach((b) => b.classList.toggle('active', b.dataset.value === s.theme));
  if (state.appInfo) {
    $('#settings-version').textContent = `NBS Download Manager ${state.appInfo.version}`;
    $('#set-ext-ids').innerHTML = `Extension IDs: Chrome/Edge <code>${state.appInfo.chromeExtensionId}</code> · Firefox <code>${escapeHtml(state.appInfo.firefoxExtensionId)}</code>. Load the <code>extension</code> folder as an unpacked extension.`;
  }
  renderBridge();
  $('#dlg-settings').showModal();
}

async function submitSettings(e) {
  e.preventDefault();
  const kb = Number($('#set-limit').value || 0);
  const port = Number($('#set-port').value || 0);
  if (port < 1024 || port > 65535) {
    toast('Port must be between 1024 and 65535', 'error');
    return;
  }
  await saveSettings({
    downloadDir: $('#set-dir').value.trim() || state.settings.downloadDir,
    defaultConnections: Number($('#set-conn').value),
    maxConcurrent: Math.max(1, Math.min(16, Number($('#set-concurrent').value || 3))),
    globalSpeedLimit: kb > 0 ? Math.round(kb * 1024) : null,
    bridgePort: port,
    autoStartBrowser: $('#set-autostart').checked,
    closeToTray: $('#set-tray').checked,
    notifyOnComplete: $('#set-notify').checked,
    theme: $('#set-theme button.active')?.dataset.value || 'system',
  });
  $('#dlg-settings').close('ok');
  toast('Settings saved', 'success');
}

/* ---------- Wiring ---------- */

function selectFilter(filter) {
  state.filter = filter;
  $$('.nav-item').forEach((b) => b.classList.toggle('active', b.dataset.filter === filter));
  renderList();
}

function bindUi() {
  hydrateIcons();
  $$('.dialog').forEach(setupDialog);

  $('#btn-add').addEventListener('click', () => openAddDialog());
  $('#btn-empty-add').addEventListener('click', () => openAddDialog());
  $$('.nav-item').forEach((b) => b.addEventListener('click', () => selectFilter(b.dataset.filter)));
  $('#search').addEventListener('input', (e) => {
    state.search = e.target.value;
    renderList();
  });
  $('#btn-resume-all').addEventListener('click', () => call('resume_all'));
  $('#btn-pause-all').addEventListener('click', () => call('pause_all'));
  $('#btn-clear').addEventListener('click', () => call('clear_completed'));
  $('#btn-settings').addEventListener('click', openSettings);
  $('#bridge-card').addEventListener('click', openSettings);
  $('#btn-theme').addEventListener('click', () => saveSettings({ theme: effectiveTheme() === 'dark' ? 'light' : 'dark' }));
  $('#global-limit').addEventListener('change', onGlobalLimitChange);

  // Add dialog
  $('#add-form').addEventListener('submit', submitAdd);
  $('#add-url').addEventListener('input', () => {
    addState.headers = [];
    scheduleProbe();
  });
  $('#add-name').addEventListener('input', () => (addState.nameEdited = $('#add-name').value.trim() !== ''));
  $('#add-conn').addEventListener('input', (e) => {
    $('#add-conn-out').textContent = e.target.value;
    setRangeFill(e.target);
    renderConnViz($('#add-conn-viz'), Number(e.target.value));
  });
  $('#add-browse').addEventListener('click', async () => {
    const dir = await call('pick_folder', { current: $('#add-dir').value });
    if (dir) $('#add-dir').value = dir;
  });

  // Settings dialog
  $('#settings-form').addEventListener('submit', submitSettings);
  $('#set-conn').addEventListener('input', (e) => {
    $('#set-conn-out').textContent = e.target.value;
    setRangeFill(e.target);
  });
  $('#set-browse').addEventListener('click', async () => {
    const dir = await call('pick_folder', { current: $('#set-dir').value });
    if (dir) $('#set-dir').value = dir;
  });
  $$('#set-theme button').forEach((b) =>
    b.addEventListener('click', () => $$('#set-theme button').forEach((x) => x.classList.toggle('active', x === b))),
  );
  $$('#native-buttons button').forEach((b) =>
    b.addEventListener('click', async () => {
      const path = await call('install_native_host', { browser: b.dataset.browser });
      toast(`Native host registered for ${b.textContent}: ${path}`, 'success', 6000);
    }),
  );

  // Global shortcuts
  document.addEventListener('click', (e) => {
    if (!e.target.closest('#menu')) closeMenu();
  });
  window.addEventListener('blur', closeMenu);
  window.addEventListener('resize', closeMenu);
  $('#list').addEventListener('scroll', closeMenu, { passive: true });
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') closeMenu();
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'n') {
      e.preventDefault();
      openAddDialog();
    }
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'f') {
      e.preventDefault();
      $('#search').focus();
    }
  });
  // Pasting a link anywhere opens the new download dialog.
  document.addEventListener('paste', (e) => {
    if (e.target.closest('input, textarea') || $('dialog[open]')) return;
    const text = e.clipboardData?.getData('text')?.trim();
    if (text && /^https?:\/\/\S+$/i.test(text)) openAddDialog({ url: text });
  });
  matchMedia('(prefers-color-scheme: dark)').addEventListener('change', applyTheme);
}

async function init() {
  bindUi();
  const [settings, downloads, bridge, appInfo] = await Promise.all([
    invoke('get_settings'),
    invoke('list_downloads'),
    invoke('bridge_status'),
    invoke('app_info'),
  ]);
  state.settings = settings;
  state.bridge = bridge;
  state.appInfo = appInfo;
  downloads.forEach(upsert);
  applyTheme();
  renderGlobalLimit();
  renderBridge();
  renderList();

  await listen('download://updated', (e) => {
    upsert(e.payload);
    renderList();
  });
  await listen('download://progress', (e) => onProgress(e.payload));
  await listen('download://removed', (e) => {
    state.downloads.delete(e.payload);
    state.elements.get(e.payload)?.remove();
    state.elements.delete(e.payload);
    state.expanded.delete(e.payload);
    renderList();
  });
  await listen('download://completed', (e) => toast(`Download complete: ${e.payload.fileName}`, 'success'));
  await listen('bridge://status', (e) => {
    state.bridge = e.payload;
    renderBridge();
  });
  await listen('bridge://download-request', (e) => {
    const p = e.payload;
    openAddDialog({ url: p.url, fileName: p.fileName || '', headers: p.headers || [], fileSize: p.fileSize, source: 'browser' });
  });
  await listen('bridge://captured', (e) => toast(`Captured from browser: ${e.payload.fileName}`, 'success'));

  // Speed graph: one sample per second, even when nothing is downloading.
  setInterval(() => {
    state.speedHistory.push(renderStats());
    state.speedHistory.shift();
    drawSparkline();
  }, 1000);
  drawSparkline();
}

init().catch((e) => {
  console.error(e);
  toast(`Failed to start: ${e}`, 'error', 10000);
});
