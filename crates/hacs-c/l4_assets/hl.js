// ============================================================================
// hl.js —— 轻量代码高亮器（2026-09-24 刚哥要求 2：代码块要「鲜明」的高亮）
//
// 取舍：不引 highlight.js / Prism（体积 + 额外请求）。自研 token 扫描器，
// 覆盖群页里最常出现的语言：python / js / ts / json / bash / sql / html / css /
// yaml / c++ / java / go / rust / ini / md。不认识的语言就退化成「等宽纯文本」。
//
// 安全：**先把整段代码 esc() 转义，再只在转义后的文本上贴 <span class="tk-*">**。
// 所以代码里的 < > & 与任何标签都不可能变成真标签（已断言）。
// ============================================================================
(function (global) {
  "use strict";

  function esc(s) {
    return String(s == null ? "" : s)
      .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  }

  // 语言别名归一（注意：**语言全名必须自己由键**，否则会落到默认的 C 系规则）
  var ALIAS = {
    py: "python", python: "python", python3: "python",
    js: "js", javascript: "js", mjs: "js", cjs: "js",
    ts: "js", tsx: "js", jsx: "js", node: "js", json: "json", json5: "json",
    sh: "bash", shell: "bash", bash: "bash", zsh: "bash", console: "bash",
    ps: "bash", powershell: "bash", ps1: "bash",
    sql: "sql", psql: "sql", postgres: "sql",
    html: "html", htm: "html", xml: "html", vue: "html", svg: "html",
    css: "css", scss: "css", less: "css",
    yml: "yaml", yaml: "yaml", toml: "ini", ini: "ini", conf: "ini", env: "ini",
    c: "c", h: "c", cpp: "c", "c++": "c", cc: "c", hpp: "c", cs: "c", java: "c",
    go: "c", golang: "c", rust: "c", rs: "c", kt: "c", swift: "c",
    md: "md", markdown: "md", text: "", plain: "", txt: ""
  };

  // 关键词表（各语言一组；c 组给 C 系共用）
  var KW = {
    python: "False None True and as assert async await break class continue def del elif else except finally for from global if import in is lambda nonlocal not or pass raise return try while with yield self match case".split(" "),
    js: "await async break case catch class const continue debugger default delete do else export extends finally for function if implements import in instanceof interface let new of package private protected public return static super switch this throw try typeof var void while with yield null true false undefined NaN".split(" "),
    json: "true false null".split(" "),
    bash: "if then else elif fi for while do done case esac function return in export local readonly declare echo printf cd ls cp mv rm mkdir rmdir cat grep sed awk curl ssh scp sudo apt yum systemctl docker podman git node npm python3 pip jq chmod chown tar find sleep test set source".split(" "),
    sql: "SELECT FROM WHERE INSERT INTO VALUES UPDATE SET DELETE CREATE TABLE ALTER DROP INDEX JOIN LEFT RIGHT INNER OUTER FULL ON GROUP BY ORDER HAVING LIMIT OFFSET UNION ALL DISTINCT AS AND OR NOT NULL IS IN BETWEEN LIKE CASE WHEN THEN ELSE END PRIMARY KEY FOREIGN REFERENCES DEFAULT CASCADE COUNT SUM AVG MIN MAX WITH RETURNING CONFLICT DO NOTHING NOTHING BEGIN COMMIT ROLLBACK".split(" "),
    html: "html head body div span p a img script style link meta title h1 h2 h3 h4 h5 h6 ul ol li table thead tbody tr th td button input select option textarea form label section article nav header footer main aside pre code strong em br hr template".split(" "),
    css: "color background border margin padding display position top left right bottom width height font flex grid align justify transition transform overflow content z-index box-shadow border-radius opacity".split(" "),
    yaml: "true false null yes no on off".split(" "),
    ini: "true false yes no".split(" "),
    c: "abstract assert auto bool break byte case catch char class const constexpr continue default delete do double else enum explicit export extern false final finally float for friend goto if implements import instanceof int interface long namespace new operator package private protected public register return short signed sizeof static struct switch synchronized template this throw throws transient true try typedef typename union unsigned using virtual void volatile while var let func fn impl trait pub crate mod match loop mut ref type go defer chan map package range select interface nil string byte rune error".split(" ")
  };

  var RE_ID = /[A-Za-z_$\u00c0-\uffff][A-Za-z0-9_$\u00c0-\uffff]*/y;
  var RE_NUM = /(?:0[xX][0-9a-fA-F_]+|0[bB][01_]+|0[oO][0-7_]+|(?:\d[\d_]*\.?[\d_]*|\.\d[\d_]*)(?:[eE][+-]?\d+)?)(?:[uUlLfF]+)?/y;
  // 注释开头检测（原实现有处 `src.slice(i, i + 2) === "//"`、`src.substr(i, 2) === "--"`
  // 在 `tryMatch` 的 `y` 正则与不定长扫描混用时会错位，这里统一用 startsWith 判）
  var RE_LINE_CMT2 = /\/\//;
  var RE_BLOCK_CMT_OPEN = /\/\*/;
  var RE_BLOCK_CMT_CLOSE = /\*\//;
  var RE_NUM_START = /[0-9]/;

  function isKw(lang, w) {
    var list = KW[lang];
    if (!list) return false;
    // KW 表已按语言归一；调用方传入的 w 已是标识符原文
    // 注：这里**不能**做 lowercase 比较 —— Python 的 None/True/False 与关键字大小写敏感
    for (var i = 0; i < list.length; i++) {
      if (list[i] === w) return true;
    }
    return false;
  }

  var SPAN = function (cls, txt) { return '<span class="tk-' + cls + '">' + txt + "</span>"; };

  // ---- 扫描主循环：单遍、无回溯 ----
  function highlight(code, lang) {
    var L = ALIAS[(lang || "").toLowerCase().trim()];
    if (L === undefined) L = "c";                  // 未知语言：按 C 系通用规则试一把
    var src = esc(code);
    var out = "", i = 0, n = src.length;
    var lineStart = true;

    function tryMatch(re) {
      re.lastIndex = i;
      var m = re.exec(src);
      return (m && m.index === i && m[0].length) ? m[0] : null;
    }

    // y 标志正则带粘滞语义，多轮调用前必须重置 lastIndex；这里包装一层
    function kwScan(re, s, at) {
      re.lastIndex = at;
      var m = re.exec(s);
      return (m && m.index === at && m[0].length) ? m[0] : null;
    }

    while (i < n) {
      var c = src[i];

      // 注释（放在最前，防止 /* 里的引号被当字符串）
      if ((L === "python" || L === "bash" || L === "yaml" || L === "ini") && c === "#") {
        var e1 = src.indexOf("\n", i);
        if (e1 < 0) e1 = n;
        out += SPAN("cmt", src.slice(i, e1)); i = e1; continue;
      }
      if (L === "sql" && c === "-" && src.slice(i, i + 2) === "--") {
        var e2 = src.indexOf("\n", i); if (e2 < 0) e2 = n;
        out += SPAN("cmt", src.slice(i, e2)); i = e2; continue;
      }
      if (c === "/" && RE_LINE_CMT2.test(src.slice(i, i + 2))) {
        var e3 = src.indexOf("\n", i); if (e3 < 0) e3 = n;
        out += SPAN("cmt", src.slice(i, e3)); i = e3; continue;
      }
      if (c === "/" && src.slice(i, i + 2) === "/*") {
        var e4 = src.indexOf("*/", i + 2);
        e4 = (e4 < 0) ? n : e4 + 2;
        out += SPAN("cmt", src.slice(i, e4)); i = e4; continue;
      }

      // 字符串
      if (c === '"' || c === "'" || c === "`") {
        var q = c, j = i + 1;
        while (j < n) {
          if (src[j] === "\\") { j += 2; continue; }
          if (src[j] === q) { j++; break; }
          j++;
        }
        out += SPAN("str", src.slice(i, j)); i = j; continue;
      }
      // Python 三引号
      if (L === "python" && (src.slice(i, i + 3) === '"""' || src.slice(i, i + 3) === "'''")) {
        var tq = src.slice(i, i + 3), e5 = src.indexOf(tq, i + 3);
        e5 = (e5 < 0) ? n : e5 + 3;
        out += SPAN("str", src.slice(i, e5)); i = e5; continue;
      }
      // HTML 标签： <name ...> 整段当标签色（但内容已转义，安全）
      if (L === "html" && c === "&" && src.slice(i, i + 4) === "&lt;") {
        var e6 = src.indexOf("&gt;", i);
        e6 = (e6 < 0) ? n : e6 + 4;
        out += SPAN("tag", src.slice(i, e6)); i = e6; continue;
      }

      // 数字
      if (RE_NUM_START.test(c) || (c === "." && /[0-9]/.test(src[i + 1] || ""))) {
        var num = tryMatch(RE_NUM);
        if (num) { out += SPAN("num", num); i += num.length; continue; }
      }

      // 标识符 / 关键字 / 函数名
      if (/[A-Za-z_$\u00c0-\uffff]/.test(c)) {
        var id = tryMatch(RE_ID);
        if (id) {
          var rest = src.slice(i + id.length);
          var nxt = rest.replace(/^\s+/, "")[0];
          if (isKw(L, id)) out += SPAN("kw", id);
          else if (nxt === "(") out += SPAN("fn", id);
          else if (/^[A-Z]/.test(id) && (L === "python" || L === "c")) out += SPAN("cls", id);
          else out += id;
          i += id.length; continue;
        }
      }

      // 运算符上色（注意：& 在 esc 后是实体开头，**不能**上色，否则第二遍处理时会拆坏 &lt; &gt;）
      if (/[=+\-*\/%<>!|?~^]/.test(c)) { out += SPAN("op", c); i++; continue; }

      out += c;
      i++;
    }
    return out;
  }

  global.HL = { highlight: highlight, esc: esc };
})(window);
