'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

const ACTIVE = new Set(['connecting', 'downloading', 'merging']);
const STATUS_LABEL = {
  queued: 'Queued',
  connecting: 'Connecting',
  downloading: 'Downloading',
  paused: 'Paused',
  merging: 'Merging',
  completed: 'Complete',
  failed: 'Failed',
};

const CATEGORIES = {
  archive: { label: 'Compressed', icon: 'archive', ext: ['zip', 'rar', '7z', 'tar', 'gz', 'tgz', 'bz2', 'xz', 'zst', 'cab'] },
  video: { label: 'Video', icon: 'video', ext: ['mp4', 'mkv', 'avi', 'mov', 'webm', 'flv', 'wmv', 'm4v', 'ts', 'mpg', 'mpeg'] },
  audio: { label: 'Music', icon: 'music', ext: ['mp3', 'flac', 'wav', 'ogg', 'm4a', 'aac', 'opus', 'wma'] },
  app: { label: 'Program', icon: 'program', ext: ['exe', 'msi', 'dmg', 'pkg', 'deb', 'rpm', 'appimage', 'apk', 'iso', 'img', 'bin', 'jar'] },
  doc: { label: 'Document', icon: 'fileText', ext: ['pdf', 'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx', 'txt', 'odt', 'ods', 'epub', 'csv', 'md', 'rtf'] },
  image: { label: 'Image', icon: 'image', ext: ['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp', 'tiff', 'heic', 'avif', 'psd'] },
  other: { label: 'File', icon: 'file', ext: [] },
};

const FILTERS = {
  all: { title: 'All Downloads', match: () => true },
  active: { title: 'Downloading', match: (d) => ACTIVE.has(d.status) || d.status === 'queued' },
  unfinished: { title: 'Unfinished', match: (d) => d.status !== 'completed' },
  completed: { title: 'Finished', match: (d) => d.status === 'completed' },
  failed: { title: 'Failed', match: (d) => d.status === 'failed' },
  torrent: { title: 'Torrents', match: (d) => isTorrent(d) },
  stream: { title: 'Streams', match: (d) => isTorrent(d) && d.stream },
  seeding: { title: 'Seeding', match: (d) => isTorrent(d) && d.seeding },
};
for (const [key, cat] of Object.entries(CATEGORIES)) {
  FILTERS[`cat:${key}`] = { title: key === 'other' ? 'Other files' : `${cat.label}${cat.label.endsWith('c') ? '' : 's'}`, match: (d) => category(d.fileName) === key };
}
FILTERS['cat:archive'].title = 'Compressed';
FILTERS['cat:audio'].title = 'Music';

const state = {
  downloads: new Map(),
  rows: new Map(),
  selected: new Set(),
  focus: null,
  anchor: null,
  filter: 'all',
  search: '',
  sort: { key: 'added', dir: -1 },
  settings: null,
  bridge: null,
  appInfo: null,
  torrentStatus: null,
  speedHistory: new Array(60).fill(0),
};

/* ---------- Helpers ---------- */

function fmtBytes(n) {
  if (n == null || Number.isNaN(n)) return '—';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(i === 0 ? 0 : n < 10 ? 2 : n < 100 ? 1 : 0)} ${units[i]}`;
}
const fmtSpeed = (n) => `${fmtBytes(n || 0)}/s`;

function fmtEta(s) {
  if (s == null) return '—';
  if (s < 60) return `${s} sec`;
  if (s < 3600) return `${Math.floor(s / 60)} min ${s % 60} sec`;
  const h = Math.floor(s / 3600);
  if (h >= 48) return `${Math.floor(h / 24)} d ${h % 24} h`;
  return `${h} h ${Math.floor((s % 3600) / 60)} min`;
}

function fmtDate(ms, long = false) {
  if (!ms) return '—';
  const d = new Date(ms);
  const today = new Date();
  if (!long && d.toDateString() === today.toDateString()) return `Today, ${d.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' })}`;
  return d.toLocaleString(undefined, long ? { dateStyle: 'medium', timeStyle: 'short' } : { year: 'numeric', month: '2-digit', day: '2-digit' });
}

function escapeHtml(s) {
  return String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
}

function category(name) {
  const ext = (name || '').includes('.') ? name.split('.').pop().toLowerCase() : '';
  return Object.keys(CATEGORIES).find((k) => CATEGORIES[k].ext.includes(ext)) || 'other';
}

function host(url) {
  try {
    return new URL(url).host;
  } catch {
    return '';
  }
}

function setTile(el, name, torrent = false) {
  const cat = torrent ? 'torrent' : category(name);
  const size = el.classList.contains('ftile-lg') ? ' ftile-lg' : '';
  el.className = `ftile${size} cat-${cat}`;
  el.innerHTML = icon(torrent ? 'magnet' : CATEGORIES[cat].icon);
}

const isUrl = (s) => /^https?:\/\/\S+$/i.test((s || '').trim());
const progress = (d) => (d.status === 'completed' ? 1 : d.totalSize ? Math.min(1, d.downloaded / d.totalSize) : 0);

function isTorrent(d) {
  return d?.kind === 'torrent';
}

/** Magnet links, bare info-hashes and links to .torrent files. */
function isTorrentLink(s) {
  const t = (s || '').trim();
  return /^magnet:\?/i.test(t) || /^[0-9a-f]{40}$/i.test(t) || /^[a-z2-7]{32}$/i.test(t) || /^https?:\/\/[^?#\s]+\.torrent([?#]\S*)?$/i.test(t);
}

function statusLabel(d) {
  if (isTorrent(d)) {
    if (d.status === 'connecting') return 'Checking';
    if (d.status === 'completed') return d.seeding ? 'Seeding' : 'Complete';
  }
  return STATUS_LABEL[d.status];
}

const isMedia = (f) => f.kind === 'video' || f.kind === 'audio';
/** The file to play: the largest selected video, else any video or audio. */
function bestMedia(t) {
  const media = (t.files || []).filter(isMedia);
  const by = (list) => list.sort((a, b) => (b.kind === 'video') - (a.kind === 'video') || b.size - a.size)[0];
  return by(media.filter((f) => f.included)) || by(media) || null;
}

/* ---------- Toasts, menu, dialogs ---------- */

function toast(message, type = 'info', timeout = 3800) {
  const el = document.createElement('div');
  el.className = `toast ${type}`;
  el.innerHTML = `${icon(type === 'success' ? 'checkCircle' : type === 'error' ? 'alert' : 'info')}<span>${escapeHtml(message)}</span>`;
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

function openMenu(entries, x, y) {
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
  const w = menu.offsetWidth;
  const h = menu.offsetHeight;
  menu.style.left = `${Math.max(8, Math.min(x, innerWidth - w - 8))}px`;
  menu.style.top = `${Math.max(8, y + h > innerHeight - 8 ? y - h : y)}px`;
}

function setupDialog(dlg) {
  $$('[data-close]', dlg).forEach((b) => b.addEventListener('click', () => dlg.close('cancel')));
  dlg.addEventListener('mousedown', (e) => {
    if (e.target === dlg) dlg.close('cancel');
  });
}

/**
 * Small confirm/prompt dialog. Resolves to false/true, the number entered
 * (`input`), or `{ ok, checked }` when a checkbox is shown.
 */
function ask({ title, text = '', ok = 'OK', danger = false, input = null, checkbox = null }) {
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
  $('#confirm-check-row').hidden = !checkbox;
  if (checkbox) {
    $('#confirm-check-label').textContent = checkbox.label;
    $('#confirm-check').checked = !!checkbox.checked;
  }
  dlg.returnValue = '';
  dlg.showModal();
  if (input) $('#confirm-input').select();
  return new Promise((resolve) => {
    dlg.addEventListener(
      'close',
      () => {
        const confirmed = dlg.returnValue === 'ok';
        if (input) return resolve(confirmed ? Number($('#confirm-input').value || 0) : null);
        if (checkbox) return resolve({ ok: confirmed, checked: $('#confirm-check').checked });
        resolve(confirmed);
      },
      { once: true },
    );
  });
}

/** 1–8 connection picker. */
function renderConnPicker(container, value, onChange) {
  if (container.children.length !== 8) {
    container.innerHTML = '';
    for (let i = 1; i <= 8; i++) {
      const b = document.createElement('button');
      b.type = 'button';
      b.textContent = i;
      b.addEventListener('click', () => {
        renderConnPicker(container, i, container._onChange);
        container._onChange?.(i);
      });
      container.append(b);
    }
  }
  container._onChange = onChange;
  container.dataset.value = value;
  [...container.children].forEach((b, i) => {
    b.classList.toggle('on', i + 1 < value);
    b.classList.toggle('current', i + 1 === value);
  });
}

/* ---------- Actions ---------- */

function selectedDownloads() {
  return [...state.selected].map((id) => state.downloads.get(id)).filter(Boolean);
}

function canResume(d) {
  return d.status === 'paused' || d.status === 'failed';
}
function canPause(d) {
  return ACTIVE.has(d.status) || d.status === 'queued' || (isTorrent(d) && d.seeding);
}

const pauseItem = (d) => call(isTorrent(d) ? 'torrent_pause' : 'pause_download', { id: d.id });
const resumeItem = (d) => call(isTorrent(d) ? 'torrent_resume' : 'resume_download', { id: d.id });
const openItem = (d) => call(isTorrent(d) ? 'torrent_open' : 'open_download', { id: d.id });
const revealItem = (d) => call(isTorrent(d) ? 'torrent_reveal' : 'reveal_download', { id: d.id });

function primaryAction(d) {
  if (!d) return;
  if (canPause(d)) pauseItem(d);
  else if (canResume(d)) resumeItem(d);
  else if (d.status === 'completed') openItem(d);
}

async function deleteDownloads(list) {
  if (!list.length) return;
  const one = list.length === 1;
  const anyFile = list.some((d) => d.status === 'completed' && !(isTorrent(d) && d.stream));
  const r = await ask({
    title: one ? 'Delete download?' : `Delete ${list.length} downloads?`,
    text: one ? `“${list[0].fileName}” will be removed from the list.` : 'The selected downloads will be removed from the list.',
    ok: 'Delete',
    danger: true,
    checkbox: { label: anyFile ? 'Also delete downloaded files from disk' : 'Also delete partial data', checked: !anyFile },
  });
  if (!r.ok) return;
  for (const d of list) {
    if (isTorrent(d)) await call('torrent_remove', { id: d.id, deleteFiles: r.checked });
    else await call('remove_download', { id: d.id, deleteFile: r.checked });
  }
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
  toast(text.startsWith('magnet:') ? 'Magnet link copied to clipboard' : 'Link copied to clipboard');
}

async function restart(d) {
  if (d.downloaded > 0 && d.status !== 'completed') {
    if (!(await ask({ title: 'Restart download?', text: 'Downloaded progress will be discarded.', ok: 'Restart' }))) return;
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
  if (kb != null) await call('set_download_speed_limit', { id: d.id, limit: kb > 0 ? Math.round(kb * 1024) : null });
}

async function keepTorrent(t) {
  await call('torrent_keep', { id: t.id });
  toast(`Moved to ${state.settings.torrentDir || state.settings.downloadDir}`, 'success');
}

function torrentMenuEntries(t) {
  const entries = [];
  const media = bestMedia(t);
  if (media) entries.push({ icon: 'play', label: t.status === 'completed' ? 'Play' : 'Stream (play while downloading)', action: () => openPlayer(t, media.index) });
  if (t.status === 'completed') entries.push({ icon: 'external', label: 'Open', action: () => openItem(t) });
  entries.push({ icon: 'folder', label: 'Open Folder', action: () => revealItem(t) });
  if (canResume(t)) entries.push({ icon: 'play', label: t.status === 'failed' ? 'Retry' : 'Resume', action: () => resumeItem(t) });
  if (t.status === 'completed' && !t.seeding && !state.torrentStatus?.info?.uploadDisabled) entries.push({ icon: 'upload', label: 'Start Seeding', action: () => resumeItem(t) });
  if (t.seeding) entries.push({ icon: 'pause', label: 'Stop Seeding', action: () => pauseItem(t) });
  else if (canPause(t)) entries.push({ icon: 'pause', label: 'Pause', action: () => pauseItem(t) });
  entries.push('-');
  if (t.stream) entries.push({ icon: 'folderDown', label: 'Keep (move to download folder)', action: () => keepTorrent(t) });
  entries.push({ icon: 'copy', label: 'Copy Magnet Link', action: () => copyText(t.url) });
  entries.push({ icon: 'panelRight', label: 'Properties', action: () => focusRow(t.id, true) });
  entries.push('-');
  entries.push({ icon: 'trash', label: 'Delete', danger: true, action: () => deleteDownloads([t]) });
  return entries;
}

function menuEntries(d) {
  const multi = state.selected.size > 1;
  if (!multi && isTorrent(d)) return torrentMenuEntries(d);
  const list = multi ? selectedDownloads() : [d];
  const entries = [];
  if (!multi && d.status === 'completed') {
    entries.push({ icon: 'external', label: 'Open', action: () => openItem(d) });
  }
  if (!multi) entries.push({ icon: 'folder', label: 'Open Folder', action: () => revealItem(d) });
  if (list.some(canResume)) entries.push({ icon: 'play', label: multi ? 'Resume Selected' : d.status === 'failed' ? 'Retry Download' : 'Resume Download', action: () => list.filter(canResume).forEach(resumeItem) });
  if (list.some(canPause)) entries.push({ icon: 'pause', label: multi ? 'Pause Selected' : 'Pause Download', action: () => list.filter(canPause).forEach(pauseItem) });
  if (!multi) {
    entries.push('-');
    entries.push({ icon: 'copy', label: 'Copy Address', action: () => copyText(d.url) });
    if (d.status !== 'completed') entries.push({ icon: 'gauge', label: 'Speed Limit…', action: () => promptDownloadLimit(d) });
    if (!ACTIVE.has(d.status)) entries.push({ icon: 'refresh', label: 'Redownload', action: () => restart(d) });
    entries.push({ icon: 'panelRight', label: 'Properties', action: () => focusRow(d.id, true) });
  }
  entries.push('-');
  entries.push({ icon: 'trash', label: multi ? `Delete ${list.length} Items` : 'Delete', danger: true, action: () => deleteDownloads(list) });
  return entries;
}

/* ---------- Table rows ---------- */

function createRow(d) {
  const el = document.createElement('div');
  el.className = 'row';
  el.dataset.id = d.id;
  el.setAttribute('role', 'row');
  el.innerHTML = `
    <span class="col-check"><button class="check" aria-label="Select"></button></span>
    <div class="cell-name">
      <div class="ftile"></div>
      <div class="name-text"><strong></strong><span></span></div>
    </div>
    <div class="cell cell-size"></div>
    <div class="cell-status">
      <div class="status-line"></div>
      <div class="bar"><i></i></div>
    </div>
    <div class="cell cell-speed col-speed"></div>
    <div class="cell cell-eta col-eta"></div>
    <div class="cell cell-added col-added"></div>`;
  el.refs = {
    check: $('.check', el),
    tile: $('.ftile', el),
    name: $('.name-text strong', el),
    sub: $('.name-text span', el),
    size: $('.cell-size', el),
    status: $('.status-line', el),
    bar: $('.bar', el),
    fill: $('.bar > i', el),
    speed: $('.cell-speed', el),
    eta: $('.cell-eta', el),
    added: $('.cell-added', el),
  };
  el.refs.check.addEventListener('click', (e) => {
    e.stopPropagation();
    toggleSelect(d.id);
  });
  el.addEventListener('mousedown', (e) => {
    if (e.button !== 0 || e.target.closest('.check')) return;
    clickRow(d.id, e);
  });
  el.addEventListener('dblclick', () => primaryAction(state.downloads.get(d.id)));
  el.addEventListener('contextmenu', (e) => {
    e.preventDefault();
    if (!state.selected.has(d.id)) focusRow(d.id, false);
    openMenu(menuEntries(state.downloads.get(d.id)), e.clientX, e.clientY);
  });
  state.rows.set(d.id, el);
  return el;
}

function updateRow(el, d) {
  const r = el.refs;
  if (r.name.textContent !== d.fileName) {
    r.name.textContent = d.fileName;
    el.title = d.fileName;
    setTile(r.tile, d.fileName, isTorrent(d));
  }
  r.sub.textContent = isTorrent(d)
    ? `${d.stream ? 'Stream' : 'Torrent'} · ${d.files.filter((f) => f.included).length} of ${d.files.length} file${d.files.length === 1 ? '' : 's'}${d.peers ? ` · ${d.peers} peer${d.peers === 1 ? '' : 's'}` : ''}${d.source === 'browser' ? ' · from browser' : ''}`
    : `${CATEGORIES[category(d.fileName)].label}${host(d.url) ? ` · ${host(d.url)}` : ''}${d.source === 'browser' ? ' · from browser' : ''}`;
  r.size.textContent = d.totalSize != null ? fmtBytes(d.totalSize) : d.downloaded ? fmtBytes(d.downloaded) : '—';

  const pct = Math.floor(progress(d) * 100);
  const live = d.status === 'downloading';
  let line;
  switch (d.status) {
    case 'completed':
      line = d.seeding
        ? `<span class="status-seeding">${icon('upload')}</span><span class="status-seeding">Seeding</span><span class="up">${fmtSpeed(d.uploadSpeed)}</span>`
        : `<span class="status-completed">${icon('checkCircle')}</span><span class="status-completed">Complete</span>`;
      break;
    case 'failed':
      line = `<span class="status-failed">${icon('alert')}</span><span class="status-failed" title="${escapeHtml(d.error)}">Failed${d.totalSize ? ` · ${pct}%` : ''}</span>`;
      break;
    case 'paused':
      line = `<span class="status-paused">Paused</span>${d.totalSize ? `<b>${pct}%</b>` : ''}`;
      break;
    case 'downloading':
      line = `<span>Downloading</span>${d.totalSize ? `<b>${pct}%</b>` : `<b>${fmtBytes(d.downloaded)}</b>`}`;
      break;
    default:
      line = `<span class="status-${d.status}">${statusLabel(d)}${d.status === 'merging' || isTorrent(d) ? '…' : ''}</span>`;
  }
  if (r.status.innerHTML !== line) r.status.innerHTML = line;
  r.bar.hidden = d.status === 'completed';
  r.bar.className = `bar${d.status === 'paused' || d.status === 'queued' ? ' paused' : ''}${d.status === 'failed' ? ' failed' : ''}${live && !d.totalSize ? ' indeterminate' : ''}`;
  r.fill.style.width = `${pct}%`;
  el.classList.toggle('live', live);

  r.speed.textContent = live ? fmtSpeed(d.speed) : '—';
  r.speed.classList.toggle('live', live);
  r.eta.textContent = d.status === 'completed' ? '0 sec' : live ? fmtEta(d.eta) : '—';
  r.added.textContent = fmtDate(d.completedAt || d.createdAt);

  const sel = state.selected.has(d.id);
  el.classList.toggle('selected', sel);
  r.check.classList.toggle('on', sel);
  r.check.innerHTML = sel ? icon('check') : '';
}

/* ---------- Selection ---------- */

function visibleDownloads() {
  const f = FILTERS[state.filter].match;
  const q = state.search.trim().toLowerCase();
  const { key, dir } = state.sort;
  const val = {
    name: (d) => d.fileName.toLowerCase(),
    size: (d) => d.totalSize ?? d.downloaded ?? 0,
    status: (d) => `${Object.keys(STATUS_LABEL).indexOf(d.status)}${(1 - progress(d)).toFixed(4)}`,
    speed: (d) => (d.status === 'downloading' ? d.speed : -1),
    eta: (d) => (d.eta == null ? Infinity : d.eta),
    added: (d) => d.createdAt,
  }[key];
  return [...state.downloads.values()]
    .filter(f)
    .filter((d) => !q || d.fileName.toLowerCase().includes(q) || d.url.toLowerCase().includes(q))
    .sort((a, b) => {
      const x = val(a);
      const y = val(b);
      return (x < y ? -1 : x > y ? 1 : b.createdAt - a.createdAt) * dir;
    });
}

function clickRow(id, e) {
  if (e.shiftKey && state.anchor) {
    const ids = visibleDownloads().map((d) => d.id);
    const [a, b] = [ids.indexOf(state.anchor), ids.indexOf(id)].sort((x, y) => x - y);
    if (a >= 0 && b >= 0) {
      if (!(e.ctrlKey || e.metaKey)) state.selected.clear();
      ids.slice(a, b + 1).forEach((x) => state.selected.add(x));
      state.focus = id;
      return refreshSelection();
    }
  }
  if (e.ctrlKey || e.metaKey) return toggleSelect(id);
  focusRow(id, true);
}

function toggleSelect(id) {
  if (state.selected.has(id)) state.selected.delete(id);
  else state.selected.add(id);
  state.anchor = id;
  state.focus = state.selected.size ? (state.selected.has(id) ? id : [...state.selected].pop()) : null;
  refreshSelection();
}

function focusRow(id, openDrawer) {
  state.selected = new Set([id]);
  state.anchor = id;
  state.focus = id;
  if (openDrawer) state.drawerOpen = true;
  refreshSelection();
}

function clearSelection() {
  state.selected.clear();
  state.focus = null;
  refreshSelection();
}

function refreshSelection() {
  for (const [id, el] of state.rows) {
    const d = state.downloads.get(id);
    if (d && el.isConnected) updateRow(el, d);
  }
  updateToolbar();
  renderDrawer();
}

function updateToolbar() {
  const sel = selectedDownloads();
  $('#tool-resume').disabled = !sel.some(canResume);
  $('#tool-pause').disabled = !sel.some(canPause);
  $('#tool-delete').disabled = sel.length === 0;
  $('#tool-stop-all').disabled = ![...state.downloads.values()].some(canPause);
  $('#tool-clear').disabled = ![...state.downloads.values()].some((d) => d.status === 'completed' && !isTorrent(d));

  const visible = visibleDownloads();
  const n = visible.filter((d) => state.selected.has(d.id)).length;
  const all = $('#check-all');
  all.className = `check${n && n === visible.length ? ' on' : n ? ' partial' : ''}`;
  all.innerHTML = n && n === visible.length ? icon('check') : '';
}

/* ---------- Drawer ---------- */

function renderDrawer() {
  const d = state.drawerOpen && state.focus && state.selected.size === 1 ? state.downloads.get(state.focus) : null;
  const drawer = $('#drawer');
  drawer.hidden = !d;
  if (!d) return;

  if (drawer.dataset.id !== d.id) {
    drawer.dataset.id = d.id;
    setTile($('#d-tile'), d.fileName, isTorrent(d));
    $('#d-conns').innerHTML = '';
    $('#d-segmap').innerHTML = '';
    $('#d-files').innerHTML = '';
  }
  $('#d-name').textContent = d.fileName;
  $('#d-name').title = d.fileName;
  $('#d-host').textContent = isTorrent(d) ? (d.stream ? 'Torrent stream' : 'Torrent') : host(d.url) || '—';

  const pct = progress(d);
  $('#d-pct').textContent = `${Math.floor(pct * 100)}%`;
  const tag = $('#d-status');
  tag.textContent = statusLabel(d);
  tag.className = `status-tag ${d.status}`;
  renderSegmap($('#d-segmap'), d);
  if (isTorrent(d)) return renderTorrentDrawer(d);
  $('#d-conn-card').hidden = false;
  $('#d-files-section').hidden = true;
  $('#d-play').hidden = true;
  $('#d-folder').classList.remove('icon-only');
  $('#d-toggle').title = '';
  $('#d-resume-label').textContent = 'Resume';
  $('#d-progress-sub').textContent =
    d.status === 'completed'
      ? `Finished ${fmtDate(d.completedAt, true)}`
      : `${fmtBytes(d.downloaded)} of ${d.totalSize != null ? fmtBytes(d.totalSize) : 'unknown size'}${d.activeConnections ? ` · ${d.activeConnections} active connection${d.activeConnections > 1 ? 's' : ''}` : ''}`;

  $('#d-size').textContent = d.totalSize != null ? fmtBytes(d.totalSize) : 'Unknown';
  $('#d-eta').textContent = d.status === 'completed' ? 'Done' : d.status === 'downloading' ? fmtEta(d.eta) : '—';
  $('#d-speed').textContent = d.status === 'downloading' ? fmtSpeed(d.speed) : d.speedLimit ? `≤ ${fmtSpeed(d.speedLimit)}` : '0 B/s';

  const toggle = $('#d-toggle');
  const [ic, label] = canPause(d) ? ['pause', 'Pause'] : d.status === 'completed' ? ['external', 'Open file'] : ['play', d.status === 'failed' ? 'Retry' : 'Resume'];
  if (toggle.dataset.kind !== ic) {
    toggle.dataset.kind = ic;
    toggle.innerHTML = `${icon(ic)}<span>${label}</span>`;
  }

  const done = d.status === 'completed';
  const conn = $('#d-conn');
  const shown = d.resumable || !d.totalSize ? d.connections : 1;
  if (Number(conn.dataset.value) !== shown) renderConnPicker(conn, shown, (n) => call('set_download_connections', { id: state.focus, connections: n }));
  conn.classList.toggle('disabled', done);
  $('#d-conn-out').textContent = shown;
  const limit = $('#d-limit');
  if (document.activeElement !== limit) limit.value = d.speedLimit ? Math.round(d.speedLimit / 1024) : '';
  limit.disabled = done;

  const segs = d.segments || [];
  $('#d-conns-section').hidden = done || segs.length === 0;
  $('#d-conns-count').textContent = segs.length ? `${segs.filter((s) => s.active).length}/${segs.length}` : '';
  const list = $('#d-conns');
  while (list.children.length > segs.length) list.lastChild.remove();
  segs.forEach((s, i) => {
    let row = list.children[i];
    if (!row) {
      row = document.createElement('div');
      row.className = 'conn-row';
      row.innerHTML = '<span class="num"></span><div class="bar"><i></i></div><span class="pct"></span>';
      list.append(row);
    }
    const p = s.len ? Math.min(1, s.downloaded / s.len) : s.len === 0 ? 1 : 0;
    const finished = s.merged || (s.len != null && s.downloaded >= s.len);
    row.className = `conn-row${s.active ? ' active' : ''}${finished ? ' done' : ''}`;
    row.children[0].textContent = i + 1;
    row.children[1].firstChild.style.width = `${p * 100}%`;
    row.children[2].textContent = finished ? 'Done' : `${Math.floor(p * 100)}%`;
    row.title = `${fmtBytes(s.start)} – ${s.len != null ? fmtBytes(s.start + s.len) : '?'}`;
  });

  $('#d-url').textContent = d.url;
  $('#d-path').textContent = d.finalPath || d.saveDir;
  $('#d-resume').textContent = d.totalSize == null && !d.segments.length ? '—' : d.resumable ? 'Yes — segmented' : 'No — single connection';
  $('#d-added').textContent = fmtDate(d.createdAt, true);
  const err = $('#d-error');
  err.hidden = !d.error;
  if (d.error) err.innerHTML = `${icon('alert')}<span>${escapeHtml(d.error)}</span>`;
}

function renderSegmap(map, d) {
  const done = d.status === 'completed';
  map.classList.toggle('done', done);
  const total = d.totalSize || 0;
  let segs = done || !total ? [{ start: 0, len: 1, downloaded: done ? 1 : 0, merged: false, active: false }] : d.segments;
  if (isTorrent(d) && !done && total) {
    // One block per selected file, laid out like the segments of a download.
    let start = 0;
    segs = d.files
      .filter((f) => f.included)
      .map((f) => {
        const seg = { start, len: f.size, downloaded: f.downloaded, merged: false, active: d.status === 'downloading' && f.downloaded < f.size };
        start += f.size;
        return seg;
      });
  }
  const size = done || !total ? 1 : total;
  while (map.children.length > segs.length) map.lastChild.remove();
  while (map.children.length < segs.length) {
    const s = document.createElement('div');
    s.className = 'seg';
    s.innerHTML = '<i></i>';
    map.append(s);
  }
  segs.forEach((s, i) => {
    const el = map.children[i];
    const len = s.len ?? 0;
    el.style.left = `${(s.start / size) * 100}%`;
    el.style.width = `${(len / size) * 100}%`;
    el.firstChild.style.width = `${len ? Math.min(100, (s.downloaded / len) * 100) : 100}%`;
    el.classList.toggle('active', !!s.active && d.status === 'downloading');
    el.classList.toggle('merged', !!s.merged);
  });
}

const FILE_ICON = { video: 'video', audio: 'music', subtitle: 'captions', other: 'file' };
const KIND_ORDER = ['video', 'audio', 'subtitle', 'other'];

function renderTorrentDrawer(t) {
  $('#d-conn-card').hidden = true;
  $('#d-conns-section').hidden = true;
  const included = t.files.filter((f) => f.included);
  $('#d-progress-sub').textContent =
    t.status === 'completed'
      ? `${t.seeding ? 'Seeding' : 'Finished'}${t.completedAt ? ` · ${fmtDate(t.completedAt, true)}` : ''}`
      : `${fmtBytes(t.downloaded)} of ${t.totalSize != null ? fmtBytes(t.totalSize) : 'unknown size'}${t.peers ? ` · ${t.peers} peer${t.peers === 1 ? '' : 's'}` : ''}`;
  $('#d-size').textContent = t.totalSize != null ? fmtBytes(t.totalSize) : 'Unknown';
  $('#d-eta').textContent = t.status === 'completed' ? 'Done' : t.status === 'downloading' ? fmtEta(t.eta) : '—';
  $('#d-speed').textContent = fmtSpeed(t.status === 'downloading' ? t.speed : 0);

  const toggle = $('#d-toggle');
  const [ic, label] = t.seeding ? ['pause', 'Stop'] : canPause(t) ? ['pause', 'Pause'] : t.status === 'completed' ? ['external', 'Open'] : ['play', t.status === 'failed' ? 'Retry' : 'Resume'];
  if (toggle.dataset.kind !== `${ic}${label}`) {
    toggle.dataset.kind = `${ic}${label}`;
    toggle.innerHTML = `${icon(ic)}<span>${label}</span>`;
  }
  toggle.title = t.seeding ? 'Stop seeding' : '';
  const media = bestMedia(t);
  $('#d-play').hidden = !media;
  $('#d-play').lastChild.textContent = t.status === 'completed' ? 'Play' : 'Stream';
  $('#d-folder').classList.toggle('icon-only', !!media);

  $('#d-files-section').hidden = false;
  $('#d-files-count').textContent = `${included.length}/${t.files.length}`;
  const list = $('#d-files');
  const files = [...t.files].sort((a, b) => b.included - a.included || KIND_ORDER.indexOf(a.kind) - KIND_ORDER.indexOf(b.kind) || a.path.localeCompare(b.path));
  while (list.children.length > files.length) list.lastChild.remove();
  files.forEach((f, i) => {
    let row = list.children[i];
    if (!row) {
      row = document.createElement('div');
      row.className = 'tfile';
      row.innerHTML = `<span class="kind"></span><div class="tfile-name"><strong></strong><span></span></div><div class="tfile-actions"></div><div class="bar"><i></i></div>`;
      list.append(row);
    }
    const p = f.size ? Math.min(1, f.downloaded / f.size) : 1;
    row.className = `tfile${f.included ? '' : ' excluded'}${f.included && p >= 1 ? ' done' : ''}`;
    row.title = f.path;
    const kind = row.children[0];
    kind.className = `kind ${f.kind}`;
    if (kind.dataset.kind !== f.kind) {
      kind.dataset.kind = f.kind;
      kind.innerHTML = icon(FILE_ICON[f.kind]);
    }
    row.children[1].children[0].textContent = f.name;
    row.children[1].children[1].textContent = `${fmtBytes(f.size)}${f.included ? ` · ${Math.floor(p * 100)}%` : ' · skipped'}`;
    row.querySelector('.bar > i').style.width = `${p * 100}%`;
    const actions = row.children[2];
    const key = `${f.index}:${f.included}:${p >= 1}`;
    if (actions.dataset.key !== key) {
      actions.dataset.key = key;
      actions.innerHTML = '';
      const add = (name, title, cls, fn) => {
        const b = document.createElement('button');
        b.type = 'button';
        b.className = `icon-btn ${cls}`;
        b.title = title;
        b.innerHTML = icon(name);
        b.addEventListener('click', (e) => {
          e.stopPropagation();
          fn();
        });
        actions.append(b);
      };
      if (isMedia(f)) add('play', p >= 1 ? 'Play' : 'Stream', 'play', () => openPlayer(state.downloads.get(t.id), f.index));
      if (f.included && p >= 1) add('external', 'Open', '', () => call('torrent_open', { id: t.id, file: f.index }));
      add(f.included ? 'checkCircle' : 'plus', f.included ? 'Skip this file' : 'Download this file', f.included ? 'on' : '', () => toggleTorrentFile(t.id, f.index));
    }
  });

  $('#d-url').textContent = t.url;
  $('#d-path').textContent = t.finalPath || t.saveDir;
  $('#d-resume-label').textContent = 'Transfer';
  $('#d-resume').textContent = `${t.peers} peer${t.peers === 1 ? '' : 's'} · ↑ ${fmtSpeed(t.uploadSpeed)} · ${fmtBytes(t.uploaded)} uploaded`;
  $('#d-added').textContent = fmtDate(t.createdAt, true);
  const err = $('#d-error');
  err.hidden = !t.error;
  if (t.error) err.innerHTML = `${icon('alert')}<span>${escapeHtml(t.error)}</span>`;
}

async function toggleTorrentFile(id, index) {
  const t = state.downloads.get(id);
  if (!t) return;
  const files = new Set(t.files.filter((f) => f.included).map((f) => f.index));
  if (files.has(index)) files.delete(index);
  else files.add(index);
  if (!files.size) return toast('At least one file must stay selected', 'error');
  await call('torrent_set_files', { id, files: [...files] });
}

/* ---------- List rendering ---------- */

function renderList() {
  const list = $('#list');
  const items = visibleDownloads();
  const keep = new Set(items.map((d) => d.id));
  for (const el of [...list.children]) if (!keep.has(el.dataset.id)) el.remove();
  items.forEach((d, i) => {
    const el = state.rows.get(d.id) || createRow(d);
    updateRow(el, d);
    if (list.children[i] !== el) list.insertBefore(el, list.children[i] || null);
  });

  const all = [...state.downloads.values()];
  for (const key of Object.keys(FILTERS)) {
    const badge = $(`[data-count="${key}"]`);
    if (badge) {
      const n = all.filter(FILTERS[key].match).length;
      badge.textContent = n || '';
    }
  }
  $('#view-title').textContent = FILTERS[state.filter].title;
  $('#view-count').textContent = items.length;

  const empty = items.length === 0;
  $('#empty').hidden = !empty;
  list.hidden = empty;
  if (empty) {
    const filtered = state.search || state.filter !== 'all';
    $('#empty-title').textContent = filtered ? 'Nothing to show here' : 'No downloads yet';
    $('#empty-text').hidden = !!filtered;
  }
  $$('.th').forEach((th) => th.classList.toggle('sorted', th.dataset.sort === state.sort.key));
  updateToolbar();
  renderDrawer();
  renderStatusbar();
}

function renderStatusbar() {
  const all = [...state.downloads.values()];
  const active = all.filter((d) => ACTIVE.has(d.status) && !isTorrent(d));
  const queued = all.filter((d) => d.status === 'queued').length;
  const torrents = all.filter((d) => isTorrent(d) && ACTIVE.has(d.status)).length;
  const seeding = all.filter((d) => isTorrent(d) && d.seeding).length;
  const speed = all.filter((d) => ACTIVE.has(d.status)).reduce((s, d) => s + (d.speed || 0), 0);
  const parts = [];
  if (active.length) parts.push(`${active.length} active${queued ? ` · ${queued} queued` : ''} of ${state.settings?.maxConcurrent ?? 3} slots`);
  else if (queued) parts.push(`${queued} queued`);
  if (torrents) parts.push(`${torrents} torrent${torrents === 1 ? '' : 's'}`);
  if (seeding) parts.push(`${seeding} seeding`);
  $('#sb-speed').textContent = fmtSpeed(speed);
  $('#sb-active').textContent = parts.join(' · ') || 'Idle';
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
  const y = (v) => h - 2 - (v / max) * (h - 6);
  ctx.beginPath();
  ctx.moveTo(0, y(data[0]));
  for (let i = 1; i < data.length; i++) {
    const xm = (i - 0.5) * step;
    ctx.bezierCurveTo(xm, y(data[i - 1]), xm, y(data[i]), i * step, y(data[i]));
  }
  ctx.strokeStyle = '#7c5cf6';
  ctx.lineWidth = 2;
  ctx.stroke();
  ctx.lineTo(w, h);
  ctx.lineTo(0, h);
  ctx.closePath();
  ctx.fillStyle = 'rgba(124, 92, 246, 0.14)';
  ctx.fill();
}

/* ---------- Disk ---------- */

async function refreshDisk() {
  const dir = state.settings?.downloadDir;
  if (!dir) return;
  const parts = dir.split(/[\\/]/).filter(Boolean);
  $('#disk-path').textContent = parts.length > 2 ? `…/${parts.slice(-2).join('/')}` : dir;
  $('#disk-path').title = dir;
  try {
    const { total, free } = await invoke('disk_space', { path: dir });
    const used = total ? (total - free) / total : 0;
    $('#disk-pct').textContent = `${Math.round(used * 100)}%`;
    $('#gauge-fill').style.strokeDasharray = `${used * 100} 100`;
    $('#disk-free').textContent = `${fmtBytes(free)} free of ${fmtBytes(total)}`;
  } catch {
    $('#disk-free').textContent = 'Unavailable';
  }
}

/* ---------- Events ---------- */

function upsert(d) {
  const prev = state.downloads.get(d.id);
  state.downloads.set(d.id, d);
  return !prev || prev.status !== d.status || prev.fileName !== d.fileName || prev.seeding !== d.seeding || prev.stream !== d.stream;
}

function removeItem(id) {
  state.downloads.delete(id);
  state.rows.get(id)?.remove();
  state.rows.delete(id);
  state.selected.delete(id);
  if (state.focus === id) state.focus = null;
  renderList();
}

function onProgress(views) {
  let structural = false;
  for (const d of views) {
    if (upsert(d)) structural = true;
    else {
      const el = state.rows.get(d.id);
      if (el?.isConnected) updateRow(el, d);
    }
  }
  if (structural || ['speed', 'eta', 'status'].includes(state.sort.key)) renderList();
  else {
    if (views.some((d) => d.id === state.focus)) renderDrawer();
    renderStatusbar();
  }
}

/* ---------- Theme & global limit ---------- */

function effectiveTheme() {
  const t = state.settings?.theme || 'dark';
  if (t !== 'system') return t;
  return matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
}

function applyTheme() {
  document.documentElement.dataset.theme = state.settings?.theme || 'dark';
  const btn = $('#tool-theme');
  btn.innerHTML = `${icon(effectiveTheme() === 'dark' ? 'sun' : 'moon')}<span>${effectiveTheme() === 'dark' ? 'Light' : 'Dark'}</span>`;
}

async function saveSettings(patch) {
  state.settings = await call('save_settings', { settings: { ...state.settings, ...patch } });
  applyTheme();
  renderGlobalLimit();
  renderStatusbar();
  refreshDisk();
  return state.settings;
}

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
  let bytes = Number(select.value);
  if (select.value === 'custom') {
    const kb = await ask({
      title: 'Global speed limit',
      text: 'Maximum combined speed of all downloads. Enter 0 for unlimited.',
      ok: 'Apply',
      input: { value: state.settings.globalSpeedLimit ? Math.round(state.settings.globalSpeedLimit / 1024) : '', placeholder: 'Unlimited', suffix: 'KB/s' },
    });
    if (kb == null) return renderGlobalLimit();
    bytes = kb > 0 ? Math.round(kb * 1024) : 0;
  }
  await call('set_global_speed_limit', { limit: bytes || null });
  state.settings.globalSpeedLimit = bytes || null;
  renderGlobalLimit();
  toast(bytes ? `Speed limited to ${fmtSpeed(bytes)}` : 'Speed limit removed');
}

function renderBridge() {
  const b = state.bridge;
  if (!b) return;
  const dot = $('#bridge-dot');
  const text = $('#bridge-text');
  if (b.error) {
    dot.className = 'dot err';
    text.textContent = 'Browser: port busy';
  } else if (!b.listening) {
    dot.className = 'dot';
    text.textContent = 'Browser: starting';
  } else if (b.clients > 0) {
    dot.className = 'dot ok';
    text.textContent = `Browser connected${b.clients > 1 ? ` (${b.clients})` : ''}`;
  } else {
    dot.className = 'dot idle';
    text.textContent = 'Browser not connected';
  }
  const status = $('#set-bridge-status');
  status.innerHTML = `<span class="${dot.className}"></span><span>${escapeHtml(
    b.error || (b.listening ? `Listening on ws://127.0.0.1:${b.port} — ${b.clients} extension connection${b.clients === 1 ? '' : 's'}` : 'Starting…'),
  )}</span>`;
}

/* ---------- Add dialog ---------- */

const addState = { headers: [], nameEdited: false, probeSeq: 0, probeTimer: null, source: null };

function openAddDialog({ url = '', fileName = '', headers = [], fileSize = null, source = null } = {}) {
  const dlg = $('#dlg-add');
  Object.assign(addState, { headers, nameEdited: !!fileName, source });
  $('#add-url').value = url;
  $('#add-name').value = fileName || '';
  $('#add-dir').value = state.settings?.downloadDir || '';
  const conns = state.settings?.defaultConnections ?? 8;
  renderConnPicker($('#add-conn'), conns, (n) => ($('#add-conn-out').textContent = n));
  $('#add-conn-out').textContent = conns;
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
    setTile($('#probe-tile'), '');
  } else if (kind === 'error') {
    $('#probe-title').textContent = info;
    meta.innerHTML = '<span>You can still try to download it.</span>';
  } else if (kind === 'ok') {
    $('#probe-title').textContent = info.fileName;
    setTile($('#probe-tile'), info.fileName);
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
  if (!isUrl(url)) return setProbe('idle');
  const seq = ++addState.probeSeq;
  setProbe('loading');
  try {
    const info = await invoke('probe_url', { url, headers: addState.headers });
    if (seq !== addState.probeSeq) return;
    if (!addState.nameEdited) $('#add-name').value = info.fileName;
    setProbe('ok', info);
  } catch (e) {
    if (seq === addState.probeSeq) setProbe('error', String(e));
  }
}

async function submitAdd(e) {
  e.preventDefault();
  const url = $('#add-url').value.trim();
  if (isTorrentLink(url)) {
    $('#dlg-add').close('cancel');
    return openTorrentDialog({ link: url, headers: addState.headers, source: addState.source });
  }
  if (!isUrl(url)) {
    toast('Please enter a valid http(s) link', 'error');
    return $('#add-url').focus();
  }
  const kb = Number($('#add-limit').value || 0);
  const request = {
    url,
    fileName: $('#add-name').value.trim() || null,
    saveDir: $('#add-dir').value.trim() || null,
    connections: Number($('#add-conn').dataset.value),
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
    $('#quick-url').value = '';
    if (!FILTERS[state.filter].match(d)) selectFilter('all');
    upsert(d);
    renderList();
    focusRow(d.id, true);
  } finally {
    btn.disabled = false;
  }
}

/* ---------- Add torrent dialog ---------- */

const torState = { seq: 0, open: null, preview: null, selected: new Set(), queue: [], timer: null };

/** `open`: { link } (magnet, info-hash or .torrent URL) or { path } (.torrent file), plus optional headers/source. */
function openTorrentDialog(open = {}) {
  const dlg = $('#dlg-torrent');
  if (dlg.open) {
    if (open.link || open.path) torState.queue.push(open);
    return;
  }
  torState.open = { headers: [], ...open };
  torState.preview = null;
  $('#tor-link').value = open.path || open.link || '';
  $('#tor-source').textContent = open.source === 'browser' ? 'Captured from your browser' : open.path ? 'Torrent file' : 'Magnet link, info-hash, .torrent link or file';
  $('#tor-dir').value = state.settings?.torrentDir || state.settings?.downloadDir || '';
  $('#tor-start').checked = true;
  setTorProbe('idle');
  renderTorFiles();
  dlg.showModal();
  if (open.link || open.path) inspectTorrent();
  else $('#tor-link').focus();
}

function setTorProbe(kind, info) {
  const card = $('#tor-probe');
  card.dataset.state = kind;
  const meta = $('#tor-meta');
  setTile($('#tor-tile'), '', true);
  if (kind === 'loading') {
    $('#tor-title').textContent = 'Fetching torrent information…';
    meta.innerHTML = `<span>${torState.open?.path ? 'Reading the file' : 'Asking peers for the file list. Magnet links can take up to a minute.'}</span>`;
  } else if (kind === 'error') {
    $('#tor-title').textContent = info;
    meta.innerHTML = '';
  } else if (kind === 'ok') {
    $('#tor-title').textContent = info.name;
    const videos = info.files.filter((f) => f.kind === 'video').length;
    const tags = [`<span class="tag">${fmtBytes(info.totalSize)}</span>`, `<span class="tag">${info.files.length} file${info.files.length === 1 ? '' : 's'}</span>`];
    if (videos) tags.push(`<span class="tag">${videos} video${videos === 1 ? '' : 's'}</span>`);
    if (info.existing) tags.push('<span class="tag warn">Already in your list</span>');
    meta.innerHTML = tags.join('');
  }
}

async function inspectTorrent() {
  const open = torState.open;
  if (!open?.path && !(open?.link || '').trim()) return setTorProbe('idle');
  const seq = ++torState.seq;
  torState.preview = null;
  renderTorFiles();
  setTorProbe('loading');
  try {
    const preview = await invoke('torrent_inspect', { open });
    if (seq !== torState.seq) return;
    torState.preview = preview;
    torState.selected = new Set(preview.files.filter((f) => f.included).map((f) => f.index));
    setTorProbe('ok', preview);
    renderTorFiles();
  } catch (e) {
    if (seq === torState.seq) setTorProbe('error', String(e));
  }
}

function renderTorFiles() {
  const p = torState.preview;
  $('#tor-files-field').hidden = !p || p.files.length < 2;
  const sel = torState.selected;
  const selected = p ? p.files.filter((f) => sel.has(f.index)) : [];
  $('#tor-download').disabled = !p || !selected.length;
  $('#tor-stream').disabled = !p || !p.files.some(isMedia);
  $('#tor-dir-field').hidden = !!p?.existing;
  if (!p) return;
  $('#tor-selected').textContent = `${selected.length} of ${p.files.length} · ${fmtBytes(selected.reduce((n, f) => n + f.size, 0))}`;
  const list = $('#tor-files');
  list.innerHTML = '';
  const files = [...p.files].sort((a, b) => KIND_ORDER.indexOf(a.kind) - KIND_ORDER.indexOf(b.kind) || a.path.localeCompare(b.path));
  for (const f of files) {
    const row = document.createElement('label');
    row.className = 'tfile-opt';
    row.title = f.path;
    const on = sel.has(f.index);
    const dir = f.path.includes('/') ? f.path.slice(0, f.path.lastIndexOf('/')) : '';
    row.innerHTML = `<span class="check${on ? ' on' : ''}">${on ? icon('check') : ''}</span><span class="kind ${f.kind}">${icon(FILE_ICON[f.kind])}</span><span class="tfile-name"><strong>${escapeHtml(f.name)}</strong>${dir ? `<span>${escapeHtml(dir)}</span>` : ''}</span><span class="tfile-size">${fmtBytes(f.size)}</span>`;
    row.addEventListener('click', (e) => {
      e.preventDefault();
      if (sel.has(f.index)) sel.delete(f.index);
      else sel.add(f.index);
      renderTorFiles();
    });
    list.append(row);
  }
}

function selectTorFiles(pick) {
  const p = torState.preview;
  if (!p) return;
  torState.selected = new Set(p.files.filter(pick).map((f) => f.index));
  renderTorFiles();
}

async function addTorrent(mode) {
  const p = torState.preview;
  if (!p) return;
  let files = [...torState.selected];
  let play = null;
  if (mode === 'stream') {
    play = bestMedia({ files: p.files.map((f) => ({ ...f, included: torState.selected.has(f.index) })) });
    if (!play) return toast('This torrent has no video or audio files', 'error');
    files = [play.index];
  } else if (!files.length) {
    return toast('Select at least one file', 'error');
  }
  const request = {
    id: p.id,
    mode,
    files,
    saveDir: mode === 'download' ? $('#tor-dir').value.trim() || null : null,
    start: mode === 'stream' || $('#tor-start').checked,
    source: torState.open?.source || null,
  };
  const buttons = [$('#tor-download'), $('#tor-stream')];
  buttons.forEach((b) => (b.disabled = true));
  try {
    const t = await call('torrent_add', { request });
    $('#dlg-torrent').close('ok');
    $('#quick-url').value = '';
    if (p.existing && mode === 'download') toast('This torrent is already in your list');
    if (!FILTERS[state.filter].match(t)) selectFilter('all');
    upsert(t);
    renderList();
    focusRow(t.id, true);
    if (play) openPlayer(t, play.index);
  } finally {
    renderTorFiles();
  }
}

async function takeTorrentOpens() {
  const opens = await invoke('take_torrent_opens');
  opens.forEach(openTorrentDialog);
}

/* ---------- Stream player ---------- */

const player = { id: null, file: null, url: null, seq: 0, subUrl: null };

function stopVideo() {
  const video = $('#pl-video');
  video.pause();
  video.removeAttribute('src');
  video.load();
  clearSubtitles();
}

function clearSubtitles() {
  $$('#pl-video track').forEach((t) => t.remove());
  if (player.subUrl) URL.revokeObjectURL(player.subUrl);
  player.subUrl = null;
}

async function openPlayer(t, file) {
  if (!t) return;
  const seq = ++player.seq;
  Object.assign(player, { id: t.id, file, url: null });
  stopVideo();
  const f = t.files.find((x) => x.index === file);
  $('#pl-title').textContent = f?.name || t.fileName;
  $('#pl-title').title = f?.path || t.fileName;
  $('#pl-error').hidden = true;
  setPlayerLoading('Fetching the first pieces…');
  renderPlayerSelects(t);
  const dlg = $('#dlg-player');
  if (!dlg.open) dlg.showModal();
  renderPlayerStats();
  try {
    const url = await invoke('torrent_stream_url', { id: t.id, file });
    if (seq !== player.seq) return;
    player.url = url;
    const video = $('#pl-video');
    video.src = url;
    video.play().catch(() => {});
    const sub = matchingSubtitle(t, f);
    if (sub) {
      $('#pl-subs').value = String(sub.index);
      loadSubtitle(sub.index);
    }
  } catch (e) {
    if (seq === player.seq) showPlayerError(String(e));
  }
}

function setPlayerLoading(text) {
  $('#pl-loading').hidden = !text;
  if (text) $('#pl-loading span').textContent = text;
}

function showPlayerError(text = null) {
  setPlayerLoading(null);
  $('#pl-error').hidden = false;
  $('#pl-error-text').textContent =
    text || 'Its video or audio format is not supported here. Open it in an external player such as VLC or mpv — the torrent keeps streaming while it plays.';
}

const subtitleFiles = (t) => t.files.filter((f) => /\.(srt|vtt)$/i.test(f.name));
const baseName = (name) => name.replace(/\.[^.]+$/, '').toLowerCase();

/** A subtitle named like the video (movie.srt, movie.en.srt …). */
function matchingSubtitle(t, video) {
  if (!video) return null;
  const base = baseName(video.name);
  return subtitleFiles(t).find((s) => baseName(s.name) === base || baseName(s.name).startsWith(`${base}.`)) || null;
}

function renderPlayerSelects(t) {
  const files = $('#pl-file');
  const media = t.files.filter(isMedia);
  files.innerHTML = media.map((f) => `<option value="${f.index}">${escapeHtml(f.name)}</option>`).join('');
  files.value = String(player.file);
  files.parentElement.hidden = media.length < 2;
  const subs = $('#pl-subs');
  const list = subtitleFiles(t);
  subs.innerHTML = `<option value="">No subtitles</option>${list.map((f) => `<option value="${f.index}">${escapeHtml(f.name)}</option>`).join('')}`;
  subs.parentElement.hidden = !list.length;
}

async function loadSubtitle(index) {
  clearSubtitles();
  if (index === '' || index == null) return;
  const id = player.id;
  const seq = player.seq;
  try {
    const vtt = await invoke('torrent_subtitle', { id, file: Number(index) });
    if (seq !== player.seq) return;
    player.subUrl = URL.createObjectURL(new Blob([vtt], { type: 'text/vtt' }));
    const track = document.createElement('track');
    track.kind = 'subtitles';
    track.label = 'Subtitles';
    track.srclang = 'und';
    track.src = player.subUrl;
    track.default = true;
    $('#pl-video').append(track);
    track.track.mode = 'showing';
  } catch (e) {
    toast(`Cannot load subtitles: ${e}`, 'error');
  }
}

function renderPlayerStats() {
  if (!$('#dlg-player').open || !player.id) return;
  const t = state.downloads.get(player.id);
  if (!t) return;
  const f = t.files.find((x) => x.index === player.file);
  const done = f && f.size && f.downloaded >= f.size;
  $('#pl-stats').textContent = done
    ? `Downloaded · ${fmtBytes(f.size)}${t.seeding ? ` · seeding ↑ ${fmtSpeed(t.uploadSpeed)}` : ''}`
    : `${f ? `${Math.floor((f.downloaded / (f.size || 1)) * 100)}% of ${fmtBytes(f.size)}` : '—'} · ↓ ${fmtSpeed(t.speed)} · ${t.peers} peer${t.peers === 1 ? '' : 's'}`;
}

async function playExternal() {
  if (!player.id) return;
  const url = player.url || (await call('torrent_stream_url', { id: player.id, file: player.file }));
  await call('play_external', { url, title: $('#pl-title').textContent });
  $('#pl-video').pause();
}

function bindPlayer() {
  const dlg = $('#dlg-player');
  const video = $('#pl-video');
  dlg.addEventListener('close', () => {
    player.seq++;
    player.id = null;
    stopVideo();
  });
  video.addEventListener('playing', () => setPlayerLoading(null));
  video.addEventListener('canplay', () => setPlayerLoading(null));
  video.addEventListener('waiting', () => player.url && setPlayerLoading('Buffering…'));
  video.addEventListener('error', () => {
    if (player.url && video.getAttribute('src')) showPlayerError();
  });
  $('#pl-file').addEventListener('change', (e) => openPlayer(state.downloads.get(player.id), Number(e.target.value)));
  $('#pl-subs').addEventListener('change', (e) => loadSubtitle(e.target.value));
  $('#pl-external').addEventListener('click', playExternal);
  $('#pl-error-external').addEventListener('click', playExternal);
  $('#pl-copy').addEventListener('click', () => player.url && copyText(player.url));
  dlg.addEventListener('keydown', (e) => {
    if (e.target.closest('select, button')) return;
    if (e.key === 'ArrowRight' || e.key === 'ArrowLeft') {
      e.preventDefault();
      video.currentTime = Math.max(0, video.currentTime + (e.key === 'ArrowRight' ? 10 : -10));
    } else if (e.key.toLowerCase() === 'f') {
      if (document.fullscreenElement) document.exitFullscreen();
      else video.requestFullscreen?.();
    } else if (e.key === ' ' && e.target !== video) {
      e.preventDefault();
      if (video.paused) video.play().catch(() => {});
      else video.pause();
    }
  });
}

/* ---------- Settings dialog ---------- */

function openSettings() {
  const s = state.settings;
  $('#set-dir').value = s.downloadDir;
  renderConnPicker($('#set-conn'), s.defaultConnections, (n) => ($('#set-conn-out').textContent = n));
  $('#set-conn-out').textContent = s.defaultConnections;
  $('#set-concurrent').value = s.maxConcurrent;
  $('#set-limit').value = s.globalSpeedLimit ? Math.round(s.globalSpeedLimit / 1024) : '';
  $('#set-port').value = s.bridgePort;
  $('#set-autostart').checked = s.autoStartBrowser;
  $('#set-tray').checked = s.closeToTray;
  $('#set-notify').checked = s.notifyOnComplete;
  $$('#set-theme button').forEach((b) => b.classList.toggle('active', b.dataset.value === s.theme));
  $('#set-tdir').value = s.torrentDir || '';
  $('#set-sdir').value = s.streamDir || '';
  $('#set-sdir').placeholder = state.torrentStatus?.streamDir || 'App cache folder';
  $('#set-tdown').value = s.torrentDownloadLimit ? Math.round(s.torrentDownloadLimit / 1024) : '';
  $('#set-tup').value = s.torrentUploadLimit ? Math.round(s.torrentUploadLimit / 1024) : '';
  $('#set-tport').value = s.torrentPort || 0;
  $('#set-player').value = s.externalPlayer || '';
  selectUploadMode(s.torrentUploadMode);
  renderTorrentStatus();
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
  if (port < 1024 || port > 65535) return toast('Port must be between 1024 and 65535', 'error');
  const tport = Number($('#set-tport').value || 0);
  if (tport !== 0 && (tport < 1024 || tport > 65535)) return toast('Torrent port must be 0 or between 1024 and 65535', 'error');
  const before = { ...state.settings };
  await saveSettings({
    downloadDir: $('#set-dir').value.trim() || state.settings.downloadDir,
    defaultConnections: Number($('#set-conn').dataset.value),
    maxConcurrent: Math.max(1, Math.min(16, Number($('#set-concurrent').value || 3))),
    globalSpeedLimit: kb > 0 ? Math.round(kb * 1024) : null,
    bridgePort: port,
    autoStartBrowser: $('#set-autostart').checked,
    closeToTray: $('#set-tray').checked,
    notifyOnComplete: $('#set-notify').checked,
    theme: $('#set-theme button.active')?.dataset.value || 'dark',
    torrentDir: $('#set-tdir').value.trim(),
    streamDir: $('#set-sdir').value.trim(),
    torrentDownloadLimit: kbToBytes($('#set-tdown').value),
    torrentUploadLimit: kbToBytes($('#set-tup').value),
    torrentPort: tport,
    torrentUploadMode: $('#set-upmode button.active')?.dataset.value || 'normal',
    externalPlayer: $('#set-player').value.trim(),
  });
  $('#dlg-settings').close('ok');
  const restart = before.streamDir !== state.settings.streamDir || before.torrentPort !== state.settings.torrentPort || (before.torrentUploadMode === 'disabled') !== (state.settings.torrentUploadMode === 'disabled');
  toast(restart ? 'Options saved — some torrent options apply after restarting the app' : 'Options saved', 'success', restart ? 6000 : 3800);
}

const kbToBytes = (v) => (Number(v || 0) > 0 ? Math.round(Number(v) * 1024) : null);

const UPLOAD_HELP = {
  normal: 'Finished torrents keep uploading to other peers until you stop them.',
  stopWhenDone: 'Torrents pause as soon as they are complete. Finished files can still be played.',
  disabled: 'Nothing is ever sent to other peers; downloads can be slower. Applies after a restart.',
};

function selectUploadMode(mode) {
  $$('#set-upmode button').forEach((b) => b.classList.toggle('active', b.dataset.value === mode));
  $('#set-upmode-help').textContent = UPLOAD_HELP[mode] || '';
}

function renderTorrentStatus() {
  const t = state.torrentStatus;
  const el = $('#set-torrent-status');
  if (!t) return;
  const [cls, text] = t.error
    ? ['dot err', `Torrent engine failed to start: ${t.error}`]
    : t.ready
      ? ['dot ok', `Torrent engine running${t.info.listenPort ? ` — listening on port ${t.info.listenPort}` : ''}${t.info.uploadDisabled ? ' · upload disabled' : ''}`]
      : ['dot', 'Torrent engine starting…'];
  el.innerHTML = `<span class="${cls}"></span><span>${escapeHtml(text)}</span>`;
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

  $$('.nav-item').forEach((b) => b.addEventListener('click', () => selectFilter(b.dataset.filter)));
  $('#search').addEventListener('input', (e) => {
    state.search = e.target.value;
    renderList();
  });
  $$('.th').forEach((th) =>
    th.addEventListener('click', () => {
      const key = th.dataset.sort;
      state.sort = { key, dir: state.sort.key === key ? -state.sort.dir : key === 'name' ? 1 : -1 };
      renderList();
    }),
  );
  $('#check-all').addEventListener('click', () => {
    const visible = visibleDownloads();
    const allOn = visible.length && visible.every((d) => state.selected.has(d.id));
    state.selected = allOn ? new Set() : new Set(visible.map((d) => d.id));
    state.focus = allOn ? null : state.focus;
    refreshSelection();
  });
  $('#list').addEventListener('mousedown', (e) => {
    if (e.target === $('#list')) clearSelection();
  });

  // Toolbar
  $('#quick-add').addEventListener('submit', (e) => {
    e.preventDefault();
    const url = $('#quick-url').value.trim();
    if (isTorrentLink(url)) return openTorrentDialog({ link: url });
    if (url && !isUrl(url)) return toast('That does not look like an http(s) or magnet link', 'error');
    openAddDialog({ url });
  });
  $('#tool-torrent').addEventListener('click', () => openTorrentDialog());
  $('#tool-resume').addEventListener('click', () => selectedDownloads().filter(canResume).forEach(resumeItem));
  $('#tool-pause').addEventListener('click', () => selectedDownloads().filter(canPause).forEach(pauseItem));
  $('#tool-stop-all').addEventListener('click', () => call('pause_all'));
  $('#tool-delete').addEventListener('click', () => deleteDownloads(selectedDownloads()));
  $('#tool-clear').addEventListener('click', () => call('clear_completed'));
  $('#tool-settings').addEventListener('click', openSettings);
  $('#tool-theme').addEventListener('click', () => saveSettings({ theme: effectiveTheme() === 'dark' ? 'light' : 'dark' }));

  // Drawer
  $('#d-close').addEventListener('click', () => {
    state.drawerOpen = false;
    renderDrawer();
  });
  $('#d-toggle').addEventListener('click', () => primaryAction(state.downloads.get(state.focus)));
  $('#d-play').addEventListener('click', () => {
    const t = state.downloads.get(state.focus);
    const media = t && bestMedia(t);
    if (media) openPlayer(t, media.index);
  });
  $('#d-folder').addEventListener('click', () => revealItem(state.downloads.get(state.focus)));
  $('#d-more').addEventListener('click', (e) => {
    e.stopPropagation();
    const r = e.currentTarget.getBoundingClientRect();
    openMenu(menuEntries(state.downloads.get(state.focus)), r.right - 220, r.bottom + 6);
  });
  $('#d-limit').addEventListener('change', (e) => {
    const kb = Number(e.target.value || 0);
    call('set_download_speed_limit', { id: state.focus, limit: kb > 0 ? Math.round(kb * 1024) : null });
  });

  // Status bar & sidebar
  $('#global-limit').addEventListener('change', onGlobalLimitChange);
  $('#sb-bridge').addEventListener('click', openSettings);
  $('#disk-card').addEventListener('click', () => state.settings && call('open_folder', { path: state.settings.downloadDir }));

  // Add dialog
  $('#add-form').addEventListener('submit', submitAdd);
  $('#add-url').addEventListener('input', () => {
    addState.headers = [];
    scheduleProbe();
  });
  $('#add-name').addEventListener('input', () => (addState.nameEdited = $('#add-name').value.trim() !== ''));
  $('#add-browse').addEventListener('click', async () => {
    const dir = await call('pick_folder', { current: $('#add-dir').value });
    if (dir) $('#add-dir').value = dir;
  });

  // Torrent dialog & player
  $('#torrent-form').addEventListener('submit', (e) => {
    e.preventDefault();
    addTorrent('download');
  });
  $('#tor-stream').addEventListener('click', () => addTorrent('stream'));
  $('#tor-link').addEventListener('input', () => {
    torState.open = { link: $('#tor-link').value.trim(), headers: [], source: null };
    clearTimeout(torState.timer);
    torState.timer = setTimeout(inspectTorrent, 500);
  });
  $('#tor-file').addEventListener('click', async () => {
    const path = await call('pick_torrent_file');
    if (!path) return;
    torState.open = { path, headers: [], source: null };
    $('#tor-link').value = path;
    inspectTorrent();
  });
  $('#tor-browse').addEventListener('click', async () => {
    const dir = await call('pick_folder', { current: $('#tor-dir').value });
    if (dir) $('#tor-dir').value = dir;
  });
  $('#tor-all').addEventListener('click', () => selectTorFiles(() => true));
  $('#tor-none').addEventListener('click', () => selectTorFiles(() => false));
  $('#tor-videos').addEventListener('click', () => selectTorFiles((f) => f.kind === 'video' || f.kind === 'subtitle'));
  $('#dlg-torrent').addEventListener('close', () => {
    torState.seq++;
    clearTimeout(torState.timer);
    if (torState.queue.length) setTimeout(() => openTorrentDialog(torState.queue.shift()), 150);
  });
  bindPlayer();

  // Settings dialog
  $('#settings-form').addEventListener('submit', submitSettings);
  for (const [button, input, cmd] of [
    ['#set-tbrowse', '#set-tdir', 'pick_folder'],
    ['#set-sbrowse', '#set-sdir', 'pick_folder'],
    ['#set-pbrowse', '#set-player', 'pick_player'],
  ]) {
    $(button).addEventListener('click', async () => {
      const path = await call(cmd, cmd === 'pick_folder' ? { current: $(input).value || $(input).placeholder } : undefined);
      if (path) $(input).value = path;
    });
  }
  $$('#set-upmode button').forEach((b) => b.addEventListener('click', () => selectUploadMode(b.dataset.value)));
  $('#set-magnet').addEventListener('click', async () => toast(await call('register_magnet_handler'), 'success', 7000));
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

  // Global
  document.addEventListener('mousedown', (e) => {
    if (!e.target.closest('#menu')) closeMenu();
  });
  window.addEventListener('blur', closeMenu);
  window.addEventListener('resize', closeMenu);
  $('#list').addEventListener('scroll', closeMenu, { passive: true });
  document.addEventListener('keydown', onKey);
  document.addEventListener('paste', (e) => {
    if (e.target.closest('input, textarea') || $('dialog[open]')) return;
    const text = e.clipboardData?.getData('text')?.trim();
    if (isTorrentLink(text)) openTorrentDialog({ link: text });
    else if (isUrl(text)) openAddDialog({ url: text });
  });
  matchMedia('(prefers-color-scheme: light)').addEventListener('change', applyTheme);
}

function onKey(e) {
  const mod = e.ctrlKey || e.metaKey;
  const typing = e.target.closest('input, textarea, select') || $('dialog[open]');
  if (e.key === 'Escape') {
    if (!$('#menu').hidden) return closeMenu();
    if (!typing) clearSelection();
  }
  if (mod && e.key.toLowerCase() === 'n') {
    e.preventDefault();
    return openAddDialog();
  }
  if (mod && e.key.toLowerCase() === 't' && !$('dialog[open]')) {
    e.preventDefault();
    return openTorrentDialog();
  }
  if (mod && e.key.toLowerCase() === 'f') {
    e.preventDefault();
    return $('#search').focus();
  }
  if (typing) return;
  if (mod && e.key.toLowerCase() === 'a') {
    e.preventDefault();
    state.selected = new Set(visibleDownloads().map((d) => d.id));
    return refreshSelection();
  }
  if (e.key === 'Delete') deleteDownloads(selectedDownloads());
  if (e.key === 'Enter' && state.focus) primaryAction(state.downloads.get(state.focus));
  if (e.key === ' ' && state.selected.size) {
    e.preventDefault();
    selectedDownloads().forEach((d) => primaryAction(d.status === 'completed' ? null : d));
  }
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    const ids = visibleDownloads().map((d) => d.id);
    if (!ids.length) return;
    const i = ids.indexOf(state.focus);
    const next = ids[Math.max(0, Math.min(ids.length - 1, i < 0 ? 0 : i + (e.key === 'ArrowDown' ? 1 : -1)))];
    focusRow(next, state.drawerOpen);
    state.rows.get(next)?.scrollIntoView({ block: 'nearest' });
  }
}

async function init() {
  bindUi();
  const [settings, downloads, bridge, appInfo, torrents, torrentStatus] = await Promise.all([
    invoke('get_settings'),
    invoke('list_downloads'),
    invoke('bridge_status'),
    invoke('app_info'),
    invoke('torrent_list'),
    invoke('torrent_status'),
  ]);
  Object.assign(state, { settings, bridge, appInfo, torrentStatus });
  $('#app-version').textContent = appInfo.version;
  downloads.forEach(upsert);
  torrents.forEach(upsert);
  applyTheme();
  renderGlobalLimit();
  renderBridge();
  renderList();
  refreshDisk();

  await listen('download://updated', (e) => {
    upsert(e.payload);
    renderList();
  });
  await listen('download://progress', (e) => onProgress(e.payload));
  await listen('download://removed', (e) => removeItem(e.payload));
  await listen('download://completed', (e) => {
    toast(`Download complete: ${e.payload.fileName}`, 'success');
    refreshDisk();
  });
  await listen('bridge://status', (e) => {
    state.bridge = e.payload;
    renderBridge();
  });
  await listen('bridge://download-request', (e) => {
    const p = e.payload;
    openAddDialog({ url: p.url, fileName: p.fileName || '', headers: p.headers || [], fileSize: p.fileSize, source: 'browser' });
  });
  await listen('bridge://captured', (e) => toast(`Captured from browser: ${e.payload.fileName}`, 'success'));

  await listen('torrent://updated', (e) => {
    upsert(e.payload);
    renderList();
    renderPlayerStats();
  });
  await listen('torrent://progress', (e) => {
    onProgress(e.payload);
    renderPlayerStats();
  });
  await listen('torrent://removed', (e) => {
    if (player.id === e.payload) $('#dlg-player').close();
    removeItem(e.payload);
  });
  await listen('torrent://completed', (e) => {
    if (!e.payload.stream) toast(`Torrent complete: ${e.payload.fileName}`, 'success');
    refreshDisk();
  });
  await listen('torrent://status', (e) => {
    state.torrentStatus = e.payload;
    renderTorrentStatus();
    if (e.payload.error) toast(`Torrent engine failed to start: ${e.payload.error}`, 'error', 10000);
  });
  await listen('torrent://open-request', takeTorrentOpens);
  await listen('tauri://drag-drop', (e) => {
    const paths = (e.payload?.paths || []).filter((p) => /\.torrent$/i.test(p));
    paths.forEach((path) => openTorrentDialog({ path }));
  });
  takeTorrentOpens();

  setInterval(() => {
    state.speedHistory.push(renderStatusbar());
    state.speedHistory.shift();
    drawSparkline();
  }, 1000);
  setInterval(refreshDisk, 30000);
  drawSparkline();
}

init().catch((e) => {
  console.error(e);
  toast(`Failed to start: ${e}`, 'error', 10000);
});
