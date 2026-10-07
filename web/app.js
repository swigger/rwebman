'use strict';
const $ = (id) => document.getElementById(id);
const state = { csrf: null, view: 'search', kind: '', offset: 0, hasMore: false, query: '', searched: false, searchVersion: 0, controller: null, status: null, settingsLoaded: false, term: null, fit: null, socket: null, terminalGeneration: 0, polling: false };
const typeNames = { file: '文件', directory: '文件夹', symlink: '链接' };
const number = new Intl.NumberFormat('zh-CN');
let toastTimer, searchTimer, terminalFitFrame, terminalFitSettledFrame;
let terminalFollowingOutput = true, terminalResizing = false;

function icon(name, className = '') {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  const use = document.createElementNS('http://www.w3.org/2000/svg', 'use');
  use.setAttribute('href', `#i-${name}`); svg.append(use);
  if (className) svg.setAttribute('class', className);
  svg.setAttribute('aria-hidden', 'true'); return svg;
}
function toast(message) {
  $('toast').textContent = message; $('toast').hidden = false;
  clearTimeout(toastTimer); toastTimer = setTimeout(() => { $('toast').hidden = true; }, 4000);
}
async function api(path, options = {}) {
  const headers = { ...(options.body ? { 'Content-Type': 'application/json' } : {}), ...(state.csrf ? { 'X-CSRF-Token': state.csrf } : {}), ...options.headers };
  const response = await fetch(path, { ...options, headers, credentials: 'same-origin', cache: 'no-store' });
  const payload = await response.json().catch(() => ({}));
  if (!response.ok) {
    if (response.status === 401 && path !== '/api/login') lock();
    throw new Error(payload.error || `请求失败 (${response.status})`);
  }
  return payload;
}
function lock() {
  state.csrf = null; state.settingsLoaded = false; state.status = null;
  state.searchVersion++; state.controller?.abort(); clearTimeout(searchTimer);
  disconnectTerminal(true);
  document.body.classList.remove('terminal-view');
  $('workspace').hidden = true; $('login-screen').hidden = false;
  $('results').replaceChildren(); $('query').value = ''; $('roots').value = ''; $('excludes').value = '';
  state.searched = false; state.offset = 0; state.query = '';
  $('password').value = ''; $('password').type = 'password'; $('toggle-password').textContent = '显示';
  $('password').focus();
}
async function unlock(csrf) {
  state.csrf = csrf; $('password').value = ''; $('login-error').textContent = '';
  $('login-screen').hidden = true; $('workspace').hidden = false;
  $('machine-host').textContent = location.host;
  showView('search'); showEmpty('你要找的，就在这台电脑上。', '输入文件名开始查找，支持中文与部分名称匹配。');
  $('result-count').textContent = ''; $('search-timing').textContent = '按文件名排序';
  await pollStatus(); $('query').focus();
}
$('login-form').addEventListener('submit', async (event) => {
  event.preventDefault(); const button = event.currentTarget.querySelector('[type=submit]');
  button.disabled = true; $('login-error').textContent = '';
  try { const result = await api('/api/login', { method: 'POST', body: JSON.stringify({ password: $('password').value }) }); await unlock(result.csrf); }
  catch (error) { $('login-error').textContent = error.message; }
  finally { button.disabled = false; }
});
$('toggle-password').addEventListener('click', () => {
  const reveal = $('password').type === 'password'; $('password').type = reveal ? 'text' : 'password';
  $('toggle-password').textContent = reveal ? '隐藏' : '显示'; $('toggle-password').setAttribute('aria-label', reveal ? '隐藏密码' : '显示密码');
});
$('logout').addEventListener('click', async () => {
  try { await api('/api/logout', { method: 'POST' }); lock(); }
  catch (error) { toast(error.message); }
});

function showView(name) {
  if (!state.csrf) return;
  state.view = name;
  document.body.classList.toggle('terminal-view', name === 'terminal');
  for (const view of ['search', 'terminal', 'settings']) $(`view-${view}`).hidden = view !== name;
  document.querySelectorAll('.nav-item[data-view]').forEach(button => {
    button.classList.toggle('active', button.dataset.view === name);
    if (button.dataset.view === name) button.setAttribute('aria-current', 'page'); else button.removeAttribute('aria-current');
  });
  $('view-title').textContent = { search: '文件查找', terminal: '本地终端', settings: '索引设置' }[name];
  if (name === 'settings' && !state.settingsLoaded) loadSettings();
  if (name === 'terminal' && state.term) { scheduleTerminalFit(); state.term.focus(); }
}
document.querySelectorAll('[data-view]').forEach(button => button.addEventListener('click', () => showView(button.dataset.view)));
document.addEventListener('keydown', (event) => {
  if (!state.csrf || state.view === 'terminal' || event.altKey) return;
  const editing = ['INPUT', 'TEXTAREA'].includes(document.activeElement?.tagName);
  if ((event.key === '/' && !editing) || ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'k')) {
    event.preventDefault(); showView('search'); $('query').focus(); $('query').select();
  }
});

async function pollStatus() {
  if (!state.csrf || state.polling) return;
  state.polling = true;
  try {
    const status = await api('/api/status'); if (!state.csrf) return;
    const previous = state.status; state.status = status;
    const progress = status.index;
    $('machine-os').textContent = { windows: 'Windows 设备', macos: 'macOS 设备', linux: 'Linux 设备' }[status.os] || status.os;
    $('stat-count').textContent = number.format(progress.indexed_count);
    $('stat-roots').textContent = String(status.roots_count).padStart(2, '0');
    $('stat-updated').textContent = progress.last_completed ? formatDate(progress.last_completed, false) : '尚未建立';
    $('stat-status').textContent = progress.running ? '正在建立新的索引快照' : progress.last_completed ? '索引已就绪' : '等待首次索引';
    $('index-banner').hidden = !progress.running;
    $('reindex').disabled = progress.running;
    $('index-progress').textContent = `正在扫描 · 已发现 ${number.format(progress.visited)} 个条目 · ${progress.elapsed_seconds} 秒`;
    $('index-path').textContent = progress.current_path || '正在准备索引…';
    $('index-error').hidden = !progress.error && !progress.skipped;
    $('index-error').textContent = progress.error || `上次扫描跳过了 ${number.format(progress.skipped)} 个无法访问或已消失的条目。`;
    $('shell-name').textContent = status.shell.split(/[\\/]/).pop();
    if (!status.auto_index) $('interval-help').textContent = '当前进程通过 --no-auto-index 禁用了自动扫描。保存设置与手动更新仍然可用。';
    if (previous?.index.running && !progress.running && !progress.error && state.searched) runSearch(false);
  } catch (error) { if (state.csrf) { $('stat-status').textContent = '连接暂时中断'; } }
  finally { state.polling = false; }
}
setInterval(() => { if (!document.hidden) pollStatus(); }, 3000);
document.addEventListener('visibilitychange', () => { if (!document.hidden) pollStatus(); });
$('reindex').addEventListener('click', async () => {
  $('reindex').disabled = true;
  try { await api('/api/index', { method: 'POST' }); toast('已开始更新索引'); await pollStatus(); }
  catch (error) { toast(error.message); $('reindex').disabled = false; }
});
$('cancel-index').addEventListener('click', async () => {
  try { await api('/api/index/cancel', { method: 'POST' }); toast('正在取消扫描，将保留原有索引'); }
  catch (error) { toast(error.message); }
});

function formatDate(timestamp, year = true) {
  return new Date(timestamp * 1000).toLocaleString('zh-CN', { ...(year ? { year: 'numeric' } : {}), month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false });
}
function size(bytes) {
  if (bytes == null) return '—';
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB', 'TB']; let value = bytes / 1024, i = 0;
  while (value >= 1024 && i < units.length - 1) { value /= 1024; i++; }
  return `${value.toFixed(value < 10 ? 1 : 0)} ${units[i]}`;
}
function showEmpty(title, description) {
  $('empty-state').hidden = false; $('results-table-wrap').hidden = true; $('pagination').hidden = true;
  $('empty-title').textContent = title; $('empty-description').textContent = description;
}
async function copyPath(path) {
  try {
    if (navigator.clipboard && window.isSecureContext) await navigator.clipboard.writeText(path);
    else {
      const field = document.createElement('textarea'); field.value = path; field.className = 'sr-only'; document.body.append(field);
      field.select(); const ok = document.execCommand('copy'); field.remove(); if (!ok) throw new Error('当前浏览器不支持复制');
    }
    toast('路径已复制');
  } catch (error) { toast(error.message || '复制失败，请手动选择路径'); }
}
function renderEntries(entries) {
  const fragment = document.createDocumentFragment();
  for (const entry of entries) {
    const row = document.createElement('tr'), cell = document.createElement('td');
    const file = document.createElement('div'); file.className = 'file-cell';
    const tile = document.createElement('span'); tile.className = `file-icon ${entry.kind === 'directory' ? 'folder' : ''}`;
    tile.append(icon(entry.kind === 'directory' ? 'folder' : 'file'));
    const meta = document.createElement('div'); meta.className = 'file-meta';
    const name = document.createElement('div'); name.className = 'file-name'; name.textContent = entry.name; name.title = entry.name;
    const path = document.createElement('div'); path.className = 'file-path'; path.textContent = entry.path; path.title = entry.path;
    meta.append(name, path); file.append(tile, meta); cell.append(file); row.append(cell);
    for (const value of [typeNames[entry.kind], size(entry.size), entry.modified ? formatDate(entry.modified) : '—']) {
      const td = document.createElement('td'); td.textContent = value; row.append(td);
    }
    const actions = document.createElement('td'), copy = document.createElement('button');
    copy.className = 'copy-button'; copy.title = '复制完整路径'; copy.setAttribute('aria-label', `复制 ${entry.name} 的路径`);
    copy.append(icon('copy')); copy.addEventListener('click', () => copyPath(entry.path)); actions.append(copy); row.append(actions);
    fragment.append(row);
  }
  $('results').replaceChildren(fragment);
}
async function runSearch(reset = true) {
  if (!state.csrf) return;
  if (reset) state.offset = 0;
  state.query = $('query').value.trim(); state.searched = true;
  const version = ++state.searchVersion;
  state.controller?.abort(); state.controller = new AbortController();
  $('search-timing').textContent = '正在查找…';
  $('next').disabled = true; $('previous').disabled = true;
  const query = new URLSearchParams({ q: state.query, path: $('match-path').checked, kind: state.kind, offset: state.offset, limit: 100 });
  try {
    const result = await api(`/api/search?${query}`, { signal: state.controller.signal });
    if (version !== state.searchVersion || !state.csrf) return;
    state.hasMore = result.has_more;
    renderEntries(result.entries);
    $('search-timing').textContent = `${result.elapsed_ms} ms · 按文件名排序`;
    $('result-count').textContent = `${number.format(state.offset + result.entries.length)}${result.has_more ? '+' : ''}`;
    if (!result.entries.length) {
      showEmpty(state.status?.index.indexed_count ? '没有找到匹配的文件。' : '还没有可搜索的索引。', state.status?.index.indexed_count ? '试试更短的名称、其他文件类型，或匹配完整路径。' : '点击「更新索引」开始扫描，也可以在索引设置中选择目录。');
    } else {
      $('empty-state').hidden = true; $('results-table-wrap').hidden = false;
      $('pagination').hidden = state.offset === 0 && !result.has_more;
      $('page-label').textContent = `第 ${number.format(state.offset + 1)}–${number.format(state.offset + result.entries.length)} 条`;
      $('previous').disabled = state.offset === 0; $('next').disabled = !result.has_more;
    }
  } catch (error) {
    if (error.name !== 'AbortError' && version === state.searchVersion && state.csrf) {
      $('search-timing').textContent = '搜索未完成'; showEmpty('暂时无法完成搜索', error.message);
    }
  }
}
$('search-form').addEventListener('submit', (event) => { event.preventDefault(); clearTimeout(searchTimer); runSearch(); });
$('query').addEventListener('input', () => { clearTimeout(searchTimer); state.controller?.abort(); state.searchVersion++; searchTimer = setTimeout(() => runSearch(), 280); });
document.querySelectorAll('[data-kind]').forEach(button => button.addEventListener('click', () => {
  state.kind = button.dataset.kind;
  document.querySelectorAll('[data-kind]').forEach(b => b.classList.toggle('active', b === button));
  runSearch();
}));
$('match-path').addEventListener('change', () => runSearch());
$('previous').addEventListener('click', () => { state.offset = Math.max(0, state.offset - 100); runSearch(false); });
$('next').addEventListener('click', () => { if (state.hasMore) { state.offset += 100; runSearch(false); } });

async function loadSettings() {
  try {
    const settings = await api('/api/settings'); if (!state.csrf) return;
    $('roots').value = settings.roots.join('\n'); $('excludes').value = settings.excludes.join('\n');
    $('interval').value = settings.index_interval_minutes; state.settingsLoaded = true;
  } catch (error) { toast(error.message); }
}
$('settings-form').addEventListener('submit', async (event) => {
  event.preventDefault(); const button = event.currentTarget.querySelector('[type=submit]'); button.disabled = true;
  $('settings-error').textContent = '';
  const lines = id => $(id).value.split(/\r?\n/).map(line => line.trim()).filter(Boolean);
  try {
    await api('/api/settings', { method: 'PUT', body: JSON.stringify({ roots: lines('roots'), excludes: lines('excludes'), index_interval_minutes: Number($('interval').value) }) });
    await loadSettings(); await pollStatus(); toast('设置已保存，正在更新索引');
  } catch (error) { $('settings-error').textContent = error.message; }
  finally { button.disabled = false; }
});

function fitTerminal() {
  const term = state.term;
  if (!term || !state.fit || state.view !== 'terminal' || !state.csrf
      || !$('terminal').clientWidth || !$('terminal').clientHeight) return;
  // Keep the prompt visible on resize without pulling a reader out of scrollback.
  terminalResizing = true;
  state.fit.fit();
  if (terminalFollowingOutput) term.scrollToBottom();
  cancelAnimationFrame(terminalFitSettledFrame);
  terminalFitSettledFrame = requestAnimationFrame(() => {
    if (term === state.term && terminalFollowingOutput) term.scrollToBottom();
    terminalResizing = false;
  });
}
function scheduleTerminalFit() {
  if (!state.term || state.view !== 'terminal') return;
  terminalResizing = true;
  cancelAnimationFrame(terminalFitFrame);
  terminalFitFrame = requestAnimationFrame(fitTerminal);
}
function disconnectTerminal(dispose = false) {
  cancelAnimationFrame(terminalFitFrame); cancelAnimationFrame(terminalFitSettledFrame);
  terminalResizing = false;
  state.terminalGeneration++;
  if (state.socket) { state.socket.onclose = null; state.socket.close(); state.socket = null; }
  $('terminal-status').textContent = '已断开'; $('terminal-disconnect').hidden = true;
  $('terminal-connect').disabled = false; $('terminal-connect').hidden = false; $('terminal-connect').querySelector('span').textContent = '启动终端';
  if (dispose && state.term) { state.term.dispose(); state.term = null; state.fit = null; $('terminal').replaceChildren(); $('terminal').hidden = true; $('terminal-placeholder').hidden = false; }
  else if (state.term) state.term.writeln('\r\n\x1b[90m[会话已断开]\x1b[0m');
}
function sendTerminal(message) {
  if (state.socket?.readyState === WebSocket.OPEN) {
    if (state.socket.bufferedAmount > 256 * 1024) { toast('输入过快，终端连接已关闭'); disconnectTerminal(); return; }
    state.socket.send(JSON.stringify(message));
  }
}
function connectTerminal() {
  if (!state.csrf) return;
  disconnectTerminal(true);
  const generation = ++state.terminalGeneration;
  $('terminal-placeholder').hidden = true; $('terminal').hidden = false;
  const term = new Terminal({ cursorBlink: true, fontFamily: 'Consolas, "SFMono-Regular", Menlo, monospace', fontSize: 13, lineHeight: 1.2, scrollback: 5000, theme: { background: '#17231e', foreground: '#d3deca', cursor: '#bad99e', selectionBackground: '#426344', black: '#22352b', red: '#d59183', green: '#b3d297', yellow: '#d8c28d', blue: '#9abed5', magenta: '#c4a9ca', cyan: '#97c9bf', white: '#e0e7da' } });
  const fit = new FitAddon.FitAddon(); term.loadAddon(fit); term.open($('terminal'));
  state.term = term; state.fit = fit;
  terminalFollowingOutput = true;
  term.onScroll(() => {
    if (!terminalResizing) terminalFollowingOutput = term.buffer.active.viewportY >= term.buffer.active.baseY;
  });
  fitTerminal();
  document.fonts.ready.then(scheduleTerminalFit);
  $('terminal-connect').disabled = true; $('terminal-status').textContent = '正在连接…';
  const socket = new WebSocket(`${location.protocol === 'https:' ? 'wss:' : 'ws:'}//${location.host}/api/terminal`, ['rwebman', `csrf.${state.csrf}`]);
  state.socket = socket; socket.binaryType = 'arraybuffer';
  socket.onopen = () => {
    if (generation !== state.terminalGeneration) { socket.close(); return; }
    $('terminal-status').textContent = '已连接'; $('terminal-disconnect').hidden = false; $('terminal-connect').hidden = true;
    fitTerminal(); sendTerminal({ type: 'resize', cols: term.cols, rows: term.rows }); term.focus();
  };
  socket.onmessage = event => {
    if (generation !== state.terminalGeneration) return;
    if (typeof event.data === 'string') {
      try { const error = JSON.parse(event.data); if (error.type === 'error') toast(error.message); } catch {}
    } else term.write(new Uint8Array(event.data));
  };
  socket.onerror = () => { if (generation === state.terminalGeneration) toast('无法连接终端，请检查登录状态与终端连接数'); };
  socket.onclose = () => { if (generation === state.terminalGeneration) { disconnectTerminal(); pollStatus(); } };
  term.onData(data => {
    terminalFollowingOutput = true;
    term.scrollToBottom();
    // Small chunks keep PTY input bounded, including paste events.
    const chunks = []; let chunk = '', bytes = 0;
    for (const character of data) {
      const length = new TextEncoder().encode(character).length;
      if (bytes + length > 4096) { chunks.push(chunk); chunk = ''; bytes = 0; }
      chunk += character; bytes += length;
    }
    if (chunk) chunks.push(chunk);
    if (chunks.length > 24) { toast('单次粘贴过长，请分段粘贴'); return; }
    chunks.forEach(part => sendTerminal({ type: 'input', data: part }));
  });
  term.onResize(({ cols, rows }) => sendTerminal({ type: 'resize', cols, rows }));
}
$('terminal-connect').addEventListener('click', connectTerminal);
$('terminal-disconnect').addEventListener('click', () => disconnectTerminal());
new ResizeObserver(scheduleTerminalFit).observe($('terminal'));
window.addEventListener('resize', scheduleTerminalFit);
window.visualViewport?.addEventListener('resize', scheduleTerminalFit);
window.addEventListener('pagehide', () => disconnectTerminal(true));
window.addEventListener('pageshow', event => { if (event.persisted) api('/api/session').then(data => unlock(data.csrf)).catch(() => lock()); });
api('/api/session').then(data => unlock(data.csrf)).catch(() => {});
