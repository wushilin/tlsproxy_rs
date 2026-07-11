const API = '/apiserver';

const state = {
  view: 'dashboard',
  listeners: {},
  dns: { exact: [], suffix: [], regex: [] },
  stats: {},
  statuses: {},
  manager: { status: 'STOPPED', uptime_ms: null },
  activeConnections: [],
  activeConnectionsTimer: null,
  updatedAt: null,
  dirty: false,
};

const views = {
  dashboard: ['Overview', 'Live status of the proxy service'],
  listeners: ['Listeners', 'Configure how inbound traffic is accepted and routed'],
  dns: ['DNS overrides', 'Rewrite destination addresses before connecting upstream'],
};

const content = document.querySelector('#content');
const loading = document.querySelector('#loading');
const toasts = document.querySelector('#toasts');
const managerStatus = document.querySelector('#manager-status');
const dirtyBadge = document.querySelector('#dirty-badge');
const saveButton = document.querySelector('#save-button');

initTheme();

document.querySelectorAll('[data-view]').forEach(button => {
  button.addEventListener('click', () => setView(button.dataset.view));
});
document.querySelectorAll('.topbar [data-operation]').forEach(button => {
  button.addEventListener('click', () => serviceOperation(button.dataset.operation));
});
document.querySelector('#reload-button').addEventListener('click', async () => {
  if (state.dirty && !await confirmAction('Discard unsaved changes?',
    'Reloading fetches the configuration from the server. Edits you have not saved will be lost.')) return;
  loadAll();
});
saveButton.addEventListener('click', saveAll);
document.addEventListener('click', event => {
  if (!event.target.closest?.('[data-action-menu], .floating-action-menu')) closeActionMenus();
});
window.addEventListener('resize', closeActionMenus);
window.addEventListener('scroll', closeActionMenus, true);
window.addEventListener('beforeunload', event => {
  if (state.dirty) event.preventDefault();
});

loadAll();
setInterval(() => {
  if (['dashboard', 'listeners'].includes(state.view)) refreshStats();
}, 3000);

/* ---------- Theme ---------- */

function initTheme() {
  const stored = localStorage.getItem('tlsproxy-theme');
  const dark = stored ? stored === 'dark' : matchMedia('(prefers-color-scheme: dark)').matches;
  document.documentElement.dataset.theme = dark ? 'dark' : 'light';
  document.querySelector('#theme-toggle').addEventListener('click', () => {
    const next = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
    document.documentElement.dataset.theme = next;
    localStorage.setItem('tlsproxy-theme', next);
  });
}

/* ---------- Data ---------- */

async function request(path, options = {}) {
  const response = await fetch(`${API}${path}`, {
    headers: { 'Content-Type': 'application/json', ...(options.headers || {}) },
    ...options,
  });
  if (!response.ok) {
    const message = await response.text();
    throw new Error(message || `${response.status} ${response.statusText}`);
  }
  const text = await response.text();
  return text ? JSON.parse(text) : null;
}

async function loadAll() {
  busy(true);
  try {
    const [listeners, dns, stats, statuses, manager] = await Promise.all([
      request('/config/listeners'),
      request('/config/dns'),
      request('/stats/listeners'),
      request('/status/listeners'),
      request('/status/manager'),
    ]);
    state.listeners = normalizeListeners(listeners);
    state.dns = normalizeDns(dns);
    state.stats = stats || {};
    state.statuses = statuses || {};
    state.manager = manager || { status: 'STOPPED', uptime_ms: null };
    state.updatedAt = new Date();
    setDirty(false);
    render();
  } catch (error) {
    notify(`Unable to load configuration: ${error.message}`, true);
  } finally {
    busy(false);
  }
}

function normalizeDns(dns) {
  return {
    exact: Array.isArray(dns?.exact) ? dns.exact : [],
    suffix: Array.isArray(dns?.suffix) ? dns.suffix : [],
    regex: Array.isArray(dns?.regex) ? dns.regex : [],
  };
}

async function refreshStats() {
  try {
    const [stats, statuses, manager] = await Promise.all([
      request('/stats/listeners'),
      request('/status/listeners'),
      request('/status/manager'),
    ]);
    const failureShapeChanged = failureSignature(statuses) !== failureSignature(state.statuses);
    state.stats = stats || {};
    state.statuses = statuses || {};
    state.manager = manager || state.manager;
    state.updatedAt = new Date();
    updateManagerStatus();
    if (state.view === 'dashboard') {
      if (failureShapeChanged) render();
      else refreshDashboardInPlace();
    } else if (state.view === 'listeners') {
      render();
    }
  } catch (_) {
    managerStatus.textContent = 'Unreachable';
    managerStatus.className = 'status-pill danger';
  }
}

function failureSignature(statuses = {}) {
  return Object.entries(statuses).map(([name, s]) => `${name}:${s?.Err ? 1 : 0}`).sort().join('|');
}

function statusOk(status) {
  if (status?.Ok && typeof status.Ok === 'object') return status.Ok;
  if (status?.Ok === true) return { running: true, backends: [] };
  return null;
}

function listenerBackends(name) {
  return statusOk(state.statuses[name])?.backends || [];
}

function normalizeListeners(listeners = {}) {
  return Object.fromEntries(Object.entries(listeners).map(([name, listener]) => [
    name,
    cleanListener({
      mode: listener.mode || 'passthrough',
      bind: listener.bind || '',
      target: listener.target || '',
      target_port: Number(listener.target_port || 443),
      policy: listener.policy || 'DENY',
      max_idle_time_ms: listener.max_idle_time_ms ?? 3600000,
      speed_limit: listener.speed_limit ?? 0,
      upstream_tls: Boolean(listener.upstream_tls),
      rules: {
        static_hosts: listener.rules?.static_hosts || [],
        patterns: listener.rules?.patterns || [],
      },
    }),
  ]));
}

/* ---------- Rendering ---------- */

function setView(view) {
  state.view = view;
  document.querySelectorAll('[data-view]').forEach(button => {
    button.classList.toggle('active', button.dataset.view === view);
  });
  render();
}

function render() {
  const [title, subtitle] = views[state.view];
  document.querySelector('#page-title').textContent = title;
  document.querySelector('#page-subtitle').textContent = subtitle;
  updateManagerStatus();
  content.innerHTML = ({
    dashboard: renderDashboard,
    listeners: renderListeners,
    dns: renderDns,
  })[state.view]();
  bindView();
  content.setAttribute('aria-busy', 'false');
}

function setDirty(dirty) {
  state.dirty = dirty;
  dirtyBadge.hidden = !dirty;
  saveButton.disabled = !dirty;
}

function markDirty() { setDirty(true); }

function updateManagerStatus() {
  const statuses = Object.values(state.statuses);
  const failed = statuses.filter(status => status?.Err).length;
  const running = statuses.filter(status => statusOk(status)?.running).length;
  if (!statuses.length) {
    managerStatus.textContent = 'Stopped';
    managerStatus.className = 'status-pill neutral';
  } else if (failed) {
    managerStatus.textContent = `${failed} listener${failed > 1 ? 's' : ''} failed`;
    managerStatus.className = 'status-pill warning';
  } else if (!running) {
    managerStatus.textContent = 'Stopped';
    managerStatus.className = 'status-pill neutral';
  } else {
    managerStatus.textContent = `${running} running`;
    managerStatus.className = 'status-pill success';
  }
}

function totals() {
  const stats = Object.values(state.stats);
  return {
    active: stats.reduce((sum, item) => sum + item.active, 0),
    total: stats.reduce((sum, item) => sum + item.total, 0),
    uploaded: stats.reduce((sum, item) => sum + item.uploaded_bytes, 0),
    downloaded: stats.reduce((sum, item) => sum + item.downloaded_bytes, 0),
  };
}

function renderDashboard() {
  const t = totals();
  const names = Object.keys(state.listeners).sort((a, b) => a.localeCompare(b));
  const rows = names.map(name => {
    const listener = state.listeners[name];
    const item = state.stats[name] || { total: 0, active: 0, uploaded_bytes: 0, downloaded_bytes: 0 };
    const error = state.statuses[name]?.Err;
    const running = Boolean(statusOk(state.statuses[name])?.running);
    const pill = error
      ? '<span class="status-pill danger">Failed</span>'
      : running
        ? '<span class="status-pill success">Online</span>'
        : '<span class="status-pill neutral">Stopped</span>';
    return `
      <tr>
        <td><strong>${escapeHtml(name)}</strong></td>
        <td><code>${escapeHtml(listener.bind)}</code></td>
        <td><span class="badge">${escapeHtml(listener.mode)}</span></td>
        <td data-pill="${escapeAttr(name)}">${pill}</td>
        <td class="num" data-cell="active:${escapeAttr(name)}">${fmt(item.active)}</td>
        <td class="num" data-cell="total:${escapeAttr(name)}">${fmt(item.total)}</td>
        <td class="num" data-cell="up:${escapeAttr(name)}">${formatBytes(item.uploaded_bytes)}</td>
        <td class="num" data-cell="down:${escapeAttr(name)}">${formatBytes(item.downloaded_bytes)}</td>
      </tr>
      ${error ? `<tr><td colspan="8" class="error-note">↳ ${escapeHtml(error.message || String(error))}</td></tr>` : ''}`;
  }).join('');
  return `
    <div class="metrics">
      ${metric('Active connections', fmt(t.active), 'Open right now', 'active')}
      ${metric('Uptime', formatDuration(state.manager.uptime_ms), state.manager.status || 'Stopped', 'uptime')}
      ${metric('Total connections', fmt(t.total), 'Since the last listener start', 'total')}
      ${metric('Uploaded', formatBytes(t.uploaded), 'Client to upstream', 'up')}
      ${metric('Downloaded', formatBytes(t.downloaded), 'Upstream to client', 'down')}
    </div>
    <section class="panel">
      <div class="panel-header">
        <div class="panel-title"><h2>Listeners</h2></div>
        <span class="badge" id="updated-at">Updated ${state.updatedAt ? state.updatedAt.toLocaleTimeString() : 'just now'}</span>
      </div>
      <div class="table-wrap">
        ${names.length ? `
          <table>
            <thead><tr>
              <th>Name</th><th>Bind</th><th>Mode</th><th>Status</th>
              <th class="num">Active</th><th class="num">Total</th><th class="num">Uploaded</th><th class="num">Downloaded</th>
            </tr></thead>
            <tbody>${rows}</tbody>
          </table>` : `
          <div class="empty"><strong>No listeners configured</strong>Add one from the Listeners page to start proxying traffic.</div>`}
      </div>
    </section>`;
}

function refreshDashboardInPlace() {
  const t = totals();
  setLive('[data-metric="active"]', fmt(t.active));
  setLive('[data-metric="uptime"]', formatDuration(state.manager.uptime_ms));
  setLive('[data-metric="total"]', fmt(t.total));
  setLive('[data-metric="up"]', formatBytes(t.uploaded));
  setLive('[data-metric="down"]', formatBytes(t.downloaded));
  setLive('#updated-at', `Updated ${state.updatedAt.toLocaleTimeString()}`);
  for (const name of Object.keys(state.listeners)) {
    const item = state.stats[name] || { total: 0, active: 0, uploaded_bytes: 0, downloaded_bytes: 0 };
    setLive(`[data-cell="active:${cssEscape(name)}"]`, fmt(item.active));
    setLive(`[data-cell="total:${cssEscape(name)}"]`, fmt(item.total));
    setLive(`[data-cell="up:${cssEscape(name)}"]`, formatBytes(item.uploaded_bytes));
    setLive(`[data-cell="down:${cssEscape(name)}"]`, formatBytes(item.downloaded_bytes));
  }
}

function setLive(selector, text) {
  const node = content.querySelector(selector);
  if (node && node.textContent !== text) node.textContent = text;
}

function renderListeners() {
  const names = Object.keys(state.listeners).sort((a, b) => a.localeCompare(b));
  const rows = names.map(name => {
    const listener = state.listeners[name];
    const status = state.statuses[name];
    const error = status?.Err;
    const running = Boolean(statusOk(status)?.running);
    const statusPill = error
      ? '<span class="status-pill danger">Failed</span>'
      : running
      ? '<span class="status-pill success">Online</span>'
      : '<span class="status-pill neutral">Stopped</span>';
    const upstream = listener.mode === 'forward'
      ? renderForwardUpstream(listener, listenerBackends(name))
      : listener.mode === 'terminate'
      ? (listener.upstream_tls ? '<span class="badge accent">TLS</span>' : '<span class="badge">Plaintext</span>')
      : '<span class="muted">Same TLS stream</span>';
    const targetCount = splitTargets(listener.target).length;
    const target = listener.mode === 'forward'
      ? `<span class="muted">${fmt(targetCount)} backend${targetCount === 1 ? '' : 's'}</span>`
      : `<span class="num">${Number(listener.target_port) || ''}</span>`;
    const policy = listener.mode === 'forward' ? '<span class="muted">n/a</span>' : escapeHtml(listener.policy);
    const hosts = listener.mode === 'forward' ? '<span class="muted">-</span>' : fmt((listener.rules.static_hosts || []).length);
    const regex = listener.mode === 'forward' ? '<span class="muted">-</span>' : fmt((listener.rules.patterns || []).length);
    return `
      <tr>
        <td><strong>${escapeHtml(name)}</strong>${error ? '<div class="error-note">Failed</div>' : ''}</td>
        <td>${statusPill}</td>
        <td><code>${escapeHtml(listener.bind)}</code></td>
        <td>${target}</td>
        <td><span class="badge accent">${escapeHtml(modeLabel(listener.mode))}</span></td>
        <td>${upstream}</td>
        <td>${policy}</td>
        <td class="num">${hosts}</td>
        <td class="num">${regex}</td>
        <td class="row-actions">
          <button class="button small" data-action-menu="${escapeAttr(name)}">Actions</button>
        </td>
      </tr>
      ${error ? `<tr><td colspan="10" class="error-note">↳ ${escapeHtml(error.message || String(error))}</td></tr>` : ''}`;
  }).join('');
  return `
    <section class="panel">
      <div class="panel-header">
        <div class="panel-title">
          <h2>Listeners</h2>
          <p>Inbound endpoints and their routing mode.</p>
        </div>
        <span class="badge" id="listener-updated-at">Updated ${state.updatedAt ? state.updatedAt.toLocaleTimeString() : 'just now'}</span>
        <button class="button primary" data-add-listener>Add listener</button>
      </div>
      <div class="table-wrap">
        ${rows ? `
          <table class="listener-table">
            <thead><tr>
              <th>Name</th><th>Status</th><th>Bind</th><th>Target</th><th>Mode</th>
              <th>Upstream</th><th>Policy</th><th class="num">Hosts</th><th class="num">Regex</th><th></th>
            </tr></thead>
            <tbody>${rows}</tbody>
          </table>` : `
          <div class="empty"><strong>No listeners configured</strong>Add a listener to start proxying traffic.</div>`}
      </div>
    </section>
    <div class="note info">Forward mode accepts plaintext clients and randomly selects an online backend; targets that have not been health-checked yet still count as eligible. Upstream TLS encrypts the proxy-to-upstream leg without authenticating the upstream certificate.</div>`;
}

function renderForwardUpstream(listener, backends) {
  const transport = listener.upstream_tls ? '<span class="badge accent">TLS</span>' : '<span class="badge">Plaintext</span>';
  return `<div class="forward-upstream">${transport}${renderBackendBadges(backends, listener.target)}</div>`;
}

function renderBackendBadges(backends, configuredTargets = '') {
  const items = backends.length
    ? backends
    : splitTargets(configuredTargets).map(name => ({ name, online: null, since_ms: null }));
  if (!items.length) return '<span class="muted">No targets</span>';
  return `<div class="backend-list">${items.map(backend => {
    const state = backend.online === true ? 'online' : backend.online === false ? 'offline' : 'unknown';
    const label = state === 'unknown' ? 'not checked yet' : state;
    return `
    <span class="backend-chip ${state}" title="${escapeAttr(state === 'unknown' ? 'Not checked yet; still eligible for selection' : `${state[0].toUpperCase()}${state.slice(1)} since ${formatSince(backend.since_ms)}`)}">
      <span>${escapeHtml(backend.name)}</span>
      <small>${label}${state === 'unknown' ? '' : ` &middot; ${escapeHtml(formatSince(backend.since_ms))}`}</small>
    </span>`;
  }).join('')}</div>`;
}

function renderDns() {
  const sections = [
    ['exact', 'Exact', 'Matches the hostname exactly. <code>host:port</code> restricts the rule to one port; <code>host</code> alone matches any port.',
      [['from', 'From', 'abc.com:443'], ['to', 'To', '127.0.0.1:443']]],
    ['suffix', 'Suffix', 'Matches a domain and all its subdomains at label boundaries (<code>.abc.com</code> matches <code>x.abc.com</code> and <code>abc.com</code>, never <code>notabc.com</code>). Append <code>:port</code> to restrict.',
      [['from', 'From', '.internal.abc.com:443'], ['to', 'To', '127.0.0.1:443']]],
    ['regex', 'Regex', 'Case-insensitive pattern matched against the hostname only (the port is never part of the text). Use the port column to restrict; leave it blank for any port. Rules are tried top to bottom.',
      [['hostname', 'Hostname pattern', '^api\\d+\\.abc\\.com$'], ['port', 'Port', '443'], ['to', 'To', '127.0.0.1:443']]],
  ];
  const panels = sections.map(([kind, title, help, fields]) => {
    const rules = state.dns[kind];
    const header = fields.map(([, label]) => `<th>${label}</th>`).join('');
    const rows = rules.map((rule, index) => `
      <tr>
        ${fields.map(([field]) => `<td>${field === 'port' && (rule[field] == null || rule[field] === '') ? '<span class="muted">any</span>' : `<code>${escapeHtml(rule[field] ?? '')}</code>`}</td>`).join('')}
        <td class="row-actions">
          <button class="button small" data-edit-dns="${kind}:${index}">Edit</button>
          <button class="button small outline-danger" data-delete-dns="${kind}:${index}">Remove</button>
        </td>
      </tr>`).join('');
    return `
    <section class="panel">
      <div class="panel-header">
        <div class="panel-title">
          <h2>${title} rules</h2>
          <p>${help}</p>
        </div>
        <button class="button primary" data-add-dns="${kind}">Add rule</button>
      </div>
      <div class="table-wrap">
        ${rows ? `
          <table>
            <thead><tr>${header}<th></th></tr></thead>
            <tbody>${rows}</tbody>
          </table>` : `
          <div class="empty"><strong>No ${title.toLowerCase()} rules</strong></div>`}
      </div>
    </section>`;
  }).join('');
  return `
    <div class="note info">
      Resolution priority: exact host:port, then exact host, then suffix rules with a port,
      then suffix rules without (longer suffixes win), then regex rules in order. First hit wins;
      no hit means normal DNS. A destination without a port keeps the listener's target port.
      DNS changes are edited locally and are written only when you use Save or Apply & restart.
    </div>
    ${panels}`;
}

/* ---------- Event binding ---------- */

function bindView() {
  content.querySelectorAll('[data-operation]').forEach(button => {
    button.addEventListener('click', () => serviceOperation(button.dataset.operation));
  });
  content.querySelector('[data-add-listener]')?.addEventListener('click', () => openListenerDialog());
  content.querySelectorAll('[data-active-listener]').forEach(button => {
    button.addEventListener('click', () => {
      closeActionMenus();
      openActiveConnectionsDialog(button.dataset.activeListener);
    });
  });
  content.querySelectorAll('[data-action-menu]').forEach(button => {
    button.addEventListener('click', event => {
      event.stopPropagation();
      openActionMenu(button, button.dataset.actionMenu);
    });
  });
  content.querySelectorAll('[data-listener-operation]').forEach(button => {
    button.addEventListener('click', () => {
      closeActionMenus();
      const index = button.dataset.listenerOperation.lastIndexOf(':');
      listenerOperation(
        button.dataset.listenerOperation.slice(0, index),
        button.dataset.listenerOperation.slice(index + 1),
      );
    });
  });
  content.querySelectorAll('[data-edit-listener]').forEach(button => {
    button.addEventListener('click', () => {
      closeActionMenus();
      openListenerDialog(button.dataset.editListener);
    });
  });
  content.querySelectorAll('[data-delete-listener]').forEach(button => {
    button.addEventListener('click', async () => {
      closeActionMenus();
      const name = button.dataset.deleteListener;
      if (!await confirmAction(`Remove listener “${name}”?`,
        'The listener is removed from the pending configuration. It keeps running until you save and apply.')) return;
      delete state.listeners[name];
      markDirty();
      render();
    });
  });
  content.querySelectorAll('[data-add-dns]').forEach(button => {
    button.addEventListener('click', () => openDnsDialog(button.dataset.addDns));
  });
  content.querySelectorAll('[data-edit-dns]').forEach(button => {
    button.addEventListener('click', () => {
      const [kind, index] = button.dataset.editDns.split(':');
      openDnsDialog(kind, Number(index));
    });
  });
  content.querySelectorAll('[data-delete-dns]').forEach(button => {
    button.addEventListener('click', () => {
      const [kind, index] = button.dataset.deleteDns.split(':');
      state.dns[kind].splice(Number(index), 1);
      markDirty();
      render();
    });
  });
  bindListenerDialog();
  bindDnsDialog();
}

function closeActionMenus() {
  document.querySelectorAll('.floating-action-menu').forEach(menu => menu.remove());
}

function openActionMenu(anchor, name) {
  const existing = document.querySelector('.floating-action-menu');
  if (existing?.dataset.listener === name) {
    existing.remove();
    return;
  }
  closeActionMenus();
  const menu = document.createElement('div');
  menu.className = 'floating-action-menu';
  menu.dataset.listener = name;
  menu.innerHTML = `
    <button type="button" class="icon-action" title="Start" aria-label="Start listener" data-listener-operation="${escapeAttr(name)}:start">&#9654;</button>
    <button type="button" class="icon-action" title="Stop" aria-label="Stop listener" data-listener-operation="${escapeAttr(name)}:stop">&#9632;</button>
    <button type="button" class="icon-action" title="Restart" aria-label="Restart listener" data-listener-operation="${escapeAttr(name)}:restart">&#8635;</button>
    <button type="button" data-edit-listener="${escapeAttr(name)}">Edit</button>
    <button type="button" data-active-listener="${escapeAttr(name)}">Connections</button>
    <button type="button" class="danger-text" data-delete-listener="${escapeAttr(name)}">Remove</button>`;
  document.body.appendChild(menu);
  bindFloatingActionMenu(menu);
  const rect = anchor.getBoundingClientRect();
  const menuRect = menu.getBoundingClientRect();
  const left = Math.max(8, Math.min(window.innerWidth - menuRect.width - 8, rect.right - menuRect.width));
  const top = Math.max(8, Math.min(window.innerHeight - menuRect.height - 8, rect.bottom + 6));
  menu.style.left = `${left}px`;
  menu.style.top = `${top}px`;
}

function bindFloatingActionMenu(menu) {
  menu.querySelector('[data-active-listener]')?.addEventListener('click', event => {
    closeActionMenus();
    openActiveConnectionsDialog(event.currentTarget.dataset.activeListener);
  });
  menu.querySelector('[data-edit-listener]')?.addEventListener('click', event => {
    closeActionMenus();
    openListenerDialog(event.currentTarget.dataset.editListener);
  });
  menu.querySelector('[data-delete-listener]')?.addEventListener('click', async event => {
    closeActionMenus();
    const name = event.currentTarget.dataset.deleteListener;
    if (!await confirmAction(`Remove listener “${name}”?`,
      'The listener is removed from the pending configuration. It keeps running until you save and apply.')) return;
    delete state.listeners[name];
    markDirty();
    render();
  });
  menu.querySelectorAll('[data-listener-operation]').forEach(button => {
    button.addEventListener('click', event => {
      closeActionMenus();
      const value = event.currentTarget.dataset.listenerOperation;
      const index = value.lastIndexOf(':');
      listenerOperation(value.slice(0, index), value.slice(index + 1));
    });
  });
}

function bindListenerDialog() {
  const dialog = document.querySelector('#listener-dialog');
  const form = document.querySelector('#listener-form');
  if (!dialog || !form) return;
  document.querySelector('#listener-mode').onchange = syncListenerDialogMode;
  form.onsubmit = event => {
    event.preventDefault();
    saveListenerDialog();
  };
  document.querySelector('#listener-cancel').onclick = () => dialog.close();
}

function openListenerDialog(name = null) {
  const dialog = document.querySelector('#listener-dialog');
  const source = name ? state.listeners[name] : defaultListener();
  document.querySelector('#listener-dialog-title').textContent = name ? 'Edit listener' : 'Add listener';
  document.querySelector('#listener-original-name').value = name || '';
  document.querySelector('#listener-name').value = name || '';
  document.querySelector('#listener-bind').value = source.bind || '';
  document.querySelector('#listener-target').value = source.target || '';
  document.querySelector('#listener-target-port').value = Number(source.target_port || 443);
  document.querySelector('#listener-mode').value = source.mode || 'passthrough';
  document.querySelector('#listener-upstream-tls').checked = Boolean(source.upstream_tls);
  document.querySelector('#listener-policy').value = source.policy || 'DENY';
  document.querySelector('#listener-static-hosts').value = (source.rules?.static_hosts || []).join('\n');
  document.querySelector('#listener-patterns').value = (source.rules?.patterns || []).join('\n');
  document.querySelector('#listener-idle-timeout').value = Number(source.max_idle_time_ms ?? 3600000);
  document.querySelector('#listener-speed-limit').value = Number(source.speed_limit ?? 0);
  syncListenerDialogMode();
  dialog.showModal();
  document.querySelector('#listener-name').focus();
}

function syncListenerDialogMode() {
  const mode = document.querySelector('#listener-mode')?.value || 'passthrough';
  const section = document.querySelector('#listener-upstream-section');
  const targetSection = document.querySelector('#listener-target-section');
  const targetPort = document.querySelector('#listener-target-port');
  const aclSection = document.querySelector('#listener-acl-section');
  if (section) section.hidden = !['terminate', 'forward'].includes(mode);
  if (targetSection) targetSection.hidden = mode !== 'forward';
  if (targetPort) targetPort.required = mode !== 'forward';
  if (aclSection) aclSection.hidden = mode === 'forward';
}

function saveListenerDialog() {
  const originalName = document.querySelector('#listener-original-name').value;
  const name = document.querySelector('#listener-name').value.trim();
  if (!name) return notify('Listener name is required.', true);
  if (name !== originalName && state.listeners[name]) {
    return notify(`Listener ${name} already exists.`, true);
  }
  const listener = cleanListener({
    bind: document.querySelector('#listener-bind').value.trim(),
    target: document.querySelector('#listener-target').value.trim(),
    target_port: Number(document.querySelector('#listener-target-port').value),
    mode: document.querySelector('#listener-mode').value,
    upstream_tls: document.querySelector('#listener-upstream-tls').checked,
    policy: document.querySelector('#listener-policy').value,
    rules: {
      static_hosts: lines(document.querySelector('#listener-static-hosts').value),
      patterns: lines(document.querySelector('#listener-patterns').value),
    },
    max_idle_time_ms: Number(document.querySelector('#listener-idle-timeout').value),
    speed_limit: Number(document.querySelector('#listener-speed-limit').value),
  });
  const validationError = validateListener(name, listener);
  if (validationError) return notify(validationError, true);
  if (originalName && originalName !== name) delete state.listeners[originalName];
  state.listeners[name] = listener;
  document.querySelector('#listener-dialog').close();
  markDirty();
  render();
}

function bindDnsDialog() {
  const dialog = document.querySelector('#dns-dialog');
  const form = document.querySelector('#dns-form');
  if (!dialog || !form) return;
  form.onsubmit = event => {
    event.preventDefault();
    saveDnsDialog();
  };
  document.querySelector('#dns-cancel').onclick = () => dialog.close();
}

function openDnsDialog(kind, index = null) {
  const dialog = document.querySelector('#dns-dialog');
  const rule = index == null ? {} : state.dns[kind][index];
  document.querySelector('#dns-dialog-title').textContent = `${index == null ? 'Add' : 'Edit'} ${kind} rule`;
  document.querySelector('#dns-kind').value = kind;
  document.querySelector('#dns-index').value = index == null ? '' : String(index);
  document.querySelector('#dns-from-wrap').hidden = kind === 'regex';
  document.querySelector('#dns-hostname-wrap').hidden = kind !== 'regex';
  document.querySelector('#dns-port-wrap').hidden = kind !== 'regex';
  document.querySelector('#dns-from').value = rule.from || '';
  document.querySelector('#dns-hostname').value = rule.hostname || '';
  document.querySelector('#dns-port').value = rule.port ?? '';
  document.querySelector('#dns-to').value = rule.to || '';
  dialog.showModal();
  (kind === 'regex' ? document.querySelector('#dns-hostname') : document.querySelector('#dns-from')).focus();
}

function saveDnsDialog() {
  const kind = document.querySelector('#dns-kind').value;
  const indexRaw = document.querySelector('#dns-index').value;
  const index = indexRaw === '' ? null : Number(indexRaw);
  const to = document.querySelector('#dns-to').value.trim();
  if (!to) return notify('DNS destination is required.', true);
  let rule;
  if (kind === 'regex') {
    const hostname = document.querySelector('#dns-hostname').value.trim();
    if (!hostname) return notify('Hostname pattern is required.', true);
    try { new RegExp(hostname); } catch (_) { return notify(`Invalid DNS regex: ${hostname}`, true); }
    const portRaw = document.querySelector('#dns-port').value.trim();
    const port = portRaw === '' ? null : Number(portRaw);
    if (port != null && (!Number.isInteger(port) || port < 1 || port > 65535)) {
      return notify('DNS regex port must be from 1 to 65535.', true);
    }
    rule = { hostname, port, to };
  } else {
    const from = document.querySelector('#dns-from').value.trim();
    if (!from) return notify('DNS match is required.', true);
    rule = { from, to };
  }
  if (index == null) state.dns[kind].push(rule);
  else state.dns[kind][index] = rule;
  document.querySelector('#dns-dialog').close();
  markDirty();
  render();
}

async function openActiveConnectionsDialog(listenerName) {
  const dialog = document.querySelector('#active-dialog');
  document.querySelector('#active-dialog-title').textContent = `Active connections: ${listenerName}`;
  document.querySelector('#active-dialog-body').innerHTML = '<div class="empty"><strong>Loading connections</strong></div>';
  dialog.showModal();
  clearActiveConnectionsTimer();
  dialog.onclose = clearActiveConnectionsTimer;
  await refreshActiveConnectionsDialog(listenerName);
  state.activeConnectionsTimer = setInterval(() => {
    if (dialog.open) refreshActiveConnectionsDialog(listenerName);
  }, 3000);
}

async function refreshActiveConnectionsDialog(listenerName) {
  try {
    const active = await request('/active/listeners');
    state.activeConnections = Array.isArray(active) ? active : [];
    const rows = state.activeConnections
      .filter(connection => connection.listener === listenerName)
      .map(connection => `
        <tr>
          <td><code>${escapeHtml(connection.request_id)}</code></td>
          <td>${escapeHtml(connection.remote_address)}</td>
          <td>${formatDuration(connection.uptime_ms)}</td>
          <td class="num">${formatBytes(connection.uploaded_bytes)}</td>
          <td class="num">${formatBytes(connection.downloaded_bytes)}</td>
        </tr>`)
      .join('');
    document.querySelector('#active-dialog-body').innerHTML = rows ? `
      <div class="table-wrap">
        <table>
          <thead><tr><th>Request</th><th>Remote</th><th>Uptime</th><th class="num">Uploaded</th><th class="num">Downloaded</th></tr></thead>
          <tbody>${rows}</tbody>
        </table>
      </div>` : '<div class="empty"><strong>No active connections</strong>This listener has no open proxy connections right now.</div>';
  } catch (error) {
    document.querySelector('#active-dialog-body').innerHTML = `<div class="empty"><strong>Unable to load connections</strong>${escapeHtml(error.message)}</div>`;
  }
}

function clearActiveConnectionsTimer() {
  if (state.activeConnectionsTimer) {
    clearInterval(state.activeConnectionsTimer);
    state.activeConnectionsTimer = null;
  }
}

/* ---------- Actions ---------- */

async function saveAll() {
  const validationError = validateConfiguration();
  if (validationError) return notify(validationError, true);
  busy(true);
  try {
    await persistConfiguration();
    setDirty(false);
    notify('Configuration saved. Apply & restart on the Overview page to activate it.');
  } catch (error) {
    notify(`Save failed: ${error.message}`, true);
  } finally {
    busy(false);
  }
}

async function serviceOperation(operation) {
  const messages = {
    apply: ['Apply configuration?', 'The pending configuration is saved and all listeners restart. Active proxy connections will be interrupted.'],
    stop: ['Stop listeners?', 'All active proxy connections will close immediately.'],
    reset: ['Revert to last applied configuration?', 'Unsaved listener and DNS changes will be discarded.'],
  };
  if (messages[operation] && !await confirmAction(...messages[operation])) return;
  const validationError = operation === 'apply' ? validateConfiguration() : null;
  if (validationError) return notify(validationError, true);
  busy(true);
  try {
    if (operation === 'apply') await persistConfiguration();
    await request(`/config/${operation}`, { method: 'POST', body: '' });
    notify({
      start: 'Listeners started.',
      stop: 'Listeners stopped.',
      apply: 'Configuration applied; listeners restarted.',
      reset: 'Last applied configuration restored.',
    }[operation] || `${operation} completed.`);
    await loadAll();
  } catch (error) {
    notify(`${operation} failed: ${error.message}`, true);
  } finally {
    busy(false);
  }
}

async function listenerOperation(name, operation) {
  const messages = {
    start: [`Start listener “${name}”?`, 'Pending configuration is saved first, then only this listener starts.'],
    stop: [`Stop listener “${name}”?`, 'Active connections for this listener will close. Other listeners keep running.'],
    restart: [`Restart listener “${name}”?`, 'Pending configuration is saved first, then only this listener restarts.'],
  };
  if (messages[operation] && !await confirmAction(...messages[operation])) return;
  const shouldPersist = ['start', 'restart'].includes(operation);
  const validationError = shouldPersist ? validateConfiguration() : null;
  if (validationError) return notify(validationError, true);
  busy(true);
  try {
    if (shouldPersist) {
      await persistConfiguration();
      setDirty(false);
    }
    if (operation === 'restart') {
      await request(`/config/listeners/${encodeURIComponent(name)}/stop`, { method: 'POST', body: '' });
      await sleepMs(500);
      await request(`/config/listeners/${encodeURIComponent(name)}/start`, { method: 'POST', body: '' });
    } else {
      await request(`/config/listeners/${encodeURIComponent(name)}/${operation}`, { method: 'POST', body: '' });
    }
    notify({
      start: `Listener ${name} started.`,
      stop: `Listener ${name} stopped.`,
      restart: `Listener ${name} restarted.`,
    }[operation] || `Listener ${name} updated.`);
    await loadAll();
  } catch (error) {
    notify(`Listener ${operation} failed: ${error.message}`, true);
  } finally {
    busy(false);
  }
}

function sleepMs(ms) {
  return new Promise(resolve => setTimeout(resolve, ms));
}

async function persistConfiguration() {
  await request('/config/listeners', { method: 'PUT', body: JSON.stringify(state.listeners) });
  await request('/config/dns', { method: 'PUT', body: JSON.stringify(state.dns) });
}

function validateConfiguration() {
  for (const [name, listener] of Object.entries(state.listeners)) {
    const error = validateListener(name, listener);
    if (error) return error;
  }
  for (const kind of ['exact', 'suffix']) {
    for (const rule of state.dns[kind]) {
      if (!rule.from?.trim()) return `A ${kind} DNS rule is missing its match.`;
      if (!rule.to?.trim() || /\s/.test(rule.to)) return `DNS destination for ${rule.from} is invalid.`;
    }
  }
  for (const rule of state.dns.regex) {
    if (!rule.hostname?.trim()) return 'A regex DNS rule is missing its hostname pattern.';
    if (rule.port != null && (!Number.isInteger(rule.port) || rule.port < 1 || rule.port > 65535)) {
      return `DNS regex rule ${rule.hostname} has an invalid port.`;
    }
    if (!rule.to?.trim() || /\s/.test(rule.to)) return `DNS destination for ${rule.hostname} is invalid.`;
  }
  return null;
}

function validateListener(name, listener) {
  if (!listener.bind?.trim()) return `Listener ${name} requires a bind address.`;
  if (listener.mode !== 'forward' && (!Number.isInteger(listener.target_port) || listener.target_port < 1 || listener.target_port > 65535)) {
    return `Listener ${name} requires a target port from 1 to 65535.`;
  }
  if (!['passthrough', 'terminate', 'forward'].includes(listener.mode)) return `Listener ${name} has an invalid mode.`;
  if (listener.mode === 'forward' && !splitTargets(listener.target).length) return `Listener ${name} requires at least one forward target.`;
  for (const target of splitTargets(listener.target)) {
    if (!/^[^\s:]+:\d+$/.test(target)) return `Listener ${name} has an invalid forward target: ${target}`;
    const port = Number(target.slice(target.lastIndexOf(':') + 1));
    if (!Number.isInteger(port) || port < 1 || port > 65535) return `Listener ${name} has an invalid forward target port: ${target}`;
  }
  if (listener.mode !== 'forward') {
    if (!['ALLOW', 'DENY'].includes(listener.policy)) return `Listener ${name} has an invalid policy.`;
    // Regex patterns are validated server-side: the backend uses Rust's regex
    // dialect, which JavaScript's RegExp would falsely reject (e.g. `(?i)`).
  }
  return null;
}

function defaultListener() {
  return cleanListener({
    bind: '0.0.0.0:1443',
    target: '',
    target_port: 443,
    policy: 'DENY',
    rules: { static_hosts: [], patterns: [] },
    max_idle_time_ms: 3600000,
    speed_limit: 0,
    mode: 'passthrough',
    upstream_tls: false,
  });
}

function cleanListener(listener) {
  const mode = listener.mode || 'passthrough';
  return {
    bind: listener.bind || '',
    target: listener.target || '',
    target_port: Number(listener.target_port || 443),
    policy: listener.policy || 'DENY',
    rules: {
      static_hosts: mode === 'forward' ? [] : (listener.rules?.static_hosts || []),
      patterns: mode === 'forward' ? [] : (listener.rules?.patterns || []),
    },
    max_idle_time_ms: listener.max_idle_time_ms ?? 3600000,
    speed_limit: listener.speed_limit ?? 0,
    mode,
    upstream_tls: ['terminate', 'forward'].includes(mode) && Boolean(listener.upstream_tls),
  };
}

function modeLabel(mode) {
  if (mode === 'terminate') return 'TLS termination';
  if (mode === 'forward') return 'Port forward';
  return 'TLS passthrough';
}

function splitTargets(targets = '') {
  return targets.split(/[;,]/).map(item => item.trim()).filter(Boolean);
}

function formatSince(ms) {
  if (!ms) return 'not checked';
  const elapsed = Math.max(0, Date.now() - Number(ms));
  return `${formatDuration(elapsed)} ago`;
}

/* ---------- UI primitives ---------- */

function confirmAction(title, message) {
  const dialog = document.querySelector('#confirm-dialog');
  document.querySelector('#confirm-title').textContent = title;
  document.querySelector('#confirm-message').textContent = message;
  dialog.showModal();
  return new Promise(resolve => {
    dialog.addEventListener('close', () => resolve(dialog.returnValue === 'confirm'), { once: true });
  });
}

function notify(message, error = false) {
  const toast = document.createElement('div');
  toast.className = `toast ${error ? 'error' : 'success'}`;
  toast.innerHTML = `<span>${escapeHtml(message)}</span><button aria-label="Dismiss">×</button>`;
  toast.querySelector('button').addEventListener('click', () => toast.remove());
  toasts.append(toast);
  while (toasts.children.length > 4) toasts.firstChild.remove();
  setTimeout(() => toast.remove(), error ? 9000 : 4500);
}

function busy(active) { loading.hidden = !active; }
function value(selector) { return content.querySelector(selector)?.value || ''; }
function lines(text) { return text.split(/\r?\n|,/).map(item => item.trim()).filter(Boolean); }
function fmt(value = 0) { return Number(value).toLocaleString(); }
function formatBytes(value = 0) {
  if (!value) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const exponent = Math.min(Math.floor(Math.log(value) / Math.log(1024)), units.length - 1);
  return `${(value / 1024 ** exponent).toFixed(exponent ? 1 : 0)} ${units[exponent]}`;
}
function formatDuration(ms) {
  if (ms == null) return '0s';
  let seconds = Math.floor(Number(ms) / 1000);
  const days = Math.floor(seconds / 86400);
  seconds %= 86400;
  const hours = Math.floor(seconds / 3600);
  seconds %= 3600;
  const minutes = Math.floor(seconds / 60);
  seconds %= 60;
  if (days) return `${days}d ${hours}h`;
  if (hours) return `${hours}h ${minutes}m`;
  if (minutes) return `${minutes}m ${seconds}s`;
  return `${seconds}s`;
}
function cssEscape(value) { return CSS.escape(`${value}`); }

function metric(label, value, note, key) {
  return `<article class="metric"><span>${label}</span><strong data-metric="${key}">${value}</strong><small>${note}</small></article>`;
}
function plainInput(label, id, placeholder, type = 'text') {
  return `<div class="field"><label for="${id}">${label}</label><input id="${id}" type="${type}" placeholder="${placeholder}" required></div>`;
}
function inputField(label, value, field, listener, helper = '') {
  return `<div class="field"><label>${label}</label><input data-listener="${escapeAttr(listener)}" data-field="${field}" value="${escapeAttr(value ?? '')}">${helperText(helper)}</div>`;
}
function numberField(label, value, field, listener, helper = '', step = '') {
  return `<div class="field"><label>${label}</label><input type="number" min="0" ${step ? `step="${step}"` : ''} data-listener="${escapeAttr(listener)}" data-field="${field}" value="${Number(value || 0)}">${helperText(helper)}</div>`;
}
function selectField(label, selected, field, listener, options) {
  return `<div class="field"><label>${label}</label><select data-listener="${escapeAttr(listener)}" data-field="${field}">${options.map(([value, text]) => `<option value="${value}" ${value === selected ? 'selected' : ''}>${text}</option>`).join('')}</select></div>`;
}
function checkboxField(label, checked, field, listener, helper = '') {
  return `<div class="field"><label><input type="checkbox" data-listener="${escapeAttr(listener)}" data-field="${field}" ${checked ? 'checked' : ''}> ${label}</label>${helperText(helper)}</div>`;
}
function textareaField(label, value, field, listener, helper) {
  return `<div class="field"><label>${label}</label><textarea data-listener="${escapeAttr(listener)}" data-field="${field}">${escapeHtml(value)}</textarea>${helperText(helper)}</div>`;
}
function helperText(helper) { return helper ? `<p class="helper">${helper}</p>` : ''; }
function escapeHtml(value) { return String(value ?? '').replace(/[&<>"']/g, char => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[char])); }
function escapeAttr(value) { return escapeHtml(value); }
