// 息壤桌面版（P0–P3）：多文件加载、可折叠树、增删改、图视图、实时校验。
const pathEl = document.getElementById('path');
const treeEl = document.getElementById('tree');
const countEl = document.getElementById('count');
const showAuxEl = document.getElementById('showAux');
const addBtn = document.getElementById('addBtn');
const newNameEl = document.getElementById('newName');
const newValueEl = document.getElementById('newValue');
const viewToggle = document.getElementById('viewToggle');
const graphEl = document.getElementById('graph');
const readonlyEl = document.getElementById('readonly');
const undoBtn = document.getElementById('undoBtn');
const redoBtn = document.getElementById('redoBtn');
let lastOp = null; // { type: 'create'|'update'|'remove', file, nodeId, newValue? }
let redoOp = null;
const collapsed = new Set(); // 折叠的节点 id（刷新后保持折叠态）
const expanded = new Set(); // 用户展开的「默认折叠」节点（@模板/@实例）

// 禁用原生右键菜单（翻译等），后续可换息壤自己的菜单
document.addEventListener('contextmenu', (e) => e.preventDefault());

let currentPaths = [];   // 已打开的文件路径（逗号分隔）
let selectedId = null;   // 选中节点 id
let selectedFile = null; // 选中节点所在文件
let currentView = 'tree';

document.getElementById('open').addEventListener('click', load);
pathEl.addEventListener('keydown', (e) => { if (e.key === 'Enter') load(); });

// 浏览…：系统文件选择对话框（可多选）
document.getElementById('browse').addEventListener('click', async () => {
  try {
    const selected = await window.__TAURI__.dialog.open({
      multiple: true,
      directory: false,
      filters: [{ name: 'XiRang', extensions: ['xirang'] }],
    });
    if (selected) {
      const paths = Array.isArray(selected) ? selected : [selected];
      pathEl.value = paths.join(', ');
      load();
    }
  } catch (err) { alert(`选择文件失败：${err}`); }
});

function parsePaths() {
  return pathEl.value.split(/[,;]/).map((s) => s.trim()).filter(Boolean);
}

async function load() {
  const paths = parsePaths();
  if (paths.length === 0) return;
  currentPaths = paths;
  selectedId = null;
  selectedFile = null;
  addBtn.disabled = true;
  try {
    const data = await window.invoke('load_tree', { paths });
    window.__lastData = data;
    countEl.textContent = `${data.total} 节点 · ${paths.length} 文件`;
    renderFiles(data.files);
    await showValidation();
  } catch (err) {
    treeEl.innerHTML = `<div class="error">错误：${err}</div>`;
  }
}

async function refresh() {
  if (currentPaths.length === 0) return;
  try {
    const data = await window.invoke('load_tree', { paths: currentPaths });
    window.__lastData = data;
    countEl.textContent = `${data.total} 节点 · ${currentPaths.length} 文件`;
    renderFiles(data.files);
    await showValidation();
  } catch (err) {
    treeEl.innerHTML = `<div class="error">错误：${err}</div>`;
  }
}

async function showValidation() {
  const errorsEl = document.getElementById('errors');
  if (currentPaths.length === 0) { errorsEl.innerHTML = ''; return; }
  try {
    const errs = await window.invoke('validate', { path: currentPaths[0] });
    if (errs.length === 0) {
      errorsEl.innerHTML = '';
    } else {
      errorsEl.innerHTML = `⚠️ ${errs.length} 个校验错误：<ul>${errs.map((e) => `<li>${e}</li>`).join('')}</ul>`;
    }
  } catch (e) { errorsEl.innerHTML = ''; }
}

function escapeHtml(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

// 可读化：文本值若是合法 JSON，则用缩进多行展示（如「@format=json」的导出结果）
function fmtVal(v, type) {
  if (type === 'text' && typeof v === 'string') {
    try { return JSON.stringify(JSON.parse(v), null, 2); } catch (e) { /* 不是 JSON，原样 */ }
  }
  return v;
}

function renderFiles(files) {
  treeEl.innerHTML = '';
  const showAux = showAuxEl.checked;
  for (const f of files) {
    const header = document.createElement('div');
    header.className = 'file-header';
    header.textContent = `📄 ${f.path}（${f.nodeCount} 节点）`;
    treeEl.appendChild(header);
    for (const root of f.roots) {
      treeEl.appendChild(renderNode(root, showAux, f.path, null));
    }
  }
}

function renderNode(node, showAux, file, markedKind) {
  const div = document.createElement('div');
  div.className = 'node';

  const isAux = node.isAux;
  // 注释式编辑规则：找最近的「有标注的根」。@模板(空)根→受保护；@实例根或其它→可编辑。无例外。
  const isTemplate = node.children.some((c) => c.name === '@模板' && (c.value == null));
  const isInstance = node.children.some((c) => c.name === '@实例');
  const selfMarked = isTemplate ? 'template' : (isInstance ? 'instance' : null);
  const effectiveMarked = selfMarked || markedKind;
  const protectedNode = effectiveMarked === 'template';
  const canEdit = !protectedNode && !readonlyEl.checked;
  const name = node.name || '(空节点)';

  const hasChildren = node.children.some((c) => showAux || !c.isAux);
  const head = document.createElement('div');
  head.className = 'head' + (isAux ? ' aux' : '') + (node.id === selectedId ? ' selected' : '');

  const toggle = document.createElement('span');
  toggle.className = 'toggle';
  toggle.textContent = hasChildren ? '▾' : '·';

  // 名字：@模板 作「模板库」入口展示；可内联编辑（点一下直接改）
  const displayName = node.name === '@模板' ? '📚 模板库' : (node.name === '@实例' ? '📦 实例' : name);
  const nameSpan = document.createElement('span');
  nameSpan.className = 'name';
  nameSpan.textContent = displayName;
  if (canEdit) {
    nameSpan.contentEditable = 'true';
    nameSpan.title = '点击编辑名字';
    nameSpan.addEventListener('blur', async () => {
      const nv = nameSpan.textContent.trim();
      if (nv !== node.name) {
        try {
          await window.invoke('rename_node', { path: file, nodeId: node.id, name: nv });
          refresh();
        } catch (err) { alert(`改名失败：${err}`); }
      }
    });
  }

  head.appendChild(toggle);
  head.appendChild(nameSpan);

  // 值：简单类型可内联编辑
  if (node.value != null) {
    head.appendChild(document.createTextNode(' = '));
    const valSpan = document.createElement('span');
    valSpan.className = 'value';
    valSpan.textContent = fmtVal(node.value, node.type);
    const editable = ['text', 'integer', 'float', 'boolean'].includes(node.type);
    if (canEdit && editable) {
      valSpan.contentEditable = 'true';
      valSpan.title = '点击编辑值';
      valSpan.addEventListener('blur', async () => {
        const vv = valSpan.textContent.trim();
        // 与「展示值」比较（JSON 已格式化，避免点一下没改却触发保存）
        if (vv !== fmtVal(node.value, node.type)) {
          try {
            await window.invoke('update_node', { path: file, nodeId: node.id, value: vv });
            lastOp = { type: 'update', file, nodeId: node.id, newValue: vv };
            undoBtn.disabled = false;
            redoBtn.disabled = true;
            refresh();
          } catch (err) { alert(`改值失败：${err}`); }
        }
      });
    }
    head.appendChild(valSpan);
  }

  // 按钮：＋新增子节点、删
  if (canEdit) {
    // 「＋」= 进入待填态：选中该节点为父、聚焦顶部新名/新值框；点「＋新增子节点」才真正新建
    const add = document.createElement('button');
    add.className = 'del';
    add.textContent = '＋';
    add.title = '在此节点下新建（填好名/值后点「＋新增子节点」）';
    add.addEventListener('click', (e) => {
      e.stopPropagation();
      selectedId = node.id;
      selectedFile = file;
      addBtn.disabled = false;
      addBtn.title = `在「${name}」下新增（${file}）`;
      Array.from(treeEl.querySelectorAll('.head')).forEach((h) => h.classList.remove('selected'));
      head.classList.add('selected');
      newNameEl.value = '';
      newValueEl.value = '';
      newNameEl.focus();
    });

    const del = document.createElement('button');
    del.className = 'del';
    del.textContent = '删';
    del.title = '删除（置空）';
    del.addEventListener('click', async (e) => {
      e.stopPropagation();
      try {
        await window.invoke('remove_node', { path: file, nodeId: node.id });
        lastOp = { type: 'remove', file, nodeId: node.id };
        undoBtn.disabled = false;
        refresh();
      } catch (err) { alert(`删除失败：${err}`); }
    });

    head.appendChild(add);
    head.appendChild(del);
  }

  // 点击 head 空白处 / toggle 选中（供工具栏新增）
  head.addEventListener('click', (e) => {
    if (e.target !== head && !toggle.contains(e.target)) return;
    if (protectedNode) return;
    selectedId = node.id;
    selectedFile = file;
    addBtn.disabled = false;
    addBtn.title = `在「${name}」下新增（${file}）`;
    Array.from(treeEl.querySelectorAll('.head')).forEach((h) => h.classList.remove('selected'));
    head.classList.add('selected');
  });
  div.appendChild(head);

  if (hasChildren) {
    const kids = document.createElement('div');
    kids.className = 'children';
    const defaultCollapsed = node.name === '@模板' || node.name === '@实例';
    const isCollapsed = defaultCollapsed ? !expanded.has(node.id) : collapsed.has(node.id);
    kids.style.display = isCollapsed ? 'none' : '';
    toggle.textContent = isCollapsed ? '▸' : '▾';
    for (const c of node.children) {
      if (!showAux && c.isAux) continue;
      kids.appendChild(renderNode(c, showAux, file, effectiveMarked));
    }
    div.appendChild(kids);
    toggle.addEventListener('click', (e) => {
      e.stopPropagation();
      const nowHidden = kids.style.display !== 'none';
      kids.style.display = nowHidden ? 'none' : '';
      toggle.textContent = nowHidden ? '▸' : '▾';
      // 默认折叠的节点（@模板/@实例）用 expanded 记忆展开态，其余用 collapsed
      if (nowHidden) { expanded.delete(node.id); collapsed.add(node.id); }
      else { expanded.add(node.id); collapsed.delete(node.id); }
    });
  }
  return div;
}

readonlyEl.addEventListener('change', () => {
  addBtn.disabled = readonlyEl.checked;
  if (window.__lastData) renderFiles(window.__lastData.files);
});

addBtn.addEventListener('click', async () => {
  if (readonlyEl.checked) return;
  const name = newNameEl.value.trim();
  if (!name) { alert('请输入节点名'); newNameEl.focus(); return; } // 不创建占位「未命名」
  const value = newValueEl.value;
  const file = selectedFile || currentPaths[0];
  try {
    const newId = await window.invoke('create_node', {
      path: file,
      parent: selectedId || 'nil',
      name,
      value,
    });
    lastOp = { type: 'create', file, nodeId: newId };
    undoBtn.disabled = false;
    newNameEl.value = '';
    newValueEl.value = '';
    refresh();
    newNameEl.focus(); // 保持焦点，便于连续新增
  } catch (err) { alert(`新增失败：${err}`); }
});
// 在「名」里回车 = 点「＋新增子节点」（保存）
newNameEl.addEventListener('keydown', (e) => { if (e.key === 'Enter') addBtn.click(); });

undoBtn.addEventListener('click', async () => {
  if (!lastOp) return;
  const op = lastOp;
  try {
    if (op.type === 'create') {
      await window.invoke('remove_node', { path: op.file, nodeId: op.nodeId });
      redoOp = null;
      redoBtn.disabled = true;
    } else {
      await window.invoke('revert_node', { path: op.file, nodeId: op.nodeId });
      redoOp = op;
      redoBtn.disabled = false;
    }
    lastOp = null;
    undoBtn.disabled = true;
    refresh();
  } catch (err) { alert(`撤销失败：${err}`); }
});

redoBtn.addEventListener('click', async () => {
  if (!redoOp) return;
  const op = redoOp;
  try {
    if (op.type === 'update') {
      await window.invoke('update_node', { path: op.file, nodeId: op.nodeId, value: op.newValue });
    } else if (op.type === 'remove') {
      await window.invoke('remove_node', { path: op.file, nodeId: op.nodeId });
    }
    lastOp = op;
    redoOp = null;
    redoBtn.disabled = true;
    undoBtn.disabled = false;
    refresh();
  } catch (err) { alert(`重做失败：${err}`); }
});

showAuxEl.addEventListener('change', () => {
  if (window.__lastData) renderFiles(window.__lastData.files);
});

// —— 主题 / 帮助 / i18n ——
const I18N = {
  zh: {
    open: '打开', showAux: '显示辅助节点', add: '＋新增子节点',
    name: '名', value: '值(可空)', help: '帮助',
    path: '.xirang 路径（多文件用逗号分隔）',
    view: { tree: '图视图', graph: '分屏', split: '树视图' },
    helpText: '息壤 XiRang 阅读编辑工具\n\n• 路径框可输入多个文件（逗号/分号分隔）→ 多文件同窗\n• 点节点选中 → 顶部「新增子节点」；节点上「改」改值、「删」置空删除\n• 「图视图」→ 跨文件引用边\n• 「显示辅助节点」→ 切换 @ 节点显隐\n\nCLI 见 rust/cli（xr），验收见 验收测试清单.md',
  },
  en: {
    open: 'Open', showAux: 'Show aux', add: '＋Add child',
    name: 'Name', value: 'Value(opt)', help: 'Help',
    path: '.xirang path(s), comma-separated',
    view: { tree: 'Graph', graph: 'Split', split: 'Tree' },
    helpText: 'XiRang reader/editor\n\n• Enter multiple paths (comma/semicolon) for multi-file\n• Click a node to select → add child / edit / delete\n• "Graph" → cross-file reference edges\n• "Show aux" → toggle @ nodes\n\nCLI: rust/cli (xr). Acceptance: 验收测试清单.md',
  },
};
let lang = 'zh';
function applyLang() {
  document.getElementById('open').textContent = I18N[lang].open;
  document.getElementById('showAuxLabel').textContent = I18N[lang].showAux;
  document.getElementById('addBtn').textContent = I18N[lang].add;
  document.getElementById('newName').placeholder = I18N[lang].name;
  document.getElementById('newValue').placeholder = I18N[lang].value;
  document.getElementById('helpBtn').textContent = I18N[lang].help;
  pathEl.placeholder = I18N[lang].path;
  viewToggle.textContent = I18N[lang].view[currentView];
}
document.getElementById('langToggle').addEventListener('click', (e) => {
  lang = lang === 'zh' ? 'en' : 'zh';
  e.target.textContent = lang === 'zh' ? 'EN' : '中';
  applyLang();
});
document.getElementById('themeToggle').addEventListener('click', (e) => {
  const dark = document.body.classList.toggle('dark');
  e.target.textContent = dark ? '☀️' : '🌙';
});
document.getElementById('helpBtn').addEventListener('click', () => {
  alert(I18N[lang].helpText);
});

// —— 图视图（引用图）：树 / 图 / 分屏 三态 ——
const VIEW_ORDER = ['tree', 'graph', 'split'];

viewToggle.addEventListener('click', () => {
  const i = VIEW_ORDER.indexOf(currentView);
  const next = VIEW_ORDER[(i + 1) % VIEW_ORDER.length];
  applyView(next);
});

function applyView(view) {
  currentView = view;
  viewToggle.textContent = I18N[lang].view[view];
  const views = document.querySelector('.views');
  if (view === 'tree') {
    views.classList.remove('split');
    treeEl.style.display = '';
    graphEl.style.display = 'none';
  } else if (view === 'graph') {
    views.classList.remove('split');
    treeEl.style.display = 'none';
    graphEl.style.display = 'block';
    loadGraphData();
  } else {
    views.classList.add('split');
    treeEl.style.display = '';
    graphEl.style.display = 'block';
    loadGraphData();
  }
}

async function loadGraphData() {
  if (currentPaths.length === 0) return;
  try {
    const data = await window.invoke('load_graph', { paths: currentPaths });
    renderGraph(data);
  } catch (err) { alert(`图视图失败：${err}`); }
}

function renderGraph(data) {
  const nodes = data.nodes, edges = data.edges;
  const W = graphEl.clientWidth || 1000, H = graphEl.clientHeight || 600;
  const cx = W / 2, cy = H / 2, R = Math.min(W, H) / 2 - 60;
  const pos = {};
  const n = Math.max(nodes.length, 1);
  nodes.forEach((node, i) => {
    const ang = (2 * Math.PI * i) / n;
    pos[node.id] = { x: cx + R * Math.cos(ang), y: cy + R * Math.sin(ang) };
  });
  const edgesSvg = edges.map((e) => {
    const a = pos[e.from], b = pos[e.to];
    if (!a || !b) return '';
    return `<line x1="${a.x}" y1="${a.y}" x2="${b.x}" y2="${b.y}" stroke="#0969da" stroke-opacity="0.5" marker-end="url(#arrow)" />`;
  }).join('');
  const nodesSvg = nodes.map((node) => {
    const p = pos[node.id];
    const label = node.name || '(空)';
    const fill = node.isAux ? '#8b5e3c' : '#0969da';
    const fileLabel = node.file ? `\n${node.file.split('/').pop()}` : '';
    return `<g><circle cx="${p.x}" cy="${p.y}" r="10" fill="${fill}" /><text x="${p.x + 14}" y="${p.y + 2}" font-size="12">${escapeHtml(label)}</text><text x="${p.x + 14}" y="${p.y + 16}" font-size="9" fill="#888">${escapeHtml(fileLabel)}</text></g>`;
  }).join('');
  graphEl.innerHTML = `<defs><marker id="arrow" markerWidth="8" markerHeight="8" refX="8" refY="4" orient="auto"><path d="M0,0 L8,4 L0,8 z" fill="#0969da"/></marker></defs>${edgesSvg}${nodesSvg}`;
}
