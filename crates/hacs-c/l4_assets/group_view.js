
/* ⚠️ 状态声明必须放脚本最前面：文件后面的 boot() 会同步走到访问它们的路径，
   声明在后 → 「Cannot access 'REFPOP' before initialization」→ 页面启动失败、
   聊天记录永远加载不出来。 */
const MSGCACHE = new Map();      // 键 = "<群 id>#<seq>" -> 消息对象（pull() 时顺手写）
const REFPOP = new Map();        // seq -> {el, seq, sid, x, y, pinned}
const REFREST = new Set();       // 已还原过「固定浮窗」的群（轮询/重渲染不重复还原）
const LS_REFPINS = "group.refpins";
let REFSID = "";
let REFZ = 70;
let REFSRC = "";                     // 最近一次取正文实际走的来源（single / list），便于真机实测取证

// —— 左栏 TAB 状态（必须在任何访问之前初始化，否则顶部同步路径踩 TDZ）——
const ATABS = ["groups", "queue"];
const TQSTATUS = { open: "待办", doing: "进行中", done: "已完成" };
const TQCLS = { open: "pending", doing: "running", done: "done" };   // 沿用既有 .tqs 三档配色
const TQRANK = { doing: 0, open: 1, done: 2 };                       // 在办的在上、完成的在下
let ATAB = "groups";
"use strict";
const $ = (id) => document.getElementById(id);
let WHO = null, MYID = "", AGENTS = [], GROUPS = [], CUR = null, CURSOR = 0, TIMER = null;
const SEEN = new Set();   // 已渲染过的 message_id：轮询只追加新的，防重复
const LS_LAST = "group.last_sid";   // 记住上次看的群，刷新后回到原处

const esc = (s) => String(s == null ? "" : s)
  .replace(/[&<>"]/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
const mid = (a) => (a && typeof a === "object") ? (a.id || "") : (a || "");
const mname = (a) => (a && typeof a === "object") ? (a.display || a.id || "") : (a || "");

// ================= #418-3 左栏「我的群」：向左折叠 + 分隔条左右拖拽调宽 =================
// 折叠：左栏占 0 宽，聊天区 + 输入区吃满整页；拖拽：拉分隔条调宽；两者都记 localStorage。
// 另：#418-2 输入区右边界跟随消息区滚动条宽度（--hx-sb），左右都与 #log 内容盒对齐。
const LS_AW = "group.aside_w", LS_AC = "group.aside_collapsed";
const ASIDE_MIN = 110, ASIDE_MAX = 560;

function asideDefaultW() { return (window.innerWidth <= 640) ? 118 : 290; }

function asideReadW() {
  let v = NaN;
  try { v = parseInt(localStorage.getItem(LS_AW) || "", 10); } catch (e) { v = NaN; }
  if (!v || isNaN(v)) v = asideDefaultW();
  return Math.min(ASIDE_MAX, Math.max(ASIDE_MIN, v));
}

function asideReadCollapsed() {
  try { return localStorage.getItem(LS_AC) === "1"; } catch (e) { return false; }
}

let ASIDE_W = asideReadW(), ASIDE_C = asideReadCollapsed();

// 输入区右内边距 = 14px + 消息区滚动条宽（滚动条出现/消失都跟着走）
function composerSyncSb() {
  const lg = $("log"); if (!lg) return;
  const sb = Math.max(0, lg.offsetWidth - lg.clientWidth);
  document.documentElement.style.setProperty("--hx-sb", sb + "px");
}

function asideApply() {
  const root = document.documentElement;
  root.style.setProperty("--hx-aside-w", (ASIDE_C ? 0 : ASIDE_W) + "px");
  root.classList.toggle("aside-collapsed", !!ASIDE_C);
  const chev = $("splitchev");
  if (chev) { chev.textContent = ASIDE_C ? "▶" : "◀"; chev.title = ASIDE_C ? "展开「我的群」面板" : "折叠「我的群」面板"; }
  composerSyncSb();
}

function asideSave() {
  try {
    localStorage.setItem(LS_AW, String(ASIDE_W));
    localStorage.setItem(LS_AC, ASIDE_C ? "1" : "0");
  } catch (e) { /* 无痕模式就算了 */ }
}

function asideToggle() { ASIDE_C = !ASIDE_C; asideApply(); asideSave(); }
function asideReset() { ASIDE_W = asideDefaultW(); asideApply(); asideSave(); }

asideApply();

(function asideDrag() {
  const sp = $("split");
  if (!sp) return;
  let dragging = false;
  sp.addEventListener("mousedown", (e) => {
    if (e.target && e.target.id === "splitchev") return;    // 点箭头 = 切换折叠，不算拖拽
    dragging = true; sp.classList.add("drag");
    document.body.style.userSelect = "none";
    e.preventDefault();
  });
  window.addEventListener("mousemove", (e) => {
    if (!dragging) return;
    const main = document.querySelector("main");
    const left = main ? main.getBoundingClientRect().left : 0;
    const w = e.clientX - left;
    if (ASIDE_C && w > 60) ASIDE_C = false;                 // 从折叠态往右拉 = 重新展开
    ASIDE_W = Math.min(ASIDE_MAX, Math.max(ASIDE_MIN, Math.round(w)));
    asideApply();
  });
  window.addEventListener("mouseup", () => {
    if (!dragging) return;
    dragging = false; sp.classList.remove("drag");
    document.body.style.userSelect = "";
    asideSave();
  });
  sp.addEventListener("dblclick", asideReset);
})();

window.addEventListener("resize", composerSyncSb);

// ================= #438-2：输入框高度记忆（刷新后仍是上次拖拽的高度）=================
// #347-0② 让 textarea 可垂直拖拽（resize:vertical），但高度只活在内存里，刷新就回 38px。
// 这里把「拖完的高度」写进 localStorage，加载时再写回 inline style.height ——
//   ① 与原生 resize 手柄写的是同一个属性（都改 textarea.style.height / 盒高），所以同一条路；
//   ② 上下限跟 CSS 对齐：min 38px（=min-height）/ max 60vh（=max-height），恢复时同样夹紧。
const LS_TH = "group.text_h";
const TEXT_MIN_H = 38;
let TEXT_H_APPLY = false;          // 正在「恢复」而不是「用户在拖」：跳过采集，免得自己写自己

function textMaxH() {
  return Math.max(TEXT_MIN_H, Math.round((window.innerHeight || 800) * 0.6));   // = CSS max-height:60vh
}
function textReadH() {
  let v = NaN;
  try { v = parseInt(localStorage.getItem(LS_TH) || "", 10); } catch (e) { v = NaN; }
  if (!v || isNaN(v)) return null;                                              // 没存过 → 不动，保持默认 38px
  return Math.min(textMaxH(), Math.max(TEXT_MIN_H, v));
}
function textApplyH() {
  const t = $("text"); if (!t) return null;
  const h = textReadH(); if (h == null) return null;
  TEXT_H_APPLY = true;
  t.style.height = h + "px";
  TEXT_H_APPLY = false;
  return h;
}
function textSaveH() {
  const t = $("text"); if (!t) return null;
  const h = Math.round(t.getBoundingClientRect().height);
  if (!h || h < TEXT_MIN_H) return null;
  try { localStorage.setItem(LS_TH, String(h)); } catch (e) { /* 无痕模式就算了 */ }
  return h;
}
textApplyH();                      // 页面加载（脚本在 body 末尾，DOM 已就绪）就恢复
if (window.ResizeObserver) {       // 拖拽 / 字体加载 / 窗口缩放等任何盒高变化都能被看见
  try { new ResizeObserver(() => { if (!TEXT_H_APPLY) textSaveH(); }).observe($("text")); } catch (e) {}
}
window.addEventListener("mouseup", () => { if (!TEXT_H_APPLY) textSaveH(); });   // 拖完补一刀

// ================= #438-3：消息区按发言者过滤 =================
// 规则：勾中谁就只看谁的发言（可多选）；一个都不勾 = 全显（后端返回多少就显示多少）。
// 实现：消息照常全量渲染 / 轮询追加，只切 .msg 的 display —— 不动 #log 的滚动容器角色，
//       也不改 pull() 的「贴底跟随」逻辑（#347-0①）。
const FILTER = new Set();          // 勾中的发言者 id；空集 = 全显
let FILTER_SIG = null;             // 已渲染 chip 的签名（发言者集合 + 勾选态），没变就不重建 DOM

function applyMsgFilter() {
  const log = $("log"); if (!log) return { shown: 0, total: 0 };
  let shown = 0, total = 0;
  log.querySelectorAll(".msg").forEach(el => {
    const a = el.dataset.author || "";
    const vis = (!FILTER.size || FILTER.has(a));
    el.style.display = vis ? "" : "none";
    total++; if (vis) shown++;
  });
  // #460：勾了发言者过滤时，顺带按主题组重算「组内还剩几条可见」；全显时恢复真实条数
  log.querySelectorAll(".tgrp").forEach(g => {
    const c = g.querySelector(".tgc"); if (!c) return;
    const all = g.querySelectorAll(".tgb .msg").length;
    if (!FILTER.size) { c.textContent = all + " 条"; return; }
    let v = 0;
    g.querySelectorAll(".tgb .msg").forEach(m => { if (m.style.display !== "none") v++; });
    c.textContent = v + "/" + all + " 条";
  });
  const c = document.querySelector("#msgfilter .fcount");
  if (c) c.textContent = FILTER.size ? ("显示 " + shown + "/" + total + " 条") : "";
  return { shown: shown, total: total };
}

function renderMsgFilter(force) {
  const box = $("msgfilter"), log = $("log");
  if (!box || !log) return;
  const seen = new Map();                                  // 按「首次出现顺序」记发言者
  log.querySelectorAll(".msg").forEach(el => {
    const a = el.dataset.author || ""; if (!a || seen.has(a)) return;
    seen.set(a, el.dataset.authorname || a);
  });  const sig = Array.from(seen.keys()).join("|") + "#" + Array.from(FILTER).sort().join(",");
  if (!force && sig === FILTER_SIG) return;                 // 发言者没新增 → 不重建（免得 3 秒一刷）
  FILTER_SIG = sig;
  if (!seen.size) { box.hidden = true; box.innerHTML = ""; return; }   // 没人可筛 → 整条不渲染
  box.hidden = false;
  box.innerHTML = "";
  const lab = document.createElement("span");
  lab.className = "flabel"; lab.textContent = "只看：";
  box.appendChild(lab);
  seen.forEach((name, id) => {
    const c = document.createElement("span");
    c.className = "fchip" + (FILTER.has(id) ? " on" : "");  // 独立类名：与底栏 .chip 不混用
    c.dataset.m = id; c.textContent = name;
    c.title = "只看 " + name + " 的发言（可多选）；一个都不勾 = 全显";
    c.onclick = () => {
      if (FILTER.has(id)) FILTER.delete(id); else FILTER.add(id);
      renderMsgFilter(true); applyMsgFilter();
    };
    box.appendChild(c);
  });
  const clr = document.createElement("span");
  clr.className = "fchip" + (FILTER.size ? "" : " on");
  clr.dataset.clear = "1"; clr.textContent = "全显";
  clr.title = "取消全部勾选，恢复显示所有发言";
  clr.onclick = () => { FILTER.clear(); renderMsgFilter(true); applyMsgFilter(); };
  box.appendChild(clr);
  const cnt = document.createElement("span");
  cnt.className = "fcount";
  box.appendChild(cnt);
}

function filterReset() { FILTER.clear(); FILTER_SIG = null; renderMsgFilter(true); applyMsgFilter(); }

// 调试/自测句柄（headless 断言用；不影响页面行为）
window.__h438 = { FILTER, applyMsgFilter, renderMsgFilter, textSaveH, textApplyH, textMaxH, TEXT_MIN_H };

// ================= #460 主题分组（2026-09-24 刚哥需求 1）=================
// 规则：人发言 = 开一个新主题（他名下的那几条 Agent 回复 + Agent 互相讨论都归到该主题）；
//      开场那段人发言之前的消息归到「开场」组；之后的每条人发言又起一组。
// 每条消息用 data-topic 挂上组号，组头可折叠（折叠态按群记 localStorage）。
const LS_TOPICS = "group.topics_closed";   // {sid: ["0","2"]}
let TCOUNT = 0;          // 已经画出来的主题组数
let TOPIC_NEXT = 0;      // 下一条要开组的号
let LAST_AUTHOR = null;  // 上一条是谁说的（null = 还没有任何消息）
let LAST_SEQ = 0;
let LAST_TS = 0;         // 上一条时间戳（回溯开组时用来还原「何时起」）
let lastTid = -1;        // 上一条消息落入的主题组号（-1 = 还没开始）
// #460d：顶部「我还欠 N 条回应」的循环跳转状态
let PEND_LIST = [];      // 待办 seq 列表（按顺序）
let PEND_IDX = -1;       // 上次跳到了第几个（-1 = 还没跳过）
let PEND_KEY = "";       // 待办列表指纹（变了才重置 PEND_IDX，避免点击时被轮询重置）

function topicParseTs(s) {
  const t = Date.parse(String(s || "").replace(" ", "T"));
  return isNaN(t) ? 0 : t;
}

// ================= #460c 需求 2：待回应 / 已回应 =================
// 契约两种写法都吃：
//   a) 字符串数组（服务端算好的「谁没回」） → 全归 wait
//   b) 对象 {whom: true/false}（true=待回应，false=已回应）
//   c) 对象数组 [{id|agent|name, responded|done|answered|ok}]（容错）
function parseNeeds(raw) {
  const out = { wait: [], done: [] };
  if (!raw) return out;
  const push = (k, v) => {
    const s = String(v == null ? "" : v).trim();
    if (s && out[k].indexOf(s) < 0) out[k].push(s);
  };
  if (Array.isArray(raw)) {
    raw.forEach(x => {
      if (x && typeof x === "object") {
        const id = x.id || x.agent || x.name || x.who;
        const ok = x.responded || x.done || x.answered || x.ok || x.replied;
        push(ok ? "done" : "wait", id);
      } else push("wait", x);
    });
  } else if (typeof raw === "object") {
    Object.keys(raw).forEach(k => push(raw[k] ? "wait" : "done", k));
  }
  return out;
}

// 渲染成：「待回应：A、B 已回应：C」；两边都没有则不显示
function renderPendo(nr) {
  if (!nr) return "";
  const w = (nr.wait || []), d = (nr.done || []);
  let h = "";
  if (w.length) h += '<span class="w">待回应：' + esc(w.join("、")) + '</span>';
  if (d.length) h += (h ? '<span class="sep">·</span>' : '') + '<span class="d">已回应：' + esc(d.join("、")) + '</span>';
  return h ? '<span class="pendo">' + h + '</span>' : "";
}

// 全场重算：一条「等人回应」的消息，若其后有该人的发言（且晚于本条）→ 从待回应移到已回应。
// 同时刷新每条消息旁的 .pendo 与所属主题组头的 .tgs（吸顶时也能看到状态）。
function refreshNeedStates(msgs) {
  const log = $("log"); if (!log) return;
  // 1) 收集「谁在什么时间说过话」——只看本轮拉到的消息
  const spoke = new Map();   // id -> 最大时间戳
  (msgs || []).forEach(m => {
    const id = mid(m.author) || "";
    if (!id) return;
    const t = topicParseTs(m.created_at);
    const cur = spoke.get(id) || 0;
    if (t > cur) spoke.set(id, t);
  });
  // 2) 逐条重算
  const byTopic = new Map();   // tid -> {wait:Set, done:Set}
  log.querySelectorAll(".tgb .msg").forEach(el => {
    let nr = { wait: [], done: [] };
    try { nr = JSON.parse(el.dataset.nr || '{"wait":[],"done":[]}') || nr; } catch (e) {}
    const ts = Number(el.dataset.ts || 0);
    const wait = (nr.wait || []).filter(Boolean);
    const done = (nr.done || []).slice();
    // 逐个检查「还在等的人」是否已经在本条之后说过话
    const stillWait = [];
    wait.forEach(who => {
      const t = spoke.get(who) || 0;
      if (t && t >= ts) { if (done.indexOf(who) < 0) done.push(who); }
      else stillWait.push(who);
    });
    const cur = { wait: stillWait, done: done };
    // 没变就不动 DOM（免重排）
    if (JSON.stringify(cur) !== JSON.stringify(nr)) {
      el.dataset.nr = JSON.stringify(cur);
      const h = el.querySelector(".h .pendo");
      if (h) { const nh = renderPendo(cur); if (nh) h.outerHTML = nh; else h.remove(); }
      else if (renderPendo(cur)) el.querySelector(".h").insertAdjacentHTML("beforeend", renderPendo(cur));
    }
    // 顺带累积到组维度（取并集，给吸顶组头用）
    const tid = el.dataset.topic || "";
    const g = byTopic.get(tid) || { wait: new Set(), done: new Set() };
    cur.wait.forEach(x => g.wait.add(x));
    cur.done.forEach(x => { if (!g.wait.has(x)) g.done.add(x); });
    byTopic.set(tid, g);
  });
  // 3) 写回组头状态
  log.querySelectorAll(".tgrp").forEach(g => {
    const s = byTopic.get(g.dataset.tid) || { wait: new Set(), done: new Set() };
    let tg = g.querySelector(".tgh .tgs");
    if (!tg) {
      g.querySelector(".tgh").insertAdjacentHTML("beforeend", '<span class="tgs"></span>');
      tg = g.querySelector(".tgh .tgs");
    }
    if (!tg) return;
    let h = "";
    if (s.wait.size) h += '<span class="w">待回应：' + esc(Array.from(s.wait).join("、")) + '</span>';
    if (s.done.size) h += (h ? '<span class="sep">·</span>' : '') + '<span class="d">已回应：' + esc(Array.from(s.done).join("、")) + '</span>';
    tg.innerHTML = h;
  });
}

function topicsClosedRead(sid) {
  try {
    const a = JSON.parse(localStorage.getItem(LS_TOPICS) || "{}") || {};
    const v = a[sid];
    return new Set(Array.isArray(v) ? v.map(String) : []);
  } catch (e) { return new Set(); }
}

function topicClosedWrite(sid, tid, closed) {
  try {
    const a = JSON.parse(localStorage.getItem(LS_TOPICS) || "{}") || {};
    const v = new Set(Array.isArray(a[sid]) ? a[sid].map(String) : []);
    if (closed) v.add(String(tid)); else v.delete(String(tid));
    a[sid] = Array.from(v);
    localStorage.setItem(LS_TOPICS, JSON.stringify(a));
  } catch (e) { /* 无痕模式就算了 */ }
}

function topicBody(tid) {
  const g = document.querySelector('.tgrp[data-tid="' + tid + '"]');
  return g ? g.querySelector(".tgb") : null;
}

function topicCardEnsure(tid, title, firstTs) {
  const log = $("log"); if (!log) return null;
  let g = log.querySelector('.tgrp[data-tid="' + tid + '"]');
  if (g) return g.querySelector(".tgb");
  g = document.createElement("div");
  g.className = "tgrp" + (String(tid) === String(TOPIC_NEXT - 1) ? " cur" : "");
  g.dataset.tid = String(tid);
  const closed = topicsClosedRead(CUR && CUR.session_id).has(String(tid));
  if (closed) g.classList.add("closed");
  const when = firstTs ? new Date(firstTs).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" }) : "";
  g.innerHTML =
    '<div class="tgh" title="点击折叠 / 展开本主题"><span class="tgcar">▼</span>' +
      '<span class="tgt"></span><span class="tgc"></span>' +
      '<span class="spacer"></span><span class="tga"></span></div>' +
    '<div class="tgb"></div>';
  g.querySelector(".tgt").textContent = title;
  g.querySelector(".tga").textContent = when;
  g.querySelector(".tgh").onclick = () => {
    const nowClosed = !g.classList.contains("closed");
    g.classList.toggle("closed", nowClosed);
    topicClosedWrite(CUR && CUR.session_id, tid, nowClosed);
    topicUpdateCount();
  };
  log.appendChild(g);
  TCOUNT++;
  topicUpdateCount();
  return g.querySelector(".tgb");
}

// 组头标题：人的发言取正文首行（截断），Agent 开的话题取「Agent 名 · 首行」
function topicTitle(msg, seq) {
  let t = String((msg && msg.text) || "").replace(/\s+/g, " ").trim();
  t = t.replace(/^[@#][^\s]*\s*/g, "").trim();
  if (!t) t = "（无正文）";
  if (t.length > 44) t = t.slice(0, 44) + "…";
  if (!msg.isHuman) t = mname(msg.author) + " · " + t;
  return "#" + seq + " " + t;
}

function topicCountRefresh() { /* 向后兼容壳：实际工作在 topicUpdateCount */ }

function topicUpdateCount() {
  const log = $("log"); if (!log) return;
  log.querySelectorAll(".tgrp").forEach(g => {
    const n = g.querySelectorAll(".tgb .msg").length;
    const c = g.querySelector(".tgc");
    if (c) c.textContent = n + " 条";
  });
}

// topicResolve(seq, isHuman, author): 返回该消息应落入的主题组号
function topicResolve(seq, isHuman) {
  // lastTid = 上一条消息落入的组；-1 表示「还没有上一条」
  if (!TCOUNT || isHuman || lastTid < 0) {
    const tid = TOPIC_NEXT++;
    lastTid = tid;
    LAST_AUTHOR = "\u0000topic" + tid;
    return tid;
  }
  return lastTid;
}

// 首次整拉（inc=false）时从零重算主题归属
function topicReset() { TCOUNT = 0; TOPIC_NEXT = 0; LAST_AUTHOR = null; LAST_SEQ = 0; LAST_TS = 0; lastTid = -1; PEND_LIST = []; PEND_IDX = -1; PEND_KEY = ""; }

function logReset() {
  const log = $("log"); if (log) log.innerHTML = "";
  topicReset();
}

// 调试句柄（无头断言用）
window.__h460 = { topicResolve: topicResolve, topicReset: topicReset, topicUpdateCount: topicUpdateCount };

// #presence：顶部 Agent 在线态条。
//   口径：online===true 在线；false 离线（灰显）；null/undefined = 未知（还没探到，不谎报离线）。
//   数据源 /api/agents，30s 自刷新一次（服务端按节点 a2a 探活，探测在它自己的后台线程里）。
function renderAgentBar() {
  const box = $("ablist"); if (!box) return;
  if (!AGENTS.length) {
    box.innerHTML = '<span class="note">没取到 Agent 名册（/api/agents）。</span>';
    return;
  }
  const rank = a => (a.online === true ? 0 : (a.online === false ? 1 : 2));
  const list = AGENTS.slice().sort((a, b) =>
    rank(a) - rank(b) || String(mid(a)).localeCompare(String(mid(b))));
  const onN = list.filter(a => a.online === true).length;
  box.innerHTML = list.map(a => {
    const on = a.online === true, off = a.online === false;
    const cls = on ? "on" : (off ? "off" : "unk");
    const tag = on ? "在线" : (off ? "离线" : "未知");
    const m = a.meta || {};
    const tip = [a.id || "", m.node_id ? "节点 " + m.node_id : "",
                 m.host ? "(" + m.host + ")" : "", m.model ? "模型 " + m.model : "",
                 a.last_hb_at ? "心跳 " + a.last_hb_at : "", tag].filter(Boolean).join(" · ");
    return '<span class="abchip ' + cls + '" title="' + esc(tip) + '" data-a="' + esc(mid(a)) + '">' +
           '<i class="dot"></i>' + esc(a.display || a.id || mid(a)) + '<em>' + tag + '</em></span>';
  }).join("") + '<span class="absum">在线 ' + onN + ' / ' + list.length + '</span>';
}

async function refreshAgents() {
  try {
    const d = await api("GET", "/api/agents");
    if (d && d.agents) AGENTS = d.agents;
  } catch (e) { /* 取不到就沿用上一次名册（顶部不闪成空） */ }
  renderAgentBar();
}

async function boot() {
  try {
    WHO = await api("GET", "/api/whoami");
    const u = (WHO && WHO.user) || {};
    $("who").innerHTML = u.role === "owner"
      ? '身份：<b>' + esc(u.role_cn || u.role) + '</b>（' + esc(u.source || "") + ' 免登录）'
      : '身份：<b>' + esc(u.display || u.name || "?") + '</b>';
  } catch (e) { $("who").textContent = "身份读取失败：" + e.message; }
  await refreshAgents();     // #presence：名册 + 在线态条一起到位
  await loadGroups();
  // 刷新后回到上次看的那个群（否则刷完一片空白，看着像"聊天记录丢了"）
  let last = null;
  try { last = localStorage.getItem(LS_LAST); } catch (e) { last = null; }
  const pick = GROUPS.find(s => s.session_id === last) ||
               GROUPS.find(s => s.state === "open") || GROUPS[0];
  if (pick) await openGroup(pick);
}

async function loadGroups() {
  const d = await api("GET", "/api/sessions");
  GROUPS = ((d && d.sessions) || []).filter(s => (s.kind || "") === "meet");
  const box = $("groups"); box.innerHTML = "";
  if (!GROUPS.length) { box.innerHTML = '<div class="note">还没有群。点右上「＋ 建群」开一个。</div>'; return; }
  GROUPS.forEach(s => {
    const ids = (s.members || []).map(mid);
    const el = document.createElement("div");
    el.className = "gitem" + (CUR && CUR.session_id === s.session_id ? " on" : "");
    el.innerHTML = '<div class="t">' + esc(s.title || s.session_id) +
      '<span class="tag ' + esc(s.state) + '">' + esc(s.state === "open" ? "进行中" : "已散会") + '</span></div>' +
      '<div class="m">' + esc(s.session_id) + (ids.length ? " · " + esc(ids.join(",")) : "") + '</div>';
    el.onclick = () => openGroup(s);
    box.appendChild(el);
  });
}

async function openGroup(s) {
  CUR = s; CURSOR = 0; SEEN.clear();
  try { localStorage.setItem(LS_LAST, s.session_id); } catch (e) { /* 无痕模式就算了 */ }
  $("gtitle").textContent = s.title || s.session_id;
  const ids = (s.members || []).map(mid);
  $("gmeta").textContent = s.session_id + (ids.length ? " · " + ids.join(", ") : "");
  const open = s.state === "open";
  $("send").disabled = !open;
  $("closebtn").disabled = !(open && (!MYID || mid(MYID) === "" || s.created_by === MYID));
  const ab = $("addagent"); if (ab) ab.disabled = !open;   // #addagent：归档群不能加人
  $("log").innerHTML = '<div class="loading"><span class="spin"></span>正在加载聊天记录…</div>';
  topicReset();
  acClose();
  renderMentions(ids);
  filterReset();           // #438-3：切群 = 发言者全换了，过滤勾选清空（回到「全显」）
  refPopsReset(true);      // #476：换群 —— 旧群的引用浮窗全部收起（固定记录也复位，见下）
  await loadGroups();
  await pull();
  // #460e：拉完若一条都没有，明确告知「本群暂无消息」而不是留一片空白
  (function () {
    const lg = $("log"); if (!lg) return;
    if (!lg.querySelector(".msg") && !lg.querySelector(".tgrp")) {
      const empty = CUR && CUR.state === "archived"
        ? '<span class="ic">📁</span>本群已散会，且没有聊天记录。'
        : '<span class="ic">💬</span>本群暂无消息——在下方输入框说点什么吧。';
      lg.innerHTML = '<div class="emptybox">' + empty + '</div>';
    }
  })();
  refPopsRestore(s.session_id);   // #476：本群「固定过」的引用浮窗回到原位（刷新/切群都还原）
  if (TIMER) clearInterval(TIMER);
  TIMER = setInterval(() => {
    pull(true).catch(() => {});
    // #448：队列 TAB 开着时跟着轮询，状态（待办/进行中/已完成）自己在页面上刷新
    if (ATAB === "queue") tqLoad();
  }, 3000);
  if (ATAB === "queue") tqLoad();     // #432-3：切群时「任务队列」TAB 跟着换成新群的任务
}

function renderMentions(ids) {
  // #438-1：底栏「发送」按钮右边那排成员快捷标签（@所有人 / @<human-id> / @hermes / @openclaw-nb）
  //   已整排下线：这里不再建任何 chip，#mentions 恒为空盒子（CSS 里再 display:none 兜一层，
  //   所以它 0×0、无文本、不占 flex 位置 —— 发送按钮与输入框的底边对齐不受影响）。
  //   选人方式不变：在正文里手打 @xxx（配输入框联想 #ac），sel() 的 textMentions() 照常解析；
  //   不写 @ 就是默认 @所有人（见 updHint()）。
  const box = $("mentions");
  if (box) { box.innerHTML = ""; box.hidden = true; }
  void ids;
}

// 点完 @ 把光标还给输入框（并落在末尾），手不用来回挪
function focusText() {
  const t = $("text");
  if (!t) return;
  try { t.focus(); t.setSelectionRange(t.value.length, t.value.length); } catch (e) { t.focus(); }
}

// #432-2：点右侧的 agent chip（如 @hermes）= 在【当前光标处】插入 "@hermes "。
//   用 textarea 的 selectionStart/selectionEnd 切原串（有选区就替换选区），
//   不追加到末尾、不把光标挤回队尾；插完光标落在 @xxx 之后并 focus 回输入框。
//   insertAtCursor 是纯函数式的取值-改值-设光标，不触发 input 事件（免得联想浮层乱弹）。
function insertAtCursor(ins) {
  const t = $("text"); if (!t) return null;
  const v = String(t.value == null ? "" : t.value);
  let s = (t.selectionStart == null) ? v.length : t.selectionStart;
  let e = (t.selectionEnd == null) ? s : t.selectionEnd;
  s = Math.max(0, Math.min(s, v.length));
  e = Math.max(s, Math.min(e, v.length));
  // 后面紧跟的字符已经是空白就不再补空格，避免插出两个空格
  const tail = /^\s/.test(v.slice(e)) ? "" : " ";
  const piece = ins + tail;
  t.value = v.slice(0, s) + piece + v.slice(e);
  const np = s + piece.length;                 // 光标落在 @xxx + 尾空格之后
  try { t.setSelectionRange(np, np); } catch (err) {}
  try { t.focus(); } catch (err) {}
  updHint();
  return { before: v, at: s, inserted: piece, after: t.value, caret: np };
}

// 同一个 @ 词已经在正文里就不再插（连点 chip 不会插出两个 @hermes）
function insertMention(id) {
  const t = $("text"); if (!t) return null;
  const tok = "@" + id;
  const re = new RegExp(tok.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + "(?![\\w\\u4e00-\\u9fa5-])");
  if (re.test(String(t.value || ""))) { updHint(); try { t.focus(); } catch (e) {} return null; }
  return insertAtCursor(tok);
}

// 输入框里手打 @openclaw-nb / @所有人 —— 等同于点下面的 @ 栏
function textMentions() {
  const t = $("text").value || "";
  const out = [];
  const re = /@([A-Za-z0-9_\u4e00-\u9fa5\-]+)/g;
  let m; while ((m = re.exec(t)) !== null) {
    const n = m[1];
    if (n === "所有人" || n === "all" || n === "全员") out.push("@all");
    else out.push(n);
  }
  return out;
}

function groupMemberIds() { return ((CUR && CUR.members) || []).map(mid).filter(Boolean); }

// ---------- 输入 @ 时的联想（只列本群能对话的成员） ----------
const AC = { open: false, items: [], idx: 0, start: -1 };

function acCandidates(prefix) {
  const ids = groupMemberIds().filter(id => id !== MYID);
  let list = [{ id: "@all", label: "@所有人", sub: "全群" }].concat(
    ids.map(id => {
      const a = AGENTS.find(x => mid(x) === id) || {};
      return { id, label: "@" + id, sub: mname(a) === id ? "" : mname(a) };
    }));
  const p = String(prefix || "").toLowerCase();
  if (!p) return list;
  return list.filter(x => x.label.toLowerCase().indexOf(p) >= 0 || String(x.sub).toLowerCase().indexOf(p) >= 0);
}

function acClose() {
  AC.open = false; AC.items = []; AC.start = -1;
  const b = $("ac"); if (b) { b.innerHTML = ""; b.classList.remove("on"); }
}

function acRender() {
  const box = $("ac"); if (!box) return;
  box.innerHTML = "";
  if (!AC.items.length) { acClose(); return; }
  AC.items.forEach((it, i) => {
    const d = document.createElement("div");
    d.className = "it" + (i === AC.idx ? " sel" : "");
    d.innerHTML = esc(it.label) + (it.sub ? '<span class="sub">' + esc(it.sub) + '</span>' : "");
    d.onmousedown = (e) => { e.preventDefault(); acPick(i); };   // preventDefault：别让输入框失焦
    box.appendChild(d);
  });
  box.classList.add("on");
  AC.open = true;
  box.scrollTop = Math.max(0, AC.idx * 30 - 60);
}

// 看光标前是不是正在打 @xxx
function acCheck() {
  const t = $("text"); if (!t) return;
  const pos = (t.selectionStart == null) ? t.value.length : t.selectionStart;
  const m = /@([A-Za-z0-9_\u4e00-\u9fa5\-]*)$/.exec(t.value.slice(0, pos));
  if (!m) { acClose(); return; }
  AC.start = pos - m[1].length - 1;            // '@' 所在下标
  AC.items = acCandidates(m[1]); AC.idx = 0;
  if (AC.items.length) acRender(); else acClose();
}

function acPick(i) {
  const it = AC.items[i]; if (!it || AC.start < 0) return;
  const t = $("text");
  const cur = (t.selectionStart == null) ? t.value.length : t.selectionStart;
  const ins = "@" + (it.id === "@all" ? "所有人" : it.id) + " ";
  t.value = t.value.slice(0, AC.start) + ins + t.value.slice(cur);
  const np = AC.start + ins.length;
  try { t.setSelectionRange(np, np); } catch (e) {}
  acClose(); updHint(); t.focus();
}

function sel() {
  const out = [];
  document.querySelectorAll("#mentions .chip.on").forEach(c => {
    out.push(c.dataset.all ? "@all" : c.dataset.m);
  });
  const ids = groupMemberIds();
  // #539：正文手打的 @名字 按**大小写不敏感**回落到成员表原文再发。
  // 群里显示的就是 "Hermes" 这种写法，人照显示名打 @Hermes 是本能的；
  // 以前只做严格相等匹配 → "Hermes" 被丢掉 → mentions 空成 [] →
  // 后端只认正文解析，点名解析不出就整条钩子不触发（派活不入队）。
  // 多个成员仅大小写不同（有歧义）时不回落，保持原来的严格匹配。
  const canon = (n) => {
    const low = String(n).toLowerCase();
    const hit = ids.filter(i => String(i).toLowerCase() === low);
    return hit.length === 1 ? hit[0] : null;
  };
  textMentions().forEach(x => {
    if (x === "@all") out.push("@all");
    else if (ids.indexOf(x) >= 0) out.push(x);   // 只认群内成员，避免后端 422 unknown_mention
    else {
      const c = canon(x);
      if (c) out.push(c);
    }
  });
  return Array.from(new Set(out));
}
function updHint() {
  const s = sel();
  $("hint").textContent = s.length
    ? "将发给：" + s.map(x => x === "@all" ? "@所有人" : "@" + x).join("、")
    : "没选人 = @所有人（全群都要处理并回应）。";
}

// #347-0①：是否已贴底（24px 容差）。每次实时算，不缓存标志位。
function atBottom(el) {
  if (!el) return true;
  return (el.scrollHeight - el.scrollTop - el.clientHeight) <= 24;
}

async function pull(inc) {
  if (!CUR) return;
  const q = "?limit=200" + (inc && CURSOR ? "&after_seq=" + CURSOR : "");
  const d = await api("GET", "/api/sessions/" + encodeURIComponent(CUR.session_id) + "/messages" + q);
  if (d && d.me) MYID = d.me;
  const msgs = (d && d.messages) || [];
  const log = $("log");
  // #347-0①：先判贴底、再追加。首次/切群(!inc)视为贴底；其余按当前滚动位置实时算。
  const stick = !inc || atBottom(log);
  if (!inc) { logReset(); SEEN.clear(); }
  msgs.forEach(m => {
    const seq = m.seq || 0;
    const key = m.message_id || ("seq:" + seq);
    if (SEEN.has(key)) return;          // 同一条只渲染一次（后端重放/参数兜底都不怕）
    SEEN.add(key);
    if (seq) CURSOR = Math.max(CURSOR, seq);
    if (seq) MSGCACHE.set(msgKey(CUR && CUR.session_id, seq), m);   // #476：本地缓存，**按群 + seq** 存
    const meta = m.meta || {};
    // #460c 需求 2：待回应/已回应要按**实际回复动态更新**。契约两种可能：
    //   a) meta.needs_response 是字符串数组（旧写法，服务端已算好）
    //   b) meta.needs_response 是对象 {whom: true/false}（true=待回应，false=已回应）
    // 这里统一解析成 {wait:[], done:[]}，两种都能吃。
    const nr = parseNeeds(meta.needs_response);
    const need = nr.wait;                          // 「还在等谁」——只有真待回应的才标黄
    const aud = (meta.audience || []).map(x => x === "<human-id>" ? "刚哥(" + x + ")" : x);
    const mine = mid(m.author) === MYID;
    const isHuman = !(m.author && m.author.type === "agent");
    const tid = topicResolve(seq, isHuman, m.author);
    const el = document.createElement("div");
    el.className = "msg" + (mine ? " mine" : "") + (need.includes(MYID) ? " need" : "");
    // #438-3：发言者落到 data-* 上 —— 过滤条按它归组，勾选后切 display（不动渲染内容本身）
    el.dataset.author = mid(m.author) || "";
    el.dataset.authorname = mname(m.author) || "";
    el.dataset.seq = String(seq);           // #476：这条消息自己的编号
    el.dataset.topic = String(tid);         // #460：这条消息属于哪个主题组
    el.dataset.nr = JSON.stringify(nr);     // #460c：回应状态（后续收到新消息时据此重算）
    // 顺序推演：本组第一条的时间（组头右侧时刻）取上一组最后一条之后的那条
    const ts = topicParseTs(m.created_at);
    if (ts) LAST_TS = ts;
    el.dataset.ts = String(ts || LAST_TS || 0);   // #460c：时间戳，判断「后续谁回应了」
    // #460：主题分组 + 接收者高亮（→ 后面那段单独上色）+ Markdown 渲染（md.js）
    const audHtml = aud
      ? Array.from(new Set(String(aud).split("、"))).filter(Boolean).map(function (x) {
          return x === MYID ? '<span class="hl">' + esc(x) + "</span>" : esc(x);
        }).join("、")
      : "-";
    const bodyHtml = (window.MD && MD.render) ? MD.render(m.text) : linkRefs(esc(m.text));
    // #460c 需求 2：待回应/已回应 渲染（等待橙色，已答绿色）
    const pendoHtml = renderPendo(nr);
    // 上下文水位（2026-09-24）：服务端按真源（OpenClaw sessions.json）渲染好挂在
    // meta.context，前端只负责显示 —— Agent 不再手打那行，避免分母写错与污染 prompt cache。
    const cx = (meta.context && meta.context.text) ? meta.context : null;
    el.innerHTML =
      '<div class="h"><span class="seq mref" data-seq="' + esc(seq) +
        '" title="点击展开 / 收起 #' + esc(seq) + ' 这条消息">#' + esc(seq) + '</span> <b>' + esc(mname(m.author)) +
      '</b>' + (m.author && m.author.type === "agent" ? '（Agent）' : '（人）') +
      ' <span class="arrow">→</span> <span class="aud" title="本条消息的接收者（受众）">' + audHtml + '</span>' +
      ' · ' + esc(m.created_at || "") +
      pendoHtml +
      '</div><div class="b md">' + bodyHtml + '</div>' +
      (cx ? '<div class="ctxwm" title="当前上下文占用 / 模型窗口（服务端按真源渲染，非 Agent 自述）">📚 ' +
        esc(cx.text) + '</div>' : '');   // #476：正文里的 #NNN 变可点标签
    // #460：先确保本主题的组卡片存在，再把消息塞进组内
    let tbody = topicBody(tid);
    if (!tbody) tbody = topicCardEnsure(tid, topicTitle({ text: m.text, author: m.author, isHuman: isHuman }, seq), ts || LAST_TS);
    tbody.appendChild(el);
    if (window.MDX && MDX.applyHighlight) MDX.applyHighlight(el);   // #460b：这条消息的代码块上色
    LAST_AUTHOR = m.author;   // 存「人/Agent 对象」，不是 id —— 免得 mid() 把对象拍平后丢失类型
    LAST_SEQ = seq;
    topicUpdateCount();
    topicsAllSync();
  });
  // #460c 需求 2：全场重算「待回应/已回应」—— 收到新回复后，旧问题自动从待回应移到已回应
  refreshNeedStates(msgs);
  if (stick) log.scrollTop = log.scrollHeight;   // #347-0①：只有贴底才跟随，非贴底一动不动

  const my = (d && d.my_pending) || [];
  const p = $("pending");
  if (my.length) {
    p.style.display = "block";
    p.innerHTML = "★ 我还欠 " + my.length + " 条回应：" +
      my.map(x => '<span class="pj" data-seq="' + esc(x.seq) + '" title="点击跳到 #' + esc(x.seq) +
              '（连点依次跳下一条）">#' + esc(x.seq) + "（来自 " + esc(x.from) + "）</span>").join("　");
    // #460e：待办列表记下来供点击循环跳转；**但 PEND_IDX 只在会话切换/内容变化时重置**，
    //         点击跳转本身绝不能触发隐藏或重置 —— 只有自己真的回应了才会消失
    const listKey = my.map(x => Number(x.seq)).join(",");
    if (listKey !== PEND_KEY) { PEND_KEY = listKey; PEND_IDX = -1; }
    PEND_LIST = my.map(x => Number(x.seq)).filter(n => n > 0);
    if (PEND_IDX >= PEND_LIST.length) PEND_IDX = -1;
  } else {
    p.style.display = "none";
    PEND_LIST = []; PEND_IDX = -1; PEND_KEY = "";
  }
  composerSyncSb();        // #418-2：消息区滚动条出现/消失 → 输入区右边界跟着对齐
  // #438-3：本轮可能带进新发言者 → 补过滤 chip（集合没变则不重建），再按当前勾选切显示
  renderMsgFilter(false); applyMsgFilter();
}

async function send() {
  if (!CUR) return;
  const text = $("text").value.trim();
  if (!text) return;
  $("send").disabled = true;
  try {
    await api("POST", "/api/sessions/" + encodeURIComponent(CUR.session_id) + "/messages",
              { text, mentions: sel() });
    $("text").value = "";
    acClose();
    document.querySelectorAll("#mentions .chip.on").forEach(c => c.classList.remove("on"));
    updHint();
    await pull(true);
    // #347-0① 例外：自己刚发的那条永远跟到底（本地动作，不算「新消息到达」）
    const lg = $("log"); if (lg) { lg.scrollTop = lg.scrollHeight; }
    focusText();                       // 发完手还在输入框，接着说话
  } catch (e) { alert("发送失败：" + e.message); }
  finally { $("send").disabled = CUR.state !== "open"; }
}

async function closeGroup() {
  if (!CUR) return;
  if (!confirm("确定宣布散会？群会变成归档状态，之后不能再发言。")) return;
  try {
    await api("PATCH", "/api/sessions/" + encodeURIComponent(CUR.session_id), { state: "archived" });
    CUR.state = "archived";
    $("closebtn").disabled = true; $("send").disabled = true;
    await pull(true); await loadGroups();
  } catch (e) { alert("关群失败：" + e.message); }
}

function openDialog() {
  $("gt").value = "";
  const box = $("agsel"); box.innerHTML = "";
  AGENTS.filter(a => (a.id !== MYID)).forEach(a => {
    const c = document.createElement("span");
    c.className = "chip"; c.textContent = "@" + a.id; c.dataset.m = a.id;
    c.onclick = () => c.classList.toggle("on");
    box.appendChild(c);
  });
  if (!AGENTS.length) box.innerHTML = '<div class="note">没取到 Agent 名册（/api/agents）。</div>';
  $("dlg").showModal();
}

// #addagent：把名册里的 Agent 拉进当前群。
//   数据源 GET /api/agents（名册）；已在本群的置灰；
//   提交 POST /api/sessions/{sid}/members {members:[...]}，响应返成员全量 → 重载当前群。
function openAddDialog() {
  if (!CUR) { alert("先选一个群，再添加 Agent。"); return; }
  if (CUR.state !== "open") { alert("本群已散会（归档），不能再加人。"); return; }
  const box = $("addsel"); box.innerHTML = "";
  const inGroup = new Set(groupMemberIds());
  const list = AGENTS.slice().sort((a, b) => String(mid(a)).localeCompare(String(mid(b))));
  if (!list.length) box.innerHTML = '<div class="note">没取到 Agent 名册（/api/agents）。</div>';
  list.forEach(a => {
    const id = mid(a); if (!id) return;
    const c = document.createElement("span");
    const already = inGroup.has(id);
    c.className = "chip" + (already ? " on" : "");
    c.textContent = "@" + id + (already ? "（已在群）" : "");
    c.dataset.m = id;
    if (already) {
      c.style.opacity = ".45"; c.style.cursor = "not-allowed";
      c.title = "已经在群里了";
    } else {
      c.onclick = () => c.classList.toggle("on");
    }
    box.appendChild(c);
  });
  const n = $("addnote");
  if (n) n.textContent = "选择要拉进本群的 Agent（已在群里的置灰不可选）—— 共 " +
    list.length + " 个，当前群 " + inGroup.size + " 人。";
  $("addok").disabled = false;
  $("adddlg").showModal();
}

async function addAgentsToGroup() {
  if (!CUR) return;
  const ids = [];
  document.querySelectorAll("#addsel .chip.on").forEach(c => {
    if (c.style.cursor !== "not-allowed" && c.dataset.m) ids.push(c.dataset.m);
  });
  if (!ids.length) { alert("先点选至少一个 Agent。"); return; }
  $("addok").disabled = true;
  try {
    await api("POST", "/api/sessions/" + encodeURIComponent(CUR.session_id) + "/members", { members: ids });
    $("adddlg").close();
    await loadGroups();
    const fresh = GROUPS.find(x => x.session_id === CUR.session_id);
    if (fresh) await openGroup(fresh); else await pull(true);
  } catch (e) {
    alert("添加失败：" + e.message);
  } finally {
    $("addok").disabled = false;
  }
}

async function createGroup() {
  const title = $("gt").value.trim() || "临时群";
  const members = [];
  document.querySelectorAll("#agsel .chip.on").forEach(c => members.push(c.dataset.m));
  $("gok").disabled = true;
  try {
    const r = await api("POST", "/api/sessions", { kind: "meet", title, members });
    $("dlg").close();
    await loadGroups();
    const sid = (r.session || {}).session_id;
    const fresh = GROUPS.find(x => x.session_id === sid);
    if (fresh) await openGroup(fresh);
  } catch (e) { alert("建群失败：" + e.message); }
  finally { $("gok").disabled = false; }
}

$("newbtn").onclick = openDialog;
$("collapsebtn").onclick = asideToggle;      // #418-3 折叠左栏
$("splitchev").onclick = asideToggle;        // #418-3 分隔条上的箭头：展开 / 折叠
$("gcancel").onclick = () => $("dlg").close();
$("gok").onclick = createGroup;
$("addagent").onclick = openAddDialog;
$("addcancel").onclick = () => $("adddlg").close();
$("addok").onclick = addAgentsToGroup;
$("refresh").onclick = () => { refreshAgents(); loadGroups().then(() => { if (CUR) pull(); }).catch(e => alert(e.message)); };
$("send").onclick = send;
$("closebtn").onclick = closeGroup;
$("text").addEventListener("input", () => { updHint(); acCheck(); });   // 手打 @ 出联想
$("text").addEventListener("click", acCheck);
$("text").addEventListener("keydown", e => {
  if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) { e.preventDefault(); send(); return; }  // 任何时候都能发
  if (AC.open) {
    if (e.key === "ArrowDown") { e.preventDefault(); AC.idx = (AC.idx + 1) % AC.items.length; acRender(); return; }
    if (e.key === "ArrowUp")   { e.preventDefault(); AC.idx = (AC.idx - 1 + AC.items.length) % AC.items.length; acRender(); return; }
    if (e.key === "Enter" || e.key === "Tab") { e.preventDefault(); acPick(AC.idx); return; }
    if (e.key === "Escape")    { e.preventDefault(); acClose(); return; }
  }
});
focusText();
boot().catch(e => { $("who").textContent = "启动失败：" + e.message; });

// #432-3：任务查询分组 = 当前打开的群（原来写死 demo01，切群后清单不跟着走）
function tgid() { return (CUR && CUR.session_id) || TGROUP_DEFAULT; }

// ================= #432-3 / #448 左栏 TAB：「我的群」/「任务队列」 =================
// #448：数据源从 task_svc :9981 的临时清单**换成 9980 自己的会话台账 todos**
// （GET /api/sessions/<本群 session_id>/todos）—— 同源、落库，刷新/换设备都还在。
// 入队不靠前端：人在群里 @agent 下发任务时后端自动建条目（待办），
// agent 开工/完成时把同一条目翻「进行中 / 已完成」，这里只如实呈现。


// ===== #534：列表只显示「抽象后的任务名」，鼠标悬停再看完整详细任务信息 =====
// #541：短名/详情改由**模型**给（入队后异步富化回写 task_name + task_summary）。
// 优先级：后端 todos.task_name（模型名 > 规则兜底名）> 前端兜底现抽。
// 富化是异步的：条目先带规则名 + enrich_state=pending 出现，模型回来后下一次轮询
// 就换成抽象名（复用下面 #448 已有的 5 秒对台账，不额外加轮询、不发重复请求）。
const TQNAME_MAX = 36;
const TQNAME_PREFIX_RE = /^(如下任务|以下任务|任务如下|任务|需求|要求|待办|你负责|负责如下|安排如下|分配如下)[:：,，、 ]*/;
const TQNAME_SERIAL_RE = /^[\(\[（【]?\d+[\.、\)）】]\s*/;

function tqName(t) {                       // 前端兜底：与后端 todo_task_name 同一套口径
  const back = String((t && t.task_name) || "").trim();
  if (back) return back;
  let b = String((t && t.text) || "").replace(/^#\d+\s*/, "").trim();
  b = b.replace(TQNAME_PREFIX_RE, "").replace(TQNAME_SERIAL_RE, "").trim();
  b = b.split(/[。！？；;!?\n\r]/)[0].trim().replace(/[，,、:：;；\s]+$/, "");
  if (!b) b = String((t && t.text) || "").trim();
  return b.length > TQNAME_MAX ? b.slice(0, TQNAME_MAX) + "…" : b;
}

function tqSummary(t) {                    // #541：模型给的详情（没回来就是空）
  return String((t && t.task_summary) || "").trim();
}

// #541-hover：富化状态的**展示映射**（只做人话标签 + 悬停解释，不改后端口径、不新增状态）：
//   enrich_state 原样回显（pending=富化中 / done=已富化 / fallback|skipped|空=未富化），
//   触发时间 = 本条入队时间（富化是在入队那一刻提交给模型的）。
function tqEnrich(t) {
  const st = String((t && t.enrich_state) || "");
  const label = st === "pending" ? "富化中" : (st === "done" ? "已富化" : "未富化");
  const at = String((t && t.created_at) || "").replace("T", " ").slice(0, 19);
  const why = st === "pending"
    ? "已交给模型重判、还没回来：当前显示的是规则兜底名"
    : (st === "done" ? "模型已回写任务名 / 详情"
       : (st === "fallback" ? "模型没回来（服务降级中）：仍显示规则兜底名"
          : (st === "skipped" ? "回填时模型判它不是任务：只标记、不删条目"
             : "老条目没有富化状态（#541 之前的存量）")));
  return {state: st, label: label, at: at, why: why,
          line: "富化：" + label + "（" + (st || "空") + "）· 触发 " + (at || "-") + " —— " + why};
}

function tqDetail(t) {                     // 悬停显示的完整信息（换行分段）
  const landing = (t.landing || [])[0] || "";
  const sum = tqSummary(t);
  const nm = String((t && t.task_name) || "").trim();
  const lines = [];
  if (nm) lines.push("任务名：" + nm);
  if (sum) lines.push("详情：" + sum.replace(/\n/g, " "));
  lines.push("原文：" + String(t.text || ""));
  lines.push("责任人：" + (mname(t.owner) || t.owner || "-"));
  lines.push("创建时间：" + String(t.created_at || "").replace("T", " ").slice(0, 19));
  lines.push("状态：" + (TQSTATUS[t.status] || t.status || "?"));
  lines.push(tqEnrich(t).line);              // #541-hover：富化状态 + 触发时间
  lines.push("落点：" + (landing || "（暂无）"));
  return lines.join("\n");
}


function aTabSet(name) {
  if (ATABS.indexOf(name) < 0) name = "groups";
  ATAB = name;
  const on = (n) => name === n;
  [["tab-groups", "groups"], ["tab-queue", "queue"]].forEach(([id, n]) => {
    const b = $(id); if (!b) return;
    b.classList.toggle("on", on(n));
    b.setAttribute("aria-selected", on(n) ? "true" : "false");
  });
  $("groups").hidden = !on("groups");
  $("tqueue").hidden = !on("queue");
  const cb = $("tqclear");                      // #448-3：清空键只在队列 TAB 露头
  if (cb) cb.hidden = !on("queue");
  if (on("queue")) tqLoad();
}

async function tqLoad() {
  const box = $("tqueue"); if (!box) return;
  const g = tgid();
  const nEl = $("atq-n");
  try {
    // #448：同源读本群的会话台账（落库的 todos），不再是 9981 的临时清单
    const d = await api("GET", "/api/sessions/" + encodeURIComponent(g) + "/todos");
    const ts = (d && d.items) || [];
    const nOpen = ts.filter(t => t.status === "open").length;
    const nDoing = ts.filter(t => t.status === "doing").length;
    const nDone = ts.filter(t => t.status === "done").length;
    if (nEl) nEl.textContent = ts.length ? String(ts.length) : "";
    box.dataset.count = String(ts.length);
    box.dataset.group = g;
    box.dataset.open = String(nOpen);
    box.dataset.doing = String(nDoing);
    box.dataset.done = String(nDone);
    box.innerHTML = "";
    if (!ts.length) {
      box.innerHTML = '<div class="note">本群还没有任务记录。<br>' +
                      "人在群里 @agent 下发任务时会自动入队（待办）。</div>";
      return;
    }
    const sum = document.createElement("div");
    sum.className = "note"; sum.id = "tqsum";
    sum.textContent = "共 " + ts.length + " 条 · 待办 " + nOpen + " / 进行中 " +
                      nDoing + " / 已完成 " + nDone;
    box.appendChild(sum);
    ts.slice().sort((a, b) => ((TQRANK[a.status] ?? 9) - (TQRANK[b.status] ?? 9)) ||
                              String(b.created_at || "").localeCompare(String(a.created_at || "")))
      .forEach(t => {
        const el = document.createElement("div");
        el.className = "tqitem" + (t.status === "done" ? " done" : "");
        el.dataset.id = t.id; el.dataset.status = t.status || ""; el.dataset.owner = t.owner || "";
        const _nm = tqName(t);
        el.dataset.name = _nm;                       // #534 实测断言读这里
        el.dataset.hasDetail = tqDetail(t) ? "1" : "0";
        // #541：模型富化的实况（CDP 断言读这里：摘要/状态；pending=模型还没回来）
        const _sum = tqSummary(t);
        const _st = String((t && t.enrich_state) || "");
        el.dataset.summary = _sum;
        el.dataset.enrich = _st;
        const landing = (t.landing || [])[0] || "";
        const when = String(t.created_at || "").slice(5, 16).replace("T", " ");
        // #541-hover：悬停说明里带富化状态 + 触发时间（CDP 断言读 el.title / .tqd title / .tqe title）
        const _en = tqEnrich(t);
        el.dataset.enrichLabel = _en.label;
        el.dataset.enrichAt = _en.at;
        el.title = tqDetail(t);
        const badge = _st === "pending"
          ? '<span class="tqe" title="' + esc(_en.line) + '">富化中…</span>'
          : (_st === "fallback"
             ? '<span class="tqe" title="' + esc(_en.line) + '">规则名</span>'
             : (_st === "skipped"
                ? '<span class="tqe" title="' + esc(_en.line) + '">未富化</span>'
                : ""));
        el.innerHTML =
          '<div class="tqh"><span class="tqs ' + esc(TQCLS[t.status] || "") + '">' +
            esc(TQSTATUS[t.status] || t.status || "?") + "</span>" +
            '<span class="tqm">' + esc(mid(t.owner) || t.owner || "") + "</span></div>" +
          '<div class="tqd" title="' + esc(tqDetail(t)) + '">' +
            esc(tqName(t)) + badge + "</div>" +
          '<div class="tqm">' + esc(when) +
            (landing ? " · " + esc(String(landing).slice(0, 46)) : "") + "</div>";
        box.appendChild(el);
      });
  } catch (e) {
    if (nEl) nEl.textContent = "";
    box.dataset.count = "0";
    box.dataset.group = g;
    box.innerHTML = '<div class="note err">任务队列拉取失败：' + esc(e.message) +
                    "（服务连不上，不是没有任务）</div>";
  }
}

async function tqClear() {
  const g = tgid();
  if (!g) return;
  if (!window.confirm("清空本群「任务队列」？\n（服务端落库清空，刷新后仍为空；历史发言不受影响）")) return;
  const btn = $("tqclear");
  if (btn) { btn.disabled = true; btn.textContent = "清空中…"; }
  try {
    const d = await api("POST", "/api/sessions/" + encodeURIComponent(g) + "/todos/purge", {});
    if (btn) btn.textContent = "已清 " + ((d && d.changed) || 0) + " 条";
    await tqLoad();
  } catch (e) {
    if (btn) btn.textContent = "清空失败";
    window.alert("清空失败：" + e.message);
  }
  window.setTimeout(() => { if (btn) { btn.disabled = false; btn.textContent = "清空"; } }, 1600);
}

$("tab-groups").onclick = () => aTabSet("groups");
$("tab-queue").onclick = () => aTabSet("queue");
const _tqcb = $("tqclear");
if (_tqcb) _tqcb.onclick = tqClear;
aTabSet("groups");
// #448：停在「任务队列」TAB 时每 5 秒对一次台账 —— 群里新下发的任务不用手动刷新就冒出来
window.setInterval(() => { if (ATAB === "queue") tqLoad(); }, 5000);


// ================= #452-1：全页可编辑元素一律关掉拼写检查 =================
// 静态属性写在 HTML 上（#text / #tp-title / #tp-owner / #gt）；
// 这里再扫一遍 DOM 兜住以后动态插入的 input/textarea，免得漏一个又画红线。
// 注：@ 联想浮层 #ac 不是可编辑元素（是 div 列表），输入 @ 仍发生在 #text 里，已被覆盖。
(function noSpellCheck(root) {
  const mark = (el) => {
    if (!el || (el.tagName !== "INPUT" && el.tagName !== "TEXTAREA")) return;
    el.spellcheck = false;
    el.setAttribute("spellcheck", "false");
    el.setAttribute("autocorrect", "off");
    el.setAttribute("autocapitalize", "off");
    el.setAttribute("autocomplete", "off");
  };
  const sweep = (r) => {
    if (!r || !r.querySelectorAll) return;
    r.querySelectorAll("input,textarea").forEach(mark);
  };
  sweep(root || document);
  if (window.MutationObserver && document.body) {
    try {
      new MutationObserver((ms) => ms.forEach((m) => m.addedNodes.forEach((nd) => {
        mark(nd); sweep(nd);
      }))).observe(document.body, { childList: true, subtree: true });
    } catch (e) {}
  }
})(document);


// ================= #476：#编号 引用标签 → 点击展开 / 收起的「引用浮窗」 =================
// 刚哥 #476 的需求：
//   ① 聊天记录里 #452 这类编号标识可点：点一次展开它代表的那条对话内容，再点一次关闭；
//   ② 弹出的内容允许拖动移动位置，也可以「固定」在所处位置；
//   ③ 可同时点开多个标签，多个浮窗并存。
// 数据来源：GET /api/sessions/<sid>/messages/<seq>（会话台账里单条取，就是那条消息的
//   发送方 / 时间 / 正文）；页面上已经渲染过的消息先吃本地缓存 MSGCACHE（pull 时顺手存），
//   命中就不打网络。浮窗层 #refpop 挂在 body 下 —— #log 每 3 秒轮询会 innerHTML=""，
//   挂在那儿才不会被轮询连带清掉。
// ⚠️ 必须带群 id：#NNN 只是**会话内**序号，跨群会撞号（真机实测踩到 —— 切到别的群再回来，
//    引用标签曾展开出另一个群同号的正文）。所以缓存键 = 群 id + seq，绝不裸用 seq。
function msgKey(sid, seq) { return String(sid || "") + "#" + String(seq); }

// 把（已转义的）正文里的 #NNN 换成可点标签。前缀不吃 # / 词字符，避免 ##123、T-#123 这种误伤。
function linkRefs(html) {
  return String(html == null ? "" : html).replace(
    /(^|[^\w#])#(\d{1,7})(?!\d)/g,
    (all, pre, n) => pre + '<span class="mref" data-seq="' + n +
      '" title="点击展开 / 收起 #' + n + ' 这条消息">#' + n + "</span>");
}

// —— 固定记录（localStorage，按群分桶）：{sid: {"452": {x, y}}} ——
function refPinsAll() {
  try { return JSON.parse(localStorage.getItem(LS_REFPINS) || "{}") || {}; }
  catch (e) { return {}; }
}
function refPinsFor(sid) {
  const a = refPinsAll(), v = a[sid];
  return (v && typeof v === "object") ? v : {};
}
function refPinSet(sid, seq, rec) {
  if (!sid) return;
  const a = refPinsAll();
  const v = (a[sid] && typeof a[sid] === "object") ? a[sid] : {};
  if (rec) v[String(seq)] = { x: Math.round(rec.x), y: Math.round(rec.y) };
  else delete v[String(seq)];
  a[sid] = v;
  try { localStorage.setItem(LS_REFPINS, JSON.stringify(a)); } catch (e) { /* 无痕模式就算了 */ }
}

function refLayer() {
  let l = document.getElementById("refpop");
  if (!l) {
    l = document.createElement("div");
    l.id = "refpop";
    document.body.appendChild(l);
  }
  return l;
}

// 夹回视口内 → 落坐标。写回 rec.x/rec.y（取整），固定记录与还原走同一套算术，
// 所以「固定后刷新」拿到的坐标与固定那一刻逐像素一致。
function refPlace(rec) {
  const el = rec.el; if (!el) return;
  const w = el.offsetWidth || 360, h = el.offsetHeight || 180;
  const maxX = Math.max(6, (window.innerWidth || 1280) - w - 6);
  const maxY = Math.max(6, (window.innerHeight || 800) - h - 6);
  rec.x = Math.round(Math.min(Math.max(6, rec.x), maxX));
  rec.y = Math.round(Math.min(Math.max(6, rec.y), maxY));
  el.style.left = rec.x + "px";
  el.style.top = rec.y + "px";
}

function refFront(rec) {
  REFZ = Math.max(71, REFZ + 1);
  if (rec && rec.el) rec.el.style.zIndex = String(REFZ);
}

// 同一条引用的所有标签（正文里的 + 消息头部的 #seq）一起点亮 / 熄灭
function refMarkOn(seq) {
  document.querySelectorAll('.mref[data-seq="' + seq + '"]').forEach(x => x.classList.add("on"));
}
function refMarkOff(seq) {
  document.querySelectorAll('.mref[data-seq="' + seq + '"]').forEach(x => x.classList.remove("on"));
}

function refPinApply(seq, on) {                  // 置位（不翻转），还原时用
  const r = REFPOP.get(seq); if (!r) return;
  r.pinned = !!on;
  r.el.classList.toggle("pinned", r.pinned);
  const b = r.el.querySelector(".refpin");
  if (b) { b.classList.toggle("on", r.pinned); b.textContent = r.pinned ? "已固定" : "固定"; }
  refPinSet(r.sid, seq, r.pinned ? { x: r.x, y: r.y } : null);
}
function refPinToggle(seq) {
  const r = REFPOP.get(parseInt(seq, 10)); if (!r) return;
  refPinApply(r.seq, !r.pinned);
}

function refClose(seq, keepPin) {
  seq = parseInt(seq, 10);
  const r = REFPOP.get(seq); if (!r) return false;
  if (r.el && r.el.parentNode) r.el.parentNode.removeChild(r.el);
  REFPOP.delete(seq);
  refMarkOff(seq);
  // 收起 = 顺手取消固定：否则刷新后它又自己冒出来，像关不掉
  if (!keepPin && r.pinned) refPinSet(r.sid, seq, null);
  return true;
}

// forget=true：清掉「已还原」标记（换群回来/刷新后再还原一遍）；keepPin 保住固定记录
function refPopsReset(forget, sid) {
  Array.from(REFPOP.keys()).forEach(seq => refClose(seq, true));
  if (sid) { REFSID = sid; if (forget) REFREST.delete(sid); }
  else if (forget) REFREST.clear();
}

// 还原本群固定过的浮窗（刷新 / 切群回来都在原位）
function refPopsRestore(sid) {
  if (!sid) return 0;
  REFSID = sid;
  if (REFREST.has(sid)) return 0;
  REFREST.add(sid);
  const pins = refPinsFor(sid);
  const keys = Object.keys(pins);
  keys.forEach(k => {
    const seq = parseInt(k, 10); if (!seq) return;
    const p = pins[k] || {};
    refOpen(seq, { x: Number(p.x), y: Number(p.y), pinned: true });
  });
  return keys.length;
}

function refCloseAll() { refPopsReset(false); }

// 拖动：抓手是浮窗标题栏（.ph）；按钮上按下不算拖动
function refDrag(rec, ev) {
  if (ev.button !== undefined && ev.button !== 0) return;
  const sx = ev.clientX, sy = ev.clientY, ox = rec.x, oy = rec.y;
  rec.el.classList.add("dragging");
  const mv = (e) => { rec.x = ox + (e.clientX - sx); rec.y = oy + (e.clientY - sy); refPlace(rec); };
  const up = () => {
    window.removeEventListener("mousemove", mv);
    window.removeEventListener("mouseup", up);
    rec.el.classList.remove("dragging");
    refPlace(rec);
    rec.el.dataset.dragged = "1";
    if (rec.pinned) refPinSet(rec.sid, rec.seq, { x: rec.x, y: rec.y });   // 固定态拖完立刻落盘
  };
  window.addEventListener("mousemove", mv);
  window.addEventListener("mouseup", up);
  ev.preventDefault();
}

// 展开：元素同步建出来（点击立刻可见），正文再异步补 —— 不阻塞、也不闪一下空白
// #476：取某条消息的正文 —— 三级来源，前一档拿不到就退下一档：
//   ① 页面上已渲染过的消息（MSGCACHE，pull() 时顺手存）；
//   ② GET /api/sessions/<sid>/messages/<seq>（单条快路，服务端若提供则省一次流量）；
//   ③ GET /api/sessions/<sid>/messages?limit=1&before_seq=<seq+1>（**早就冻结的列表接口**），
//      从返回里挑 seq 相等那条 —— 线上当前实际走的就是这一条（9980 的 chat_svc 正由
//      别的活轮次在改，路由表里没有 ② 时引用标签照样能展开任意历史编号）。
//   前端因此不依赖任何新增服务端接口，function 只挂在「冻结契约」的端点上。
async function refFetchMsg(sid, seq) {
  const s = encodeURIComponent(sid);
  const tries = [
    "/api/sessions/" + s + "/messages/" + seq,
    "/api/sessions/" + s + "/messages?limit=1&before_seq=" + (seq + 1)
  ];
  let last = "";
  for (let i = 0; i < tries.length; i++) {
    try {
      const d = await api("GET", tries[i]);
      if (d && d.message && Number(d.message.seq) === Number(seq)) { REFSRC = (i === 0 ? "single:" : "list:") + seq; return d.message; }
      const arr = (d && d.messages) || [];
      const hit = arr.filter(x => Number(x.seq) === Number(seq))[0];
      if (hit) { REFSRC = (i === 0 ? "single:" : "list:") + seq; return hit; }
      last = "本群台账里没有 #" + seq + " 这条";
    } catch (e) { last = e.message; }
  }
  throw new Error(last || "取不到");
}

async function refOpen(seq, opt) {
  seq = parseInt(seq, 10);
  if (!seq) return null;
  const o = opt || {};
  if (REFPOP.has(seq)) {                       // 已开着：抬到最前，不重复建
    const r0 = REFPOP.get(seq);
    refFront(r0);
    if (o.pinned && !r0.pinned) refPinApply(seq, true);
    return r0;
  }
  const sid = o.sid || REFSID;
  if (!sid) return null;
  const el = document.createElement("div");
  el.className = "pop" + (o.pinned ? " pinned" : "");
  el.dataset.seq = String(seq);
  el.innerHTML =
    '<div class="ph"><span class="refseq">#' + esc(seq) + '</span>' +
      '<span class="refmeta">读取中…</span><span class="spacer"></span>' +
      '<button class="ghost refpin' + (o.pinned ? " on" : "") +
        '" title="固定在当前位置（刷新后仍在原位）">' + (o.pinned ? "已固定" : "固定") + '</button>' +
      '<button class="ghost refclose" title="收起这条引用">✕</button></div>' +
    '<div class="pb">读取中…</div>';
  const n = REFPOP.size;                        // 并存多个：每开一个错开一点，不叠成一摞
  const rec = {
    el, seq, sid, pinned: !!o.pinned,
    x: (typeof o.x === "number" && !isNaN(o.x)) ? o.x : 96 + (n % 6) * 30,
    y: (typeof o.y === "number" && !isNaN(o.y)) ? o.y : 110 + (n % 6) * 30,
  };
  REFPOP.set(seq, rec);
  refLayer().appendChild(el);
  refPlace(rec);
  refFront(rec);
  refMarkOn(seq);
  el.querySelector(".refclose").onclick = (e) => { e.stopPropagation(); refClose(seq); };
  el.querySelector(".refpin").onclick = (e) => { e.stopPropagation(); refPinToggle(seq); };
  el.querySelector(".ph").addEventListener("mousedown", (e) => {
    if (e.target && e.target.closest && e.target.closest("button")) return;
    refDrag(rec, e);
  });
  el.addEventListener("mousedown", () => refFront(rec));

  let m = MSGCACHE.get(msgKey(sid, seq)), err = "";
  if (!m) {                                     // 没在页面上 → 打服务端取真实正文
    try { m = await refFetchMsg(sid, seq); }
    catch (e) { err = e.message; }
  }
  if (!document.body.contains(el)) return null; // 等待期间用户已经收起
  const rm = el.querySelector(".refmeta"), pb = el.querySelector(".pb");
  if (!m) {
    rm.textContent = "取不到";
    pb.className = "pb err";
    pb.textContent = "拉不到 #" + seq + " 的正文" + (err ? "（" + err + "）" : "") + "。";
    refPlace(rec);
    return rec;
  }
  rm.textContent = mname(m.author) +
    (m.author && m.author.type === "agent" ? "（Agent）" : "（人）") +
    (m.created_at ? " · " + m.created_at : "");
  // #460：引用浮窗的正文也走同一套 Markdown 渲染（渲染体已全程转义）
  pb.className = "pb md";
  pb.innerHTML = (window.MD && MD.render) ? MD.render(m.text || "") : linkRefs(esc(m.text || ""));
  if (window.MDX && MDX.applyHighlight) MDX.applyHighlight(pb);
  refPlace(rec);                                // 正文进来后高度变了，重新夹一次
  return rec;
}

function refToggle(seq) {
  seq = parseInt(seq, 10);
  if (!seq) return null;
  return REFPOP.has(seq) ? (refClose(seq), null) : refOpen(seq);
}

// 点击委托：全文档任何 .mref（正文里的引用标签 / 每条消息头部的 #seq）都走这里。
// 委托而不是逐个绑 onclick —— #log 每次轮询重渲染，绑在元素上的监听会一起没。
document.addEventListener("click", (e) => {
  const t = (e.target && e.target.closest) ? e.target.closest(".mref") : null;
  if (!t) return;
  const seq = parseInt(t.dataset.seq, 10);
  if (!seq) return;
  e.preventDefault();
  refToggle(seq);
});

// #460d：顶部「我还欠 N 条回应」——点一下依次跳到下一条未答复的，循环。
// 若正好是待办里的某条，从它开始往后循环；不是待办（比如手点别的）则从头开始。
function pendingJump(seq) {
  const log = $("log"); if (!log || !PEND_LIST.length) return;
  if (seq > 0) {
    const k = PEND_LIST.indexOf(Number(seq));
    if (k >= 0) PEND_IDX = k;                       // 从被点的这条开始
  }
  if (PEND_IDX < 0) PEND_IDX = 0;
  else if (seq <= 0) PEND_IDX = (PEND_IDX + 1) % PEND_LIST.length;   // 仅顶部行点击才向后步进
  const target = PEND_LIST[PEND_IDX];
  const el = log.querySelector('.tgb .msg[data-seq="' + target + '"]');
  if (!el) return null;
  // 若目标所在主题组是折叠的，先展开（否则看不见）
  const g = el.closest(".tgrp");
  if (g && g.classList.contains("closed")) {
    g.classList.remove("closed");
    topicClosedWrite(CUR && CUR.session_id, g.dataset.tid, false);
    topicUpdateCount();
  }
  if (el.scrollIntoView) el.scrollIntoView({ block: "center", behavior: "smooth" });
  else log.scrollTop = Math.max(0, el.offsetTop - log.clientHeight / 2);
  el.classList.remove("flash"); void el.offsetWidth; el.classList.add("flash");
  setTimeout(() => el.classList.remove("flash"), 2400);
  return target;
}

// 两条点击路径：①点顶部那行本体 → 循环跳；②点行内的 #39（来自 hermes）→ 以那条为起点往后循环
document.addEventListener("click", (e) => {
  const p = (e.target && e.target.closest) ? e.target.closest("#pending") : null;
  if (!p) return;
  if (p.style.display === "none") return;
  e.preventDefault();
  const j = (e.target.closest && e.target.closest(".pj")) || null;
  pendingJump(j ? parseInt(j.dataset.seq, 10) : 0);
});

// ================= #460f：一键全部折叠 / 全部展开 =================
// 规则：若还有「展开着」的组 → 折叠全部；若已全部折叠 → 展开全部（按钮文案跟着变）。
function topicsAll(closed) {
  const log = $("log"); if (!log) return;
  log.querySelectorAll(".tgrp").forEach(g => {
    g.classList.toggle("closed", !!closed);
    topicClosedWrite(CUR && CUR.session_id, g.dataset.tid, !!closed);
    // 折叠时把吸顶组头也拉回原位，避免残影
    if (closed) g.scrollTop = 0;
  });
  topicUpdateCount();
  topicsAllSync();
}

function topicsAllSync() {
  const log = $("log"), b = $("tgall");
  if (!log || !b) return;
  const all = log.querySelectorAll(".tgrp").length;
  const closed = log.querySelectorAll(".tgrp.closed").length;
  const anyOpen = closed < all && all > 0;
  b.textContent = anyOpen ? "⊟" : "⊞";        // 图标=将要执行的动作：折叠全部 / 展开全部（语义放 title/aria）
  b.disabled = all === 0;
  b.title = anyOpen ? "折叠本群所有主题组" : "展开本群所有主题组";
  b.setAttribute("aria-label", b.title);        // 只剩图标：语义放 title + aria
}

if ($("tgall")) $("tgall").addEventListener("click", () => {
  const log = $("log"); if (!log) return;
  const all = log.querySelectorAll(".tgrp").length;
  const closed = log.querySelectorAll(".tgrp.closed").length;
  topicsAll(closed < all);        // 还有展开的 → 全折；已全折 → 全开
});

// #presence：在线态每 30s 自刷新（页面挂着不动也一直新）
setInterval(() => { refreshAgents().catch(() => {}); }, 30000);

// 自测句柄（真机实测读内部状态用；不改变任何行为）
window.__hP = { renderAgentBar, refreshAgents, agents: () => AGENTS };
window.__h476 = { MSGCACHE, REFPOP, linkRefs, refOpen, refClose, refToggle,
                  refPinToggle, refPinApply, refPinsFor, refPopsRestore, refPopsReset,
                  src: () => REFSRC, fetchMsg: refFetchMsg };
