// ============================================================================
// md.js —— 群页专用 Markdown 渲染器（自研，无外部依赖；2026-09-24 刚哥需求 3）
//
// 设计要点：
//  · 安全第一：**任何内容都先 HTML 转义再加标签**，绝不把原始 HTML 放进输出。
//    <img src=x onerror=...> / <b>粗</b> 这类都只会显示成字面文本（已断言）。
//  · 只认「行首可判」的 markdown 语法，不递归、不做 inline HTML，边界可控。
//  · 数学：$$...$$（块级）、\\[...\\]（块级）、$...$ 与 \\(...\\)（行内）。
//    公式先抽成不可见占位符，等 markdown 处理完再贴回去，避免被 `_`/`*` 之类误伤。
//  · 代码块 ```lang 不做高亮（保持轻量），行首空格原样保留。
//
// 用法：window.MD.render(text) → HTML 字符串（已转义）。
// ============================================================================
(function (global) {
  "use strict";

  function esc(s) {
    return String(s == null ? "" : s)
      .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;").replace(/'/g, "&#39;");
  }

  // ---- 行内：先转义，再按「先长后短」的顺序加标签 ----
  // 注意：链接/行内代码的替换值里不能再出现会被后续规则二次匹配的字符——
  // 所以 href 与 code 的内容里把 * ` _ 一律占位掉（见 _maskInline）。
  function _maskInline(s) {
    // 把可能引发二次匹配的字符换成等价实体，视觉不变、语义安全
    return String(s).replace(/\*/g, "&#42;").replace(/`/g, "&#96;").replace(/_/g, "&#95;");
  }

  function inline(src) {
    var s = String(src == null ? "" : src);
    var i;
    // 1) 行内代码 `xxx`（内容整体掩码，避免里面的 * 变粗体）
    s = s.replace(/`([^`\n]+)`/g, function (m, c) { return "<code>" + _maskInline(c) + "</code>"; });
    // 2) 链接 [文字](url http/https only)
    s = s.replace(/\[([^\]\n]+)\]\((https?:\/\/[^\s)]+)\)/g, function (m, t, u) {
      return '<a href="' + u.replace(/"/g, "%22") + '" target="_blank" rel="noopener noreferrer">'
        + t + "</a>";
    });
    // 3) 裸链接（http/https），已被 a 标签包住的不再匹配
    s = s.replace(/(^|[^"'>=])(https?:\/\/[^\s<]+)/g, function (m, pre, u) {
      return pre + '<a href="' + u.replace(/"/g, "%22") + '" target="_blank" rel="noopener noreferrer">' + u + "</a>";
    });
    // 4) 加粗 / 斜体 / 删除线（** 要在 * 之前处理）
    s = s.replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>");
    s = s.replace(/(^|[^*])\*([^*\n]+)\*(?!\*)/g, "$1<em>$2</em>");
    s = s.replace(/~~([^~\n]+)~~/g, "<del>$1</del>");
    return s;
  }

  // ---- 数学占位：先抽出来，最后再贴回 ----
  // 用 \u0000 开头的占位符，正文（用户输入）里不可能出现 \u0000。
  function extractMath(src) {
    var store = [];
    function stash(html) {
      store.push(html);
      return "\u0000M" + (store.length - 1) + "\u0000";
    }
    var T = function () { return (typeof window !== "undefined" && window.TeX) ? window.TeX : null; };

    // ---- 方案A：LaTeX 语法归一化（ASCII 上下标 -> 花括号分组）----
    // Agent 常写 E=mc^2 / \int_0^1 / x_i^2，tex.js 分词器只吃单字符脚本；
    // 归一化成 E=mc^{2} / \int_{0}^{1} 才能被正确排成上标/下标。
    function normTex(t) {
      var x = String(t == null ? "" : t);
      // ^ 或 _ 后跟「一组花括号」保持不变；否则把紧随的 ASCII 片段包成花括号
      x = x.replace(/([_^])(\{[^{}]*\}|[^\s{}]+)/g, function (m, op, body) {
        if (body.charAt(0) === "{") return op + body;
        // 只取首位数字/字母，其余交回（x^2y -> x^{2}y）
        var head = body.charAt(0);
        var rest = body.slice(1);
        return op + "{" + head + "}" + rest;
      });
      // \frac12 -> \frac{1}{2}；\sqrt3 -> \sqrt{3}
      x = x.replace(/\\frac\s*([0-9])([0-9])/g, "\\frac{$1}{$2}");
      x = x.replace(/\\sqrt\s*([0-9])(?![0-9{])/g, "\\sqrt{$1}");
      return x;
    }

    function block(tex) {
      var t = T();
      var body = normTex(tex);
      if (t && t.toMathML) return stash(t.toMathML(body));
      return stash('<div class="md-mathblock">' + esc(body) + "</div>");
    }
    function inl(tex) {
      var t = T();
      var body = normTex(tex);
      if (t && t.toMathMLInline) return stash(t.toMathMLInline(body));
      return stash('<span class="md-math">' + esc(body) + "</span>");
    }
    var s = String(src == null ? "" : src);

    // 块级：$$ ... $$（可跨行，独占行；也容忍行内出现）
    s = s.replace(/\$\$([\s\S]+?)\$\$/g, function (m, body) {
      return block(body.replace(/^\n+|\n+$/g, ""));
    });
    // 块级：\[ ... \]
    s = s.replace(/\\\[([\s\S]+?)\\\]/g, function (m, body) {
      return block(body.replace(/^\n+|\n+$/g, ""));
    });
    // 行内：\( ... \)
    s = s.replace(/\\\(([\s\S]+?)\\\)/g, function (m, body) {
      return inl(body);
    });
    // 行内：$ ... $（不跨行、不吞 $，且要求左右不是空白/数字粘连）
    s = s.replace(/\$([^\n$]+?)\$/g, function (m, body) {
      if (!body.trim()) return m;
      if (/^\s|\s$/.test(body)) return m;             // 前后留空通常是「价格 $5 到 $9」这类
      if (body.indexOf("\\") < 0 && !/[=^_{}]/.test(body)) return m;  // 无数学特征 → 不当公式
      return inl(body);
    });
    return { text: s, store: store };
  }

  function restoreMath(html, store) {
    return String(html).replace(/\u0000M(\d+)\u0000/g, function (m, n) {
      var v = store[parseInt(n, 10)];
      return v == null ? m : v;
    });
  }

  // ---- 列表：连续的 - / * / + / 1. 行归成一个 <ul>/<ol>；带两空格缩进 = 子级 ----
  var RE_UL = /^\s*[-*+]\s+(.*)$/;
  var RE_OL = /^\s*(\d+)[.)]\s+(.*)$/;

  function renderList(lines, start, ordered) {
    var items = [], i = start, re = ordered ? RE_OL : RE_UL;
    while (i < lines.length) {
      var m = lines[i].match(re);
      if (!m) break;
      var indentChild = /^\s{2,}/.test(lines[i]);
      if (items.length && indentChild) {
        // 子列表：把后续更深缩进的行交给下一个列表块，简单挂到上一项后面
        var sub = [];
        while (i < lines.length && /^\s{2,}/.test(lines[i]) && lines[i].trim()) {
          sub.push(lines[i].replace(/^\s{2}/, ""));
          i++;
        }
        var subHtml = renderBlock(sub.join("\n"));
        items[items.length - 1] += subHtml;
        continue;
      }
      if (indentChild && !items.length) { /* 首行就缩进，当普通项 */ }
      items.push(inline(ordered ? m[2] : m[1]));
      i++;
    }
    var tag = ordered ? "ol" : "ul";
    return { html: "<" + tag + ">" + items.map(function (t) { return "<li>" + t + "</li>"; }).join("") + "</" + tag + ">", next: i };
  }

  function renderTable(lines, start) {
    // 表头 | 分隔行(---) | 数据行...
    var rows = [], i = start;
    while (i < lines.length && /^\s*\|/.test(lines[i])) { rows.push(lines[i].trim()); i++; }
    if (rows.length < 2) return { html: null, next: start };
    var sep = rows[1].replace(/[|\s]/g, "");
    if (!/^:?-+:?$/.test(sep.replace(/-/g, "-").slice(0, 3)) && !/^:?-{1,}:?$/.test(sep.replace(/:?-+:?/g, "").slice(0, 3) || "-")) {
      // 简易判据：第二行去掉 | 空格后只剩 - 和 :
      if (!/^[-:]{2,}$/.test(sep)) return { html: null, next: start };
    }
    function cells(line) {
      return line.replace(/^\s*\|/, "").replace(/\|\s*$/, "").split("|").map(function (c) { return c.trim(); });
    }
    var head = cells(rows[0]);
    var body = rows.slice(2).map(cells);
    var h = "<table><thead><tr>" + head.map(function (c) { return "<th>" + inline(c) + "</th>"; }).join("") + "</tr></thead><tbody>";
    body.forEach(function (r) {
      h += "<tr>" + r.map(function (c) { return "<td>" + inline(c) + "</td>"; }).join("") + "</tr>";
    });
    return { html: h + "</tbody></table>", next: i };
  }

  // 方案A：段落里可能混着公式占位符（\u0000M<n>\u0000）。
  // 先按占位符把段落切成「文本片段 / 公式片段」：文本片段才做换行，
  // 公式片段原样贴回 —— 否则 <br> 会灌进 MathML 内部，把 E=mc<sup>2</sup> 劈成两行。
  function _phStore(store) { return store || []; }
  function _splitByPh(joined) {
    var re = /\u0000M\d+\u0000/g;
    var parts = [], phs = [], last = 0, m;
    while ((m = re.exec(joined)) !== null) {
      parts.push(joined.slice(last, m.index));
      phs.push(m[0]);
      last = m.index + m[0].length;
    }
    parts.push(joined.slice(last));
    return { parts: parts, phs: phs };
  }
  function paraToHtml(para, store) {
    var joined = para.join("\n");
    if (!/\u0000M\d+\u0000/.test(joined)) {
      return "<p>" + para.map(inline).join("<br>") + "</p>";
    }
    var sp = _splitByPh(joined);
    var buf = [], k;
    for (k = 0; k < sp.parts.length; k++) {
      if (sp.parts[k] !== "") buf.push(inline(sp.parts[k]).replace(/\n/g, "<br>"));
      if (k < sp.phs.length) {
        var idx = parseInt(sp.phs[k].replace(/[^0-9]/g, ""), 10);
        buf.push(store[idx] == null ? sp.phs[k] : store[idx]);
      }
    }
    return "<p>" + buf.join("") + "</p>";
  }

  function renderBlock(rawSrc, store) {
    // 安全铁律：进 renderBlock 之前先整段 HTML 转义。
    // 之后的 inline() 只负责「在已转义文本上加标签」，任何原始 HTML 都不会漏到输出里。
    var lines = esc(rawSrc).replace(/\r\n?/g, "\n").split("\n");
    var out = [], i = 0, para = [];
    var MSTORE = _phStore(store);

    function flushPara() {
      if (!para.length) return;
      // 「段内单换行」→ <br>（markdown 习惯）；空行已经切段
      out.push(paraToHtml(para, MSTORE));
      para = [];
    }

    while (i < lines.length) {
      var ln = lines[i];

      // 空行 → 收段
      if (!ln.trim()) { flushPara(); i++; continue; }

      // 代码块 ``` 或 ~~~
      var fence = ln.match(/^\s*(```+|~~~+)\s*([\w+#-]*)\s*$/);
      if (fence) {
        flushPara();
        var closer = fence[1][0];
        var langTag = fence[2] || "";
        var body = [], j = i + 1;
        while (j < lines.length && !new RegExp("^\\s*" + closer + "{3,}\\s*$").test(lines[j])) { body.push(lines[j]); j++; }
        out.push('<pre class="md-code" data-lang="' + esc(langTag) + '"><code>' + esc(body.join("\n")) + "</code></pre>");
        i = (j < lines.length) ? j + 1 : j;
        continue;
      }

      // 分隔线
      if (/^\s*([-*_])\s*\1\s*\1[\s\1]*$/.test(ln)) { flushPara(); out.push("<hr>"); i++; continue; }

      // 数学块占位（独占行）
      if (/^\u0000M\d+\u0000$/.test(ln.trim())) { flushPara(); out.push(ln.trim()); i++; continue; }

      // 标题
      var h = ln.match(/^(#{1,6})\s+(.*)$/);
      if (h) { flushPara(); var lv = h[1].length; out.push("<h" + lv + ">" + inline(h[2]) + "</h" + lv + ">"); i++; continue; }

      // 引用 > ...（注意：renderBlock 入口已整体转义，行首的 > 变成 &gt;）
      var qm = ln.match(/^\s*(?:&gt;|>)\s?(.*)$/);
      if (qm) {
        flushPara();
        var q = [];
        while (i < lines.length) {
          var qm2 = lines[i].match(/^\s*(?:&gt;|>)\s?(.*)$/);
          if (!qm2) break;
          q.push(qm2[1]); i++;
        }
        out.push("<blockquote>" + q.map(inline).join("<br>") + "</blockquote>");
        continue;
      }

      // 表格
      if (/^\s*\|/.test(ln)) {
        flushPara();
        var t = renderTable(lines, i);
        if (t.html) { out.push(t.html); i = t.next; continue; }
      }

      // 列表
      if (RE_UL.test(ln)) { flushPara(); var l1 = renderList(lines, i, false); out.push(l1.html); i = l1.next; continue; }
      if (RE_OL.test(ln)) { flushPara(); var l2 = renderList(lines, i, true); out.push(l2.html); i = l2.next; continue; }

      para.push(ln); i++;
    }
    flushPara();
    return out.join("");
  }

  // #460b：段落里「裸贴」的 LaTeX（无 $ 也无 \[ \]）—— 交给 codeblock.js 的 texifyParagraph 判定
  var RE_TEXHINT = /\\[A-Za-z]+/;

  function enhance(html) {
    var W = (typeof window !== "undefined") ? window : global;
    var M = W.MDX;
    if (!M || !M.texifyParagraph) return html;
    if (typeof W.DOMParser === "undefined" || typeof W.document === "undefined") return html;
    var doc;
    try {
      doc = new W.DOMParser().parseFromString("<div id=\"__mdx\">" + html + "</div>", "text/html");
      var root = doc.getElementById("__mdx");
      if (!root) return html;
      // 只改「纯段落」：里面不能已有块级元素（否则说明本来就是 markdown 结构）
      var hit = false;
      Array.prototype.slice.call(root.querySelectorAll("p")).forEach(function (p) {
        if (p.querySelector("pre,table,ul,ol,blockquote,h1,h2,h3,h4,h5,h6,math,div")) return;
        // ⚠️ 用 innerHTML 而不是 textContent：
        //   段落里的换行是 <br>，而各浏览器对 textContent 的换行还原不一致（实测有把两行粘成一行的情况）。
        //   直接拿 innerHTML（保证保留 <br>），交给 texifyParagraph 自己拆行（它认识 <br>）。
        var raw = p.innerHTML || "";
        var txt = raw.replace(/<br\s*\/?>/gi, "\n").replace(/<[^>]+>/g, "");
        if (!RE_TEXHINT.test(txt)) return;          // 没有 \命令 → 不可能是公式，不动它
        var rep = M.texifyParagraph(txt);
        if (!rep) return;
        var holder = doc.createElement("div");
        holder.innerHTML = rep;
        // 只在「确实引入了公式」时才替换（避免把普通段落的链接也重写一遍）
        if (!holder.querySelector("math")) return;
        p.parentNode.replaceChild(holder, p);
        hit = true;
      });
      if (!hit) return html;
      return root.innerHTML;
    } catch (e) {
      return html;
    }
  }

  function render(src) {
    var ex = extractMath(src);
    // 公式占位符在 renderBlock 内已按「段」还原（fix2），这里兜一次列表/引用/表格里的残留
    return enhance(restoreMath(renderBlock(ex.text, ex.store), ex.store));
  }

  global.MD = { render: render, _esc: esc, _inline: inline };
})(typeof window !== "undefined" ? window : this);
