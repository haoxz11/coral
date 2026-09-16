/* coral app.js — 原生 ES module，无依赖无构建链（≤15KB 预算） */

// ===== 主题切换（localStorage 持久化） =====
const themeToggle = document.getElementById('theme-toggle');
function applyTheme(t) {
  // VitePress 开关：亮暗由 data-theme 驱动 CSS（图标/滑块位移），无 JS 文案
  document.documentElement.dataset.theme = t;
  themeToggle.title = t === 'dark' ? '切换到亮色模式' : '切换到暗色模式';
}
themeToggle.addEventListener('click', () => {
  const cur = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
  localStorage.setItem('coral-theme', cur);
  applyTheme(cur);
});
applyTheme(document.documentElement.dataset.theme);

// ===== 移动端抽屉（<993px）与桌面端侧栏折叠开关（≥993px）=====
// 错误页无侧栏：sidebar/tree 元素缺失时跳过相关逻辑
const sidebar = document.getElementById('sidebar');
const mask = document.getElementById('sidebar-mask');
const layout = document.querySelector('.layout');
const hasSidebar = !!sidebar;
const mqDesktop = matchMedia('(min-width: 993px)');
// 桌面端折叠状态跨会话记忆
if (hasSidebar && localStorage.getItem('coral-sidebar-collapsed') === '1') {
  layout.classList.add('sidebar-collapsed');
}
function toggleSidebarCollapse() {
  const collapsed = layout.classList.toggle('sidebar-collapsed');
  localStorage.setItem('coral-sidebar-collapsed', collapsed ? '1' : '0');
  positionSplitToggle();
}
// 顶栏☰：移动端开抽屉；桌面端（<993px 无抽屉时的兜底，理论不显示）同分隔线按钮
document.getElementById('sidebar-toggle')?.addEventListener('click', () => {
  if (mqDesktop.matches) {
    toggleSidebarCollapse();
  } else {
    sidebar?.classList.add('open');
    mask?.classList.add('show');
  }
});
// 分隔线中央按钮：桌面端收起/展开
const splitToggle = document.getElementById('sidebar-split-toggle');
splitToggle?.addEventListener('click', toggleSidebarCollapse);

// 按钮水平定位：无滚动条时居中在分隔线（left=侧栏宽）；
// 侧栏有滚动条（占位宽度 > 0）时居中到滚动条上（left = 侧栏宽 - 滚动条宽/2），
// 与滚动条视觉对齐不打架。侧栏宽度固定 280px（CSS 常量）。
function positionSplitToggle() {
  if (!splitToggle || !sidebar) return;
  const collapsed = layout.classList.contains('sidebar-collapsed');
  if (collapsed) {
    splitToggle.style.left = '0px'; // 收起态：贴内容左缘（CSS 半嵌 + 箭头翻转）
    return;
  }
  const sbw = sidebar.offsetWidth - sidebar.clientWidth; // 滚动条占位宽度
  const left = sbw > 0 ? 280 - sbw / 2 : 280; // 无滚动条居分隔线，有则居滚动条
  splitToggle.style.left = left + 'px';
}
positionSplitToggle();
// 滚动条出现与否会变（懒加载/展开收起/视口变化），事后复定位
window.addEventListener('resize', positionSplitToggle);
mask?.addEventListener('click', closeDrawer);
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') closeDrawer();
});
function closeDrawer() {
  sidebar?.classList.remove('open');
  mask?.classList.remove('show');
}

// ===== 目录树：渲染 + 懒加载 =====
const treeEl = document.getElementById('tree');
const initialTree = hasSidebar ? JSON.parse(treeEl.dataset.tree || '[]') : [];
// 当前页 URL（decode 形态与节点 href encode 形态比较：直接用 location.pathname）
const currentPath = decodeURIComponent(location.pathname).replace(/\/$/, '') || '/';
// 默认形态 URL（服务端注入；permalink 页与 currentPath 不同，左树祖先链按它匹配）
const defaultPath = (document.getElementById('sidebar')?.dataset.defaultUrl || currentPath).replace(/\/$/, '') || '/';
// 当前页祖先目录 href 链（服务端注入，encode 形态）：目录带 permalink 时
// 其 URL 与子页面 URL 无前缀关系，展开判定不能按前缀推断
const dirHrefs = (document.getElementById('sidebar')?.dataset.dirHrefs || '')
  .split('|')
  .filter(Boolean)
  .map(normalizeUrl);

// scoped 树去根：服务端给的树根是一级目录自身，
// 前端丢弃根节点、把二级作为顶层渲染（一级目录入口在顶栏）。
// 全站树形态（根 url 为 '/'，当前布局下不会出现）保留首页注入逻辑。
const homeLabel = document.querySelector('.site-title')?.textContent?.trim() || '首页';
let renderTree = initialTree;
const isGlobalTree = initialTree.length === 0 || normalizeUrl(initialTree[0].url) === '/';
if (isGlobalTree) {
  initialTree.unshift({
    url: '/',
    title: homeLabel,
    node_type: 'leaf',
    has_children: false,
  });
} else {
  // scoped：取根的 children（二级）作为顶层；二级仅列出不默认展开
  // 当前页祖先链与显式记忆仍展开
  renderTree = initialTree[0].children || [];
}

function makeNode(node) {
  const li = document.createElement('li');
  const row = document.createElement(node.node_type === 'branch' ? 'div' : 'a');
  row.className = 'tree-node' + (node.node_type === 'leaf' ? ' leaf' : '');
  const navigable = node.node_type === 'leaf' || node.has_index;
  row.classList.toggle('navigable', navigable);
  // 高亮同时覆盖文档页与目录索引页（branch 的 url 即其 _index 页地址）
  const nu = normalizeUrl(node.url);
  if (nu === currentPath || nu === defaultPath) row.classList.add('current');
  if (node.node_type === 'leaf') {
    row.href = node.url;
  }
  const arrow = document.createElement('span');
  arrow.className = 'tree-arrow';
  arrow.style.visibility = node.node_type === 'branch' && node.has_children ? 'visible' : 'hidden';
  row.appendChild(arrow);
  if (node.icon) {
    // icon 判定：像路径/URL（含 / 或 http 开头）→ <img>；
    // 其余（含 : 的完整名与裸名）→ iconify-icon。裸名无前缀无法解析、
    // 渲染为空白——由扫描期 WARN 提示作者写全名，前端不猜测默认前缀
    const isImg = node.icon.includes('/') && !node.icon.includes(':');
    const icon = isImg
      ? Object.assign(document.createElement('img'), { src: node.icon, alt: '' })
      : Object.assign(document.createElement('iconify-icon'), { icon: node.icon });
    icon.className = 'tree-icon';
    row.appendChild(icon);
  }
  const label = document.createElement('span');
  label.className = 'tree-label';
  label.textContent = node.title;
  row.appendChild(label);

  li.appendChild(row);

  if (node.node_type === 'branch' && node.has_children) {
    const childrenEl = document.createElement('ul');
    childrenEl.className = 'tree-children';
    // 服务端已带的 children（首屏 initial_depth 内）直接渲染
    if (node.children && node.children.length > 0) {
      for (const c of node.children) childrenEl.appendChild(makeNode(c));
    }
    li.appendChild(childrenEl);
    // 恢复记忆的展开态 / 当前页祖先链默认展开
    initialLoads.push(applyBranchState(row, childrenEl, node));
    // 双区交互（M2）：箭头区=展开/收起（stopPropagation 防误触导航）；
    // 标题区=导航进目录首页（has_index 时）；无首页目录整行仍为展开
    arrow.addEventListener('click', (e) => {
      e.stopPropagation();
      toggleBranch(row, childrenEl, node);
    });
    if (node.has_index) {
      row.classList.add('navigable');
      row.addEventListener('click', () => {
        location.href = encodeURI(normalizeUrl(node.url));
      });
    } else {
      row.addEventListener('click', () => toggleBranch(row, childrenEl, node));
    }
  } else if (node.node_type === 'branch') {
    // 无子项的目录：点击仅展开（空）
    row.addEventListener('click', () => {
      row.classList.toggle('expanded');
      positionSplitToggle();
    });
  }
  return li;
}

function normalizeUrl(u) {
  return decodeURIComponent(u).replace(/\/$/, '') || '/';
}

// ===== 展开状态持久化（刷新不丢）=====
// 记录用户显式开关过的目录（url → boolean）；未记录的目录按"当前页祖先链默认展开"。
// 显式状态优先于祖先链：用户收起祖先后导航/刷新不反弹。
const EXPANDED_KEY = 'coral-tree-expanded';
let userExpanded = {};
try {
  userExpanded = JSON.parse(sessionStorage.getItem(EXPANDED_KEY) || '{}');
} catch {
  userExpanded = {}; // 损坏时静默回退为"无记忆"，不阻塞渲染
}
function saveExpanded() {
  sessionStorage.setItem(EXPANDED_KEY, JSON.stringify(userExpanded));
}
function isAncestorOfCurrent(dirUrl) {
  // 服务端给的祖先目录 href 链（permalink 目录也在内）优先；
  // 前缀兜底覆盖旧缓存页/无 data-dir-hrefs 的场景
  if (dirHrefs.includes(dirUrl)) return true;
  const hit = (p) => p === dirUrl || p.startsWith(dirUrl + '/');
  return hit(defaultPath) || hit(currentPath);
}

// 懒加载缓存：dir → children（二次展开不发请求）
const loadedDirs = new Map();

async function ensureChildren(childrenEl, node) {
  const dirUrl = normalizeUrl(node.url);
  if (loadedDirs.has(dirUrl) || (node.children && node.children.length > 0)) {
    return; // 已有数据（首屏或已加载）
  }
  // 首次展开：fetch children API（spinner 占位）
  const spinner = document.createElement('span');
  spinner.className = 'tree-spinner';
  childrenEl.appendChild(spinner);
  try {
    const res = await fetch('/api/tree/children?path=' + encodeURIComponent(dirUrl));
    if (!res.ok) throw new Error('HTTP ' + res.status);
    const data = await res.json();
    loadedDirs.set(dirUrl, data.children);
    spinner.remove();
    for (const c of data.children) childrenEl.appendChild(makeNode(c));
  } catch (e) {
    spinner.remove();
    const err = document.createElement('li');
    err.textContent = '加载失败';
    err.style.cssText = 'color:var(--text-2);padding:4px 8px;font-size:12px';
    childrenEl.appendChild(err);
  }
}

function applyBranchState(row, childrenEl, node) {
  const dirUrl = normalizeUrl(node.url);
  const explicit = userExpanded[dirUrl];
  // 展开优先级：用户显式开关 > 当前页祖先链
  const open = explicit !== undefined ? explicit : isAncestorOfCurrent(dirUrl);
  row.classList.toggle('expanded', open);
  childrenEl.classList.toggle('open', open);
  if (open) return ensureChildren(childrenEl, node); // 调用方聚合等待
  return Promise.resolve();
}

async function toggleBranch(row, childrenEl, node) {
  const expanded = row.classList.toggle('expanded');
  childrenEl.classList.toggle('open', expanded);
  userExpanded[normalizeUrl(node.url)] = expanded;
  saveExpanded();
  // 树高度变化可能使侧栏滚动条出现/消失，按钮位置需跟随
  positionSplitToggle();
  if (!expanded) return;
  await ensureChildren(childrenEl, node);
  positionSplitToggle();
}

// 收集首屏渲染期所有展开态目录的懒加载 Promise（当前页祖先链可能要 fetch 多层）
const initialLoads = [];
if (hasSidebar) for (const n of renderTree) treeEl.appendChild(makeNode(n));

// 顶栏 icon（导航项 + 文档库 logo + 首页 Hero/卡片）：
// data-icon → 渲染元素，判定与树节点同规则
document
  .querySelectorAll('[data-icon]:not(.tree-icon)')
  .forEach((a) => {
  const icon = a.dataset.icon;
  const isImg = icon.includes('/') && !icon.includes(':');
  if (isImg) {
    const el = Object.assign(document.createElement('img'), { src: icon, alt: '' });
    el.className = 'tree-icon';
    a.prepend(el);
  } else {
    const el = document.createElement('iconify-icon');
    el.setAttribute('icon', icon);
    // 同时设 width/height 属性：SVG 按盒子适配不按原始宽高比溢出（如 fa6 宽图标）
    const size = a.classList.contains('home-card-icon') ? 28
      : a.classList.contains('home-logo') ? 56
      : a.classList.contains('project-icon') ? 20 : 16;
    el.setAttribute('width', size);
    el.setAttribute('height', size);
    el.className = 'tree-icon';
    a.prepend(el);
  }
});

// 当前页节点滚到侧栏可视区中部：排空式等待懒加载祖先链（逐层 fetch 会
// 递归追加新 Promise，单次 Promise.all 只能等到第一层），且只滚侧栏容器
// （scrollIntoView 会牵动页面滚动位置）
(async () => {
  while (initialLoads.length > 0) {
    const batch = initialLoads.splice(0);
    await Promise.all(batch);
    positionSplitToggle?.();
  }
  const cur = document.querySelector('.tree-node.current');
  if (!cur) return;
  const sidebarEl = document.getElementById('sidebar');
  sidebarEl.scrollTop =
    cur.offsetTop - sidebarEl.clientHeight / 2 + cur.offsetHeight / 2;
})();

// ===== 详情页关键词高亮：URL ?hl=token1,token2（搜索结果页带入，
// 服务端 jieba 分词）；只处理正文区文本节点，代码块/属性不动 =====
(() => {
  const hl = new URLSearchParams(location.search).get('hl');
  if (!hl) return;
  // token 由服务端 jieba 分词并回退（专有名词整词），前端只滤空串
  const tokens = [...new Set(hl.split(',').map(decodeURIComponent).filter((t) => t.length > 0))];
  if (tokens.length === 0) return;
  const content = document.querySelector('.coral-content');
  if (!content) return;
  // 按长度降序替换，避免短 token 先命中破坏长 token 的完整匹配
  tokens.sort((a, b) => b.length - a.length);
  const walker = document.createTreeWalker(content, NodeFilter.SHOW_TEXT, {
    // 跳过代码块与已有 mark 内部（重入保护）
    acceptNode: (n) =>
      n.parentElement.closest('pre, code, mark')
        ? NodeFilter.FILTER_REJECT
        : NodeFilter.FILTER_ACCEPT,
  });
  const targets = [];
  for (let n = walker.nextNode(); n; n = walker.nextNode()) targets.push(n);
  let first = null;
  for (const node of targets) {
    const text = node.nodeValue;
    if (!tokens.some((t) => text.includes(t))) continue;
    // 分段构建：命中 token → <mark>；其余原文本
    const frag = document.createDocumentFragment();
    let rest = text;
    while (rest.length > 0) {
      let idx = -1, tok = null;
      for (const t of tokens) {
        const i = rest.indexOf(t);
        if (i >= 0 && (idx < 0 || i < idx)) {
          idx = i;
          tok = t;
        }
      }
      if (idx < 0) {
        frag.appendChild(document.createTextNode(rest));
        break;
      }
      if (idx > 0) frag.appendChild(document.createTextNode(rest.slice(0, idx)));
      const mark = document.createElement('mark');
      mark.textContent = tok;
      frag.appendChild(mark);
      if (!first) first = mark;
      rest = rest.slice(idx + tok.length);
    }
    node.parentNode.replaceChild(frag, node);
  }
  // 首个命中滚到可视区（顶部导航高度偏移）
  if (first) {
    window.scrollTo(0, first.getBoundingClientRect().top + window.scrollY - 80);
  }
})();

// ===== tabs 切换 =====
document.querySelectorAll('.tabs').forEach((tabs) => {
  const headers = tabs.querySelectorAll('.tab-header');
  headers.forEach((h) => {
    h.addEventListener('click', () => {
      const i = h.dataset.tab;
      headers.forEach((x) => x.classList.toggle('active', x === h));
      tabs.querySelectorAll('.tab-panel').forEach((p) => {
        p.classList.toggle('active', p.dataset.panel === i);
      });
    });
  });
});

// ===== 代码块：语言标签 + 复制按钮 =====
document.querySelectorAll('.coral-content pre').forEach((pre) => {
  const code = pre.querySelector('code');
  if (!code) return;
  // 外包 wrap：按钮/语言标签挂 wrap，不随 pre 内容滚动
  const wrap = document.createElement('div');
  wrap.className = 'code-wrap';
  pre.parentNode.insertBefore(wrap, pre);
  wrap.appendChild(pre);
  const m = code.className.match(/language-([\w-]+)/);
  if (m) {
    const lang = document.createElement('span');
    lang.className = 'code-lang';
    lang.textContent = m[1];
    wrap.appendChild(lang);
  }
  const ICON_COPY =
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>';
  const ICON_CHECK =
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6L9 17l-5-5"/></svg>';
  const btn = document.createElement('button');
  btn.className = 'code-copy';
  btn.title = '复制代码';
  btn.innerHTML = ICON_COPY;
  let copied = false;
  const toast = document.createElement('span');
  toast.className = 'copy-toast';
  toast.textContent = '已复制';
  const resetBtn = () => {
    copied = false;
    btn.disabled = false;
    btn.classList.remove('copied');
    btn.innerHTML = ICON_COPY;
    btn.title = '复制代码';
  };
  btn.addEventListener('click', async () => {
    if (copied) return;
    try {
      await navigator.clipboard.writeText(code.textContent);
      copied = true;
      btn.disabled = true;
      btn.classList.add('copied');
      btn.innerHTML = ICON_CHECK;
      btn.title = '已复制';
      btn.appendChild(toast);
      clearTimeout(btn._t);
      btn._t = setTimeout(resetBtn, 5000);
    } catch {
      btn.title = '复制失败';
      clearTimeout(btn._t);
      btn._t = setTimeout(() => (btn.title = '复制代码'), 2000);
    }
  });
  wrap.addEventListener('mouseleave', () => {
    clearTimeout(btn._t);
    resetBtn();
    toast.remove();
  });
  wrap.appendChild(btn);
});
// ===== TOC 当前节高亮（IntersectionObserver） =====
const tocItems = document.querySelectorAll('.toc-item');
// 当前节 = 最后一个越过视口顶 80px 参考线的标题；滚到底时最后一节兜底。
// scroll 驱动对锚点跳转/快速滚动/页尾短内容都正确（observer 方案有带宽外不触发的毛刺）
if (tocItems.length > 0) {
  const map = new Map();
  tocItems.forEach((a) => {
    const target = document.getElementById(decodeURIComponent(a.hash.slice(1)));
    if (target) map.set(target, a);
  });
  const headings = [...map.keys()];
  let activeEl = null;
  const setActive = (el) => {
    if (el === activeEl) return;
    activeEl?.classList.remove('active', 'toc-active-line');
    activeEl = el;
    el?.classList.add('active', 'toc-active-line');
  };
  const update = () => {
    const line = 80; // 与顶部导航栏高度对齐
    const atBottom =
      window.innerHeight + window.scrollY >= document.documentElement.scrollHeight - 2;
    let current = null;
    if (atBottom && headings.length > 0) {
      current = headings[headings.length - 1];
    } else {
      for (const h of headings) {
        if (h.getBoundingClientRect().top <= line) current = h;
        else break;
      }
      // 页顶（尚无标题越过参考线）默认第一节，VitePress 惯例
      if (!current && headings.length > 0) current = headings[0];
    }
    setActive(current ? map.get(current) : null);
  };
  window.addEventListener('scroll', update, { passive: true });
  update();

  // TOC 收起/展开：localStorage 持久化；
  // scroll 监听常驻（隐藏期间高亮持续更新），展开即定位当前节
  const pageBody = document.querySelector('.page-body');
  const expandBtn = document.getElementById('toc-expand');
  const collapseBtn = document.getElementById('toc-collapse');
  const applyTocState = (collapsed) => {
    pageBody.classList.toggle('toc-collapsed', collapsed);
    expandBtn.hidden = !collapsed;
  };
  applyTocState(localStorage.getItem('coral-toc-collapsed') === '1');
  collapseBtn?.addEventListener('click', () => {
    applyTocState(true);
    localStorage.setItem('coral-toc-collapsed', '1');
  });
  expandBtn?.addEventListener('click', () => {
    applyTocState(false);
    localStorage.setItem('coral-toc-collapsed', '0');
    document.querySelector('.toc-item.active')?.scrollIntoView({ block: 'nearest' });
  });
}

// ===== 标题锚点链接（¶ hover 显示） =====
document.querySelectorAll('.coral-content h1[id], .coral-content h2[id], .coral-content h3[id]').forEach((h) => {
  const a = document.createElement('a');
  a.className = 'anchor-chip';
  a.href = '#' + encodeURIComponent(h.id);
  a.textContent = '¶';
  h.appendChild(a);
});

// ===== 图片点击放大（控制栏 + 旋转 + 白底）=====
document.querySelectorAll('.coral-content img').forEach((img) => {
  img.classList.add('zoomable-img');
  img.addEventListener('click', () => openImageLayer(img));
});

function openImageLayer(img) {
  const layer = document.createElement('div');
  layer.className = 'media-layer';
  const im = document.createElement('img');
  im.src = img.src;
  im.alt = '';
  im.className = 'zoomable-img';
  if (/\.(png|svg)(\?|$)/i.test(img.src)) im.style.background = '#fff';
  // 图片绝对定位：中心点 (50%,50%) + translate(px,py) 偏移。
  // 不用滚动区（scroll 模拟平移对小图有边界死区、放大后锚不住视口中心，
  // M1 验收三十二/三十七轮的两难由此消除）：任何尺寸都居中展示，
  // 拖动是无边界的自由平移，缩放锚定视口中心（图片当前中心点）。
  im.style.position = 'absolute';
  im.style.left = '50%';
  im.style.top = '50%';
  im.style.transformOrigin = 'center center';
  // svg 的 naturalWidth 可能为 0：以首次布局尺寸为基准（闭包变量）。
  // rAF 读尺寸存在竞态（图片未解码完 naturalWidth=0）：complete 兜底 + load
  // 再校准一次，避免首帧按错误基准布局后错位
  const base = { w: 0, h: 0 };
  const measure = () => {
    base.w = im.naturalWidth || im.width;
    base.h = im.naturalHeight || im.height;
    if (base.w > 0) apply();
  };
  requestAnimationFrame(measure);
  im.addEventListener('load', measure, { once: true });
  let scale = 1, rot = 0, tx = 0, ty = 0;
  // apply(anchor)：scale 变化时锚定"图片当前中心点的视口坐标"不动，
  // 用户正在看的地方不因放大而飘走。推导：内容点 P 相对图片中心的方向
  // 向量随尺寸等比放大 k=scale/prevScale，P 视口不动 ⇒ 新中心 =
  // P - (P - 旧中心) * k。
  const apply = (anchor) => {
    const w = base.w || im.naturalWidth || im.width;
    const h = base.h || im.naturalHeight || im.height;
    if (anchor) {
      const k = scale / anchor.prevScale;
      const oldCx = layer.clientWidth / 2 + anchor.tx;
      const newCx = anchor.px - (anchor.px - oldCx) * k;
      const oldCy = layer.clientHeight / 2 + anchor.ty;
      const newCy = anchor.py - (anchor.py - oldCy) * k;
      tx = newCx - layer.clientWidth / 2;
      ty = newCy - layer.clientHeight / 2;
    }
    im.style.width = `${w * scale}px`;
    im.style.height = `${h * scale}px`;
    // 先平移到视口中心（-50% 对齐自身中心）再偏移/旋转：复合顺序保证
    // tx/ty 始终是"图片中心相对视口中心"的位移，与缩放/旋转无关
    im.style.transform = `translate(-50%, -50%) translate(${tx}px, ${ty}px) rotate(${rot}deg)`;
    pct.textContent = `${Math.round(scale * 100)}%`;
  };
  im.style.cursor = 'grab'; // 图片上手型，层空白为默认箭头
  im.addEventListener('click', (e) => e.stopPropagation());
  layer.appendChild(im);
  // 底部控制栏：缩小/放大/左转/右转/重置/关闭 + 比例
  // （无滚轮缩放——滚轮用于滚动；无点外关闭——仅 × 与 Esc）
  const bar = document.createElement('div');
  bar.className = 'media-bar';
  const mkBtn = (title, svg, fn) => {
    const b = document.createElement('button');
    b.title = title;
    b.innerHTML = svg;
    b.addEventListener('click', (e) => {
      e.stopPropagation();
      fn();
    });
    bar.appendChild(b);
    return b;
  };
  const I = (paths) => `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${paths}</svg>`;
  // 缩放/旋转统一走 anchor：以图片当前中心为锚点，视口内"正在看的位置"不飘
  const withAnchor = (fn) => () => {
    const prev = { prevScale: scale, tx, ty, px: layer.clientWidth / 2 + tx, py: layer.clientHeight / 2 + ty };
    fn();
    apply(prev);
  };
  mkBtn('缩小', I('<circle cx="11" cy="11" r="8"/><line x1="21" y1="21" x2="16.65" y2="16.65"/><line x1="8" y1="11" x2="14" y2="11"/>'),
    withAnchor(() => { scale = Math.max(0.2, scale * 0.8); }));
  mkBtn('放大', I('<circle cx="11" cy="11" r="8"/><line x1="21" y1="21" x2="16.65" y2="16.65"/><line x1="11" y1="8" x2="11" y2="14"/><line x1="8" y1="11" x2="14" y2="11"/>'),
    withAnchor(() => { scale = Math.min(5, scale * 1.25); }));
  mkBtn('左转', I('<path d="M2.5 2v6h6"/><path d="M2.5 8a10 10 0 1 1 1 7"/>'),
    withAnchor(() => { rot = (rot - 90) % 360; }));
  mkBtn('右转', I('<path d="M21.5 2v6h-6"/><path d="M21.5 8a10 10 0 1 0-1 7"/>'),
    withAnchor(() => { rot = (rot + 90) % 360; }));
  mkBtn('重置', I('<path d="M3 12a9 9 0 1 0 3-6.7L3 8"/><path d="M3 3v5h5"/>'),
    () => { scale = 1; rot = 0; tx = 0; ty = 0; apply(); });
  const pct = document.createElement('span');
  pct.className = 'media-pct';
  pct.textContent = '100%';
  bar.appendChild(pct);
  mkBtn('关闭', I('<line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/>'),
    () => close());
  bar.addEventListener('click', (e) => e.stopPropagation());
  layer.appendChild(bar);
  // 拖动平移：tx/ty 直接加减（无边界——小图/放大图都能自由拖回）；
  // 仅图片上按下才拖动；阻 img 原生 drag
  const dragMove = (e) => {
    if (e.target !== im) return; // 只在图片上开始拖
    e.preventDefault();
    let lastX = e.clientX, lastY = e.clientY;
    im.style.cursor = 'grabbing';
    const onMove = (ev) => {
      tx += ev.clientX - lastX;
      ty += ev.clientY - lastY;
      lastX = ev.clientX;
      lastY = ev.clientY;
      apply();
    };
    const onUp = () => {
      im.style.cursor = 'grab';
      window.removeEventListener('mousemove', onMove);
      window.removeEventListener('mouseup', onUp);
    };
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
  };
  layer.addEventListener('mousedown', dragMove);
  history.pushState({ __mediaLayer: true }, '');
  const onPop = () => close(true);
  const onKey = (e) => {
    if (e.key === 'Escape') close();
  };
  let closed = false;
  function close(silent) {
    if (closed) return;
    closed = true;
    window.removeEventListener('keydown', onKey);
    window.removeEventListener('popstate', onPop);
    if (!silent && history.state?.__mediaLayer) history.back(); // 消化哨兵
    layer.remove();
  }
  // 点图片外（层空白）关闭
  layer.addEventListener('click', (e) => {
    if (e.target === layer) close();
  });
  window.addEventListener('popstate', onPop);
  window.addEventListener('keydown', onKey);
  document.body.appendChild(layer);
}

// ===== 表格全屏展示（仅超宽表格 + 独占全屏 + 样式复用）=====
document.querySelectorAll('.coral-content table').forEach((table) => {
  const wrap = document.createElement('div');
  wrap.className = 'table-wrap';
  table.parentNode.insertBefore(wrap, table);
  wrap.appendChild(table);
  const zoomBtn = document.createElement('button');
  zoomBtn.className = 'table-zoom';
  zoomBtn.title = '全屏查看';
  const ICON_EXPAND =
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15 3h6v6"/><path d="M9 21H3v-6"/><path d="M21 3l-7 7"/><path d="M3 21l7-7"/></svg>';
  const ICON_COLLAPSE =
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 9V3h-6"/><path d="M3 15v6h6"/><path d="M14 10l7-7"/><path d="M10 14l-7 7"/></svg>';
  zoomBtn.innerHTML = ICON_EXPAND;
  zoomBtn.addEventListener('click', () => openTableLayer(table, zoomBtn, ICON_EXPAND, ICON_COLLAPSE), { once: true });
  wrap.appendChild(zoomBtn);
  // 仅显示不全（横向溢出）的表格显示放大按钮；
  // 窗口变化时复判
  const check = () => {
    const overflow = table.scrollWidth > table.clientWidth + 1;
    wrap.classList.toggle('has-overflow', overflow);
  };
  check();
  window.addEventListener('resize', check);
});

function openTableLayer(table, zoomBtn, ICON_EXPAND, ICON_COLLAPSE) {
  const layer = document.createElement('div');
  layer.className = 'media-layer table-layer';
  const inner = document.createElement('div');
  inner.className = 'media-layer-inner table-layer-inner';
  inner.classList.add('coral-content');
  const clone = table.cloneNode(true);
  clone.style.maxWidth = 'none';
  clone.style.display = 'table';
  clone.style.transform = 'none';
  // 跟手拖列宽的关键：table-layout fixed + 显式
  // colgroup 宽度——auto 布局下浏览器按内容重分配列宽，拖动被其他列
  // 抢占（不跟手）。fixed 下每列宽度完全由 col 元素决定，拖谁动谁。
  const cols = clone.querySelectorAll('th').length;
  const cg = document.createElement('colgroup');
  const colEls = [];
  for (let i = 0; i < cols; i++) {
    const col = document.createElement('col');
    cg.appendChild(col);
    colEls.push(col);
  }
  clone.insertBefore(cg, clone.firstChild);
  // 初始宽度 = 各列自然宽（拷贝当前渲染宽度）
  const srcThs = [...table.querySelectorAll('thead th')];
  srcThs.forEach((th, i) => {
    const w = Math.round(th.getBoundingClientRect().width) || 100;
    colEls[i].style.width = `${w}px`;
  });
  clone.style.width = 'fit-content'; // 表宽 = 各 col 之和（撑开滚动区）
  clone.style.tableLayout = 'fixed';
  inner.appendChild(clone);
  layer.appendChild(inner);
  // 层内右上角常驻关闭按钮（fixed 不随滚动）
  const closeBtn = document.createElement('button');
  closeBtn.className = 'media-close';
  closeBtn.title = '关闭全屏（Esc）';
  closeBtn.innerHTML =
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 9V3h-6"/><path d="M3 15v6h6"/><path d="M14 10l7-7"/><path d="M10 14l-7 7"/></svg>';
  closeBtn.addEventListener('click', close);
  layer.appendChild(closeBtn);
  // 列拖柄：不限于表头——表中任意行的字段
  // 边界都可拖，拖任何一个都改本列 col（fixed 布局唯一权威宽度）
  const colCells = (colIdx) => [
    ...clone.querySelectorAll(`thead th:nth-child(${colIdx + 1})`),
    ...[...clone.querySelectorAll('tbody tr')].map((tr) => tr.children[colIdx]),
  ];
  const attachResizer = (cell, colIdx) => {
    const handle = document.createElement('div');
    handle.className = 'col-resizer';
    cell.style.position = 'relative';
    cell.style.whiteSpace = 'normal'; // 内容超宽自动换行（继承 col 宽）
    cell.appendChild(handle);
    // hover 某列任意单元格 → 整列从头到尾显示边界线（拖柄控制全列宽，
    // 高亮应表达"整列被选中"）
    cell.addEventListener('mouseenter', () => {
      for (const c of colCells(colIdx)) c.classList.add('col-active');
    });
    cell.addEventListener('mouseleave', () => {
      for (const c of colCells(colIdx)) c.classList.remove('col-active');
    });
    const col = colEls[colIdx];
    handle.addEventListener('mousedown', (e) => {
      e.preventDefault();
      e.stopPropagation();
      const startX = e.clientX;
      const startW = parseFloat(col.style.width) || col.getBoundingClientRect().width;
      const onMove = (ev) => {
        // 只改本列 col：其他列不受影响（fixed 布局保证跟手）
        col.style.width = `${Math.max(40, startW + ev.clientX - startX)}px`;
      };
      const onUp = () => {
        window.removeEventListener('mousemove', onMove);
        window.removeEventListener('mouseup', onUp);
      };
      window.addEventListener('mousemove', onMove);
      window.addEventListener('mouseup', onUp);
    });
    handle.addEventListener('dblclick', (e) => {
      e.stopPropagation();
      // 还原该列初始自然宽
      const srcTh = srcThs[colIdx];
      const w = Math.round(srcTh.getBoundingClientRect().width) || 100;
      col.style.width = `${w}px`;
    });
  };
  clone.querySelectorAll('thead th').forEach((th, i) => attachResizer(th, i));
  clone.querySelectorAll('tbody tr').forEach((tr) => {
    [...tr.children].forEach((td, i) => attachResizer(td, i));
  });
  // 浏览器回退（侧键/手势）= 关闭图层而非离开页面：
  // 开层时压入哨兵 history state；用户回退触发 popstate → 只关层；
  // Esc/按钮关闭 → back() 消化哨兵（页面停留原地）
  history.pushState({ __mediaLayer: true }, '');
  const onPop = () => close(true);
  const onKey = (e) => {
    if (e.key === 'Escape') close();
  };
  let closed = false;
  function close(silent) {
    if (closed) return;
    closed = true;
    window.removeEventListener('keydown', onKey);
    window.removeEventListener('popstate', onPop);
    if (!silent && history.state?.__mediaLayer) history.back(); // 消化哨兵
    layer.remove();
    // 恢复正文按钮的打开行为与图标
    zoomBtn.innerHTML = ICON_EXPAND;
    zoomBtn.title = '全屏查看';
    zoomBtn.onclick = null;
    zoomBtn.addEventListener('click', () => openTableLayer(table, zoomBtn, ICON_EXPAND, ICON_COLLAPSE), { once: true });
  }
  window.addEventListener('popstate', onPop);
  window.addEventListener('keydown', onKey);
  zoomBtn.innerHTML = ICON_COLLAPSE;
  zoomBtn.title = '关闭全屏';
  document.body.appendChild(layer);
}


// ===== 搜索：下拉即时搜索 + Enter 全量结果页 =====
const searchInput = document.getElementById('search-input');
const searchDropdown = document.getElementById('search-dropdown');
if (searchInput && searchDropdown) {
  let debounceTimer = null;
  let activeIndex = -1;

  const hideDropdown = () => {
    searchDropdown.hidden = true;
    activeIndex = -1;
  };

  const renderDropdown = (hits, tokens) => {
    searchDropdown.innerHTML = '';
    if (hits.length === 0) {
      const empty = document.createElement('div');
      empty.className = 'search-empty';
      empty.textContent = '无匹配结果';
      searchDropdown.appendChild(empty);
    } else {
      hits.forEach((h) => {
        const a = document.createElement('a');
        a.className = 'search-result-item';
        // 详情页关键词高亮：tokens 编码进 ?hl=（与结果页链接同口径）
        a.href = tokens && tokens.length > 0
          ? h.url + '?hl=' + tokens.map(encodeURIComponent).join(',')
          : h.url;
        const title = document.createElement('div');
        title.className = 'search-result-title';
        title.textContent = h.title;
        const path = document.createElement('div');
        path.className = 'search-result-path';
        path.textContent = h.dir_path;
        const snip = document.createElement('div');
        snip.className = 'search-result-snippet';
        snip.innerHTML = h.snippet; // 服务端 SnippetGenerator 产出（转义+mark）
        a.append(title, path, snip);
        searchDropdown.appendChild(a);
      });
    }
    searchDropdown.hidden = false;
  };

  searchInput.addEventListener('input', () => {
    clearTimeout(debounceTimer);
    const q = searchInput.value.trim();
    if (q.length === 0) {
      hideDropdown();
      return;
    }
    debounceTimer = setTimeout(async () => {
      try {
        const res = await fetch('/api/search?q=' + encodeURIComponent(q) + '&limit=8');
        if (!res.ok) return;
        const data = await res.json();
        if (!data.enabled) return;
        renderDropdown(data.hits || [], data.tokens || []);
      } catch { /* 网络错误静默：下拉不弹出 */ }
    }, 300);
  });

  searchInput.addEventListener('keydown', (e) => {
    const items = searchDropdown.querySelectorAll('.search-result-item');
    if (e.key === 'Escape') {
      hideDropdown();
      return;
    }
    if (e.key === 'ArrowDown' && !searchDropdown.hidden && items.length > 0) {
      e.preventDefault();
      activeIndex = (activeIndex + 1) % items.length;
      items.forEach((x, i) => x.classList.toggle('active', i === activeIndex));
      items[activeIndex].scrollIntoView({ block: 'nearest' });
      return;
    }
    if (e.key === 'ArrowUp' && !searchDropdown.hidden && items.length > 0) {
      e.preventDefault();
      activeIndex = activeIndex <= 0 ? items.length - 1 : activeIndex - 1;
      items.forEach((x, i) => x.classList.toggle('active', i === activeIndex));
      items[activeIndex].scrollIntoView({ block: 'nearest' });
      return;
    }
    if (e.key === 'Enter') {
      const q = searchInput.value.trim();
      if (activeIndex >= 0 && !searchDropdown.hidden && items[activeIndex]) {
        location.href = items[activeIndex].href; // 键盘选中项优先
      } else if (q.length > 0) {
        location.href = '/search?q=' + encodeURIComponent(q);
      }
    }
  });

  document.addEventListener('click', (e) => {
    if (!e.target.closest('#search-box')) hideDropdown();
  });
}


// ===== Mermaid / KaTeX 渲染（M2-s3：服务端语义占位，此处 CDN 按需加载渲染） =====
const renderCfgMeta = document.getElementById('coral-render-cfg');
if (renderCfgMeta) {
  const mermaidCdn = renderCfgMeta.dataset.mermaidCdn;
  const katexCdn = renderCfgMeta.dataset.katexCdn;

  // Mermaid：pre.mermaid 官方识别形态；ESM 动态 import。
  // 失败降级：占位内已是转义原文（可读），仅打 console 警告
  const mermaidBlocks = document.querySelectorAll('pre.mermaid');
  if (mermaidBlocks.length > 0 && mermaidCdn) {
    import(mermaidCdn)
      .then((mod) => mod.default.initialize({ startOnLoad: false, securityLevel: 'strict' }))
      .then((m) => m.run({ nodes: mermaidBlocks }))
      .catch((e) => console.warn('mermaid 加载失败，保留原文显示', e));
  }

  // KaTeX：span[data-formula] 逐个 render；strict 模式公式错误显红色错误信息
  const mathNodes = document.querySelectorAll('.katex-block[data-formula], .katex-inline[data-formula]');
  if (mathNodes.length > 0 && katexCdn) {
    const script = document.createElement('script');
    script.src = katexCdn;
    script.onload = () => {
      mathNodes.forEach((el) => {
        try {
          katex.render(el.dataset.formula, el, {
            displayMode: el.classList.contains('katex-block'),
            throwOnError: false, // 公式错误时 KaTeX 显示红色错误而非中断
          });
        } catch (e) {
          console.warn('KaTeX 渲染失败，保留原文', el.dataset.formula, e);
        }
      });
    };
    script.onerror = () => console.warn('KaTeX 加载失败，保留原文显示');
    document.head.appendChild(script);
  }
}
