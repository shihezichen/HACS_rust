/* ============================================================
   items.js —— 条目三段式视图（并入自 <deploy-dir>\hacs_ui 骨架）
   契约：顶 #hx-top 固定 / 底 #hx-bottom 固定 / 中 #hx-mid 为唯一滚动容器
         + #seq 跳转高亮 + 点击条目浮窗显示原文（再点同一条 / Esc / 浮窗 × 关闭）
   数据源：优先本前台既有通道 /api/items（同源，经 serve.py 反代 chat 服务）
           不可用时回落真接口 http://127.0.0.1:8787/api/items（Access-Control-Allow-Origin:*）
           翻页一律用响应里的 next_cursor
   说明：本文件不改动 app.js 的任何逻辑，只新增一个独立视图与按钮绑定。
   ============================================================ */
(function () {
  "use strict";

  var LIMIT = (function () {
    // ?hxlimit=N 可调每页条数（自测/运维用；仍受服务端上限 200 约束）
    var n = parseInt((new URLSearchParams(location.search)).get("hxlimit") || "", 10);
    if (!n || n < 1) return 200;
    return Math.min(n, 200);
  })();
  var SAME_ORIGIN = "/api/items";
  var REAL_API = "http://127.0.0.1:8787/api/items";

  // ?hxapi=<base> 覆盖数据基址（运维/自测用；缺省走上面两条候选）
  var override = (new URLSearchParams(location.search)).get("hxapi") || "";
  var CANDIDATES = override ? [override] : [SAME_ORIGIN, REAL_API];

  var $ = function (id) { return document.getElementById(id); };
  var view, statusEl, listEl, countEl, srcEl, moreBtn;

  var state = {
    rows: [],          // {seq, item, el, index}
    bySeq: {},         // seq -> row
    cursor: null,      // next_cursor
    total: null,
    loaded: 0,
    apiBase: null,     // 探测成功的数据基址
    openSeq: null,     // 浮窗当前序号
    busy: false,
    booted: false
  };

  /* ---------------- 序号：与骨架一致 item.seq > id 内数字 ------------- */
  function seqOf(item) {
    if (item && item.seq != null && item.seq !== "") return String(item.seq);
    var m = String((item && item.id) || "").match(/\d+/);
    return m ? String(parseInt(m[0], 10)) : "?";
  }

  function esc(s) {
    return String(s == null ? "" : s).replace(/[&<>"']/g, function (c) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
    });
  }

  function fmtTime(v) { return String(v || "").replace("T", " ").slice(0, 16); }

  /* ---------------------------- 浮窗 ---------------------------- */
  function closePop() {
    var pop = $("hx-pop");
    if (pop) pop.hidden = true;
    state.openSeq = null;
    state.rows.forEach(function (r) { r.el.classList.remove("hx-hit"); });
  }

  function openPop(rec) {
    var pop = $("hx-pop");
    state.openSeq = rec.seq;
    $("hx-pop-seq").textContent = "#" + rec.seq;
    $("hx-pop-body").textContent = fullText(rec.item);
    $("hx-pop-meta").textContent = fullMeta(rec.item);
    pop.hidden = false;
    state.rows.forEach(function (r) { r.el.classList.toggle("hx-hit", r.seq === rec.seq); });
    rec.el.scrollIntoView({ block: "nearest" });
  }

  /* 原文：优先正文类字段；无则回落为全部字段的原文呈现 */
  function fullText(it) {
    if (!it) return "(无内容)";
    if (typeof it === "string") return it;
    var body = it.text || it.body || it.content || it.summary || it.detail || "";
    if (body) return String(body);
    var title = it.title || "(无标题)";
    if (it.raw && typeof it.raw === "string") return title + "\n\n" + it.raw;
    // 骨架数据只有 id/title/start/updated：原文 = 条目全字段
    return title + "\n\n（该条目无正文字段，以下为原文 JSON）\n" +
      JSON.stringify(it, null, 2);
  }

  function fullMeta(it) {
    if (!it || typeof it !== "object") return "";
    var bits = [];
    if (it.id != null) bits.push("id=" + it.id);
    if (it.start != null) bits.push("start=" + it.start);
    if (it.updated != null) bits.push("updated=" + it.updated);
    if (it.source != null) bits.push("source=" + it.source);
    return bits.join(" · ");
  }

  function togglePop(rec) { if (state.openSeq === rec.seq) closePop(); else openPop(rec); }

  /* --------------------------- 渲染 --------------------------- */
  function render(list) {
    listEl.innerHTML = "";
    state.rows = [];
    state.bySeq = {};
    state.loaded = 0;
    list.forEach(function (it) {
      var row = document.createElement("div");
      row.className = "hx-item";
      var seq = seqOf(it);
      row.innerHTML = '<span class="hx-id">#' + esc(seq) + '</span>' +
        '<span class="hx-title">' + esc(it.title || "(无标题)") + '</span>' +
        '<span class="hx-time">' + esc(fmtTime(it.updated || it.start)) + '</span>';
      var rec = { seq: seq, item: it, el: row, index: state.rows.length + 1 };
      row.addEventListener("click", function () { togglePop(rec); });
      state.rows.push(rec);
      state.bySeq[seq] = rec;
      state.loaded++;
      listEl.appendChild(row);
    });
    closePop();
    updateCount();
  }

  function append(items) {
    items.forEach(function (it) {
      var row = document.createElement("div");
      row.className = "hx-item";
      var seq = seqOf(it);
      row.innerHTML = '<span class="hx-id">#' + esc(seq) + '</span>' +
        '<span class="hx-title">' + esc(it.title || "(无标题)") + '</span>' +
        '<span class="hx-time">' + esc(fmtTime(it.updated || it.start)) + '</span>';
      var rec = { seq: seq, item: it, el: row, index: state.rows.length + 1 };
      row.addEventListener("click", function () { togglePop(rec); });
      state.rows.push(rec);
      if (!state.bySeq[seq]) state.bySeq[seq] = rec;
      state.loaded++;
      listEl.appendChild(row);
    });
    updateCount();
  }

  function updateCount() {
    countEl.textContent = state.loaded + " / " + (state.total == null ? "?" : state.total);
    moreBtn.disabled = state.busy || !state.cursor;
  }

  function setStatus(text, isErr) {
    statusEl.className = "hx-status" + (isErr ? " err" : "");
    statusEl.textContent = text;
  }

  /* --------------------------- 取数 --------------------------- */
  function buildUrl(base, cursor) {
    var u = base + (base.indexOf("?") >= 0 ? "&" : "?") + "limit=" + LIMIT;
    if (cursor) u += "&cursor=" + encodeURIComponent(cursor);
    return u;
  }

  function fetchJson(url) {
    return fetch(url, { credentials: "omit", headers: { "Accept": "application/json" } })
      .then(function (r) {
        if (!r.ok) throw new Error("HTTP " + r.status);
        return r.json();
      })
      .then(function (d) {
        if (!d || d.ok === false || !Array.isArray(d.items)) {
          throw new Error("无条目列表（schema=" + ((d && d.schema) || "-") + "）");
        }
        return d;
      });
  }

  /* 探测数据源：本前台既有通道 → 真接口 */
  function probe() {
    var i = 0;
    function next() {
      if (i >= CANDIDATES.length) return Promise.reject(new Error("所有数据源都不可用"));
      var base = CANDIDATES[i++];
      return fetchJson(buildUrl(base, null)).then(
        function (d) { state.apiBase = base; return d; },
        function () { return next(); }
      );
    }
    return next();
  }

  function load(cursor) {
    if (state.busy) return;
    state.busy = true;
    updateCount();
    var base = state.apiBase || SAME_ORIGIN;
    var url = buildUrl(base, cursor || null);
    fetchJson(url)
      .then(function (d) {
        if (cursor) append(d.items || []); else render(d.items || []);
        state.total = (d.total != null) ? d.total : (d.items || []).length;
        state.cursor = d.next_cursor || null;
        var isReal = state.apiBase === REAL_API;
        var srcName = isReal ? "真接口 8787" : ("本前台既有通道 " + state.apiBase);
        setStatus("数据源：" + srcName + " · schema=" + (d.schema || "-") +
          " · total=" + state.total + " · 本页 " + (d.items || []).length +
          (state.cursor ? " · 可翻页" : " · 已到底"));
        srcEl.textContent = "数据源：" + srcName + " · total=" + state.total;
        state.busy = false;
        updateCount();
      })
      .catch(function (e) {
        state.busy = false;
        setStatus("取数失败：" + e.message + "（布局不受影响，仅无数据）", true);
        updateCount();
      });
  }

  function boot() {
    if (state.booted) return;
    state.booted = true;
    setStatus("加载中…");
    probe().then(
      function (d) {
        render(d.items || []);
        state.total = (d.total != null) ? d.total : (d.items || []).length;
        state.cursor = d.next_cursor || null;
        var isReal = state.apiBase === REAL_API;
        var srcName = isReal ? "真接口 8787" : ("本前台既有通道 " + state.apiBase);
        setStatus("数据源：" + srcName + " · schema=" + (d.schema || "-") +
          " · total=" + state.total + " · 本页 " + (d.items || []).length +
          (state.cursor ? " · 可翻页" : " · 已到底"));
        srcEl.textContent = "数据源：" + srcName + " · total=" + state.total;
        updateCount();
      },
      function (e) { setStatus("取数失败：" + e.message, true); }
    );
  }

  /* --------------------------- 跳转 --------------------------- */
  function jumpTo(raw) {
    var m = String(raw || "").match(/\d+/);
    if (!m) { setStatus("输入 #序号 可定位条目（例：#12），点击任意条可看原文", false); return; }
    var want = String(parseInt(m[0], 10));
    var rec = state.bySeq[want];
    if (!rec) {
      setStatus("未找到序号 #" + want + "（当前已加载 " + state.loaded + " 条" +
        (state.cursor ? "，可「加载更多」" : "") + "）", true);
      return;
    }
    openPop(rec);
    setStatus("已定位 #" + want, false);
  }

  /* ----------------------- 打开 / 关闭视图 ----------------------- */
  var VIEW_HASH = /^#items(?:\/seq\/(\d+))?$/;

  function openView(seq) {
    view.hidden = false;
    boot();
    if (seq) jumpTo(seq);
    $("hx-input").focus();
  }

  function closeView(opts) {
    closePop();
    view.hidden = true;
    if (!(opts && opts.keepHash) && /^#items/.test(location.hash)) {
      history.replaceState(null, "", location.pathname + location.search);
    }
  }

  function boot2() {
    view = $("hx-view");
    if (!view) return;
    statusEl = $("hx-status");
    listEl = $("hx-list");
    countEl = $("hx-count");
    srcEl = $("hx-src");
    moreBtn = $("hx-more");

    var openBtn = $("hx-open");
    if (openBtn) openBtn.addEventListener("click", function () { openView(); });

    $("hx-pop-close").addEventListener("click", function (e) { e.stopPropagation(); closePop(); });
    $("hx-pop-close-top").addEventListener("click", function () { closeView(); });
    $("hx-reload").addEventListener("click", function () {
      state.apiBase = null; state.cursor = null; state.total = null;
      listEl.innerHTML = ""; state.rows = []; state.bySeq = {}; state.loaded = 0;
      closePop();
      if (override) { state.apiBase = override; load(null); } else { boot(); }
    });
    moreBtn.addEventListener("click", function () {
      if (state.cursor) load(state.cursor); else setStatus("没有更多页了（next_cursor=null）", false);
    });
    $("hx-jump").addEventListener("click", function () { jumpTo($("hx-input").value); });
    $("hx-input").addEventListener("keydown", function (e) {
      if (e.key === "Enter") { e.preventDefault(); jumpTo($("hx-input").value); }
    });

    // Esc：先关浮窗；浮窗已关则退出该视图（不影响其它模块的 Esc 处理）
    document.addEventListener("keydown", function (e) {
      if (e.key !== "Escape") return;
      if (!$("hx-pop").hidden) { closePop(); e.preventDefault(); }
      else if (!view.hidden) { closeView(); e.preventDefault(); }
    });

    // 深链：#items / #items/seq/12
    var m = location.hash.match(VIEW_HASH);
    if (m) openView(m[1]);

    window.addEventListener("hashchange", function () {
      var h = location.hash.match(VIEW_HASH);
      if (h) openView(h[1]);
      else if (!view.hidden && !location.hash) closeView({ keepHash: true });
    });

    window.TCItems = { state: state, open: openView, close: closeView, jumpTo: jumpTo, load: load };
  }

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", boot2);
  else boot2();
})();
