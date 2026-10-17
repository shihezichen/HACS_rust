// ============================================================================
// tex.js —— 轻量 LaTeX 公式 → 浏览器原生 <math>（MathML）渲染器（2026-09-24）
//
// 为什么要它：刚哥要求「LaTeX 类似的公式要真渲染，不是原样显示 \int_0^1 ...」。
// 不引外部库（KaTeX 要 ~300KB + 字体，群页要轻）；改用浏览器原生 MathML ——
// Chrome 109+/Edge/Firefox/Safari 全支持，零依赖、零字体、零网络请求。
//
// 覆盖范围（够工程/数学日常）：
//   上下标 _ ^、\frac、\sqrt 与开方次数、\int/\sum/\prod 与 _ ^ 限、希腊字母、
//   常用运算符/关系符/箭头/大符号、\left \right 自适应括号、\text{中文}、
//   \mathbf \mathrm \mathcal，以及 \quad \, \; \! 等间距。
// 兜底：不认识的命令照原样显示（绝不吞掉内容），并清掉渲染层里的空白。
//
// 安全：输出**只由本模块按白名单生成**，用户文本先全部被 esc() 转义，
// 任何 < > & 都变成实体，绝不把原文里的标签带进结果。
// ============================================================================
(function (global) {
  "use strict";

  function esc(s) {
    return String(s == null ? "" : s)
      .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;").replace(/'/g, "&#39;");
  }

  var GREEK = {
    alpha: "α", beta: "β", gamma: "γ", delta: "δ", epsilon: "ε", varepsilon: "ε",
    zeta: "ζ", eta: "η", theta: "θ", vartheta: "ϑ", iota: "ι", kappa: "κ",
    lambda: "λ", mu: "μ", nu: "ν", xi: "ξ", pi: "π", varpi: "ϖ", rho: "ρ",
    sigma: "σ", varsigma: "ς", tau: "τ", upsilon: "υ", phi: "φ", varphi: "φ",
    chi: "χ", psi: "ψ", omega: "ω",
    Gamma: "Γ", Delta: "Δ", Theta: "Θ", Lambda: "Λ", Xi: "Ξ", Pi: "Π",
    Sigma: "Σ", Upsilon: "Υ", Phi: "Φ", Psi: "Ψ", Omega: "Ω"
  };

  var SYM = {
    // 二元运算
    times: "×", div: "÷", cdot: "⋅", pm: "±", mp: "∓", ast: "∗", star: "⋆",
    circ: "∘", bullet: "∙", oplus: "⊕", ominus: "⊖", otimes: "⊗", oslash: "⊘",
    cup: "∪", cap: "∩", setminus: "∖", wr: "≀", diamond: "⋄", bigcirc: "◯",
    // 关系
    le: "≤", leq: "≤", ge: "≥", geq: "≥", ne: "≠", neq: "≠", equiv: "≡",
    approx: "≈", sim: "∼", simeq: "≃", cong: "≅", propto: "∝", ll: "≪", gg: "≫",
    subset: "⊂", supset: "⊃", subseteq: "⊆", supseteq: "⊇", in: "∈", ni: "∋",
    notin: "∉", mid: "∣", parallel: "∥", perp: "⊥", prec: "≺", succ: "≻",
    // 箭头
    rightarrow: "→", to: "→", leftarrow: "←", gets: "←", leftrightarrow: "↔",
    Rightarrow: "⇒", Leftarrow: "⇐", Leftrightarrow: "⇔", implies: "⟹",
    iff: "⟺", mapsto: "↦", uparrow: "↑", downarrow: "↓", updownarrow: "↕",
    longrightarrow: "⟶", longleftarrow: "⟵", hookrightarrow: "↪",
    // 大符号 / 杂项
    infty: "∞", partial: "∂", nabla: "∇", forall: "∀", exists: "∃", nexists: "∄",
    emptyset: "∅", varnothing: "∅", aleph: "ℵ", hbar: "ℏ", ell: "ℓ",
    Re: "ℜ", Im: "ℑ", wp: "℘", angle: "∠", triangle: "△", degree: "°",
    neg: "¬", land: "∧", lor: "∨", lnot: "¬", top: "⊤", bot: "⊥",
    int: "∫", iint: "∬", iiint: "∭", oint: "∮", sum: "∑", prod: "∏",
    coprod: "∐", bigcup: "⋃", bigcap: "⋂", lim: "lim", log: "log", ln: "ln",
    sin: "sin", cos: "cos", tan: "tan", cot: "cot", sec: "sec", csc: "csc",
    exp: "exp", min: "min", max: "max", det: "det", gcd: "gcd", mod: "mod",
    // 空格
    quad: "\u2003", qquad: "\u2003\u2003", ",": "\u2009", ";": "\u2005",
    ":": "\u2005", "!": "", " ": " ", "\\": "\n"
  };

  // 需要「限」符号（上下标要摆在正上/正下）的大运算符
  var BIGOPS = { "∑": 1, "∏": 1, "∫": 1, "∬": 1, "∭": 1, "∮": 1, "⋃": 1, "⋂": 1, "∐": 1, "lim": 1 };
  // \left \right 后面的分隔符
  var DELIM = {
    "(": "(", ")": ")", "[": "[", "]": "]", "\\{": "{", "\\}": "}",
    "|": "|", "\\|": "‖", ".": "", "\\langle": "⟨", "\\rangle": "⟩",
    "\\lceil": "⌈", "\\rceil": "⌉", "\\lfloor": "⌊", "\\rfloor": "⌋"
  };

  // ---------- 词法：命令 / 数字 / 字母 / 括号 / 运算符 ----------
  function tokenize(s) {
    var toks = [], i = 0;
    while (i < s.length) {
      var c = s[i];
      if (c === "\\") {
        var m = /^\\([A-Za-z]+|.)/.exec(s.slice(i));
        if (m) { toks.push({ t: "cmd", v: m[1] }); i += m[0].length; continue; }
        i++; continue;
      }
      if (/\s/.test(c)) { i++; continue; }
      if (/[0-9.]/.test(c)) {
        var n = /^[0-9]*\.?[0-9]+/.exec(s.slice(i));
        toks.push({ t: "num", v: n[0] }); i += n[0].length; continue;
      }
      if (/[A-Za-z]/.test(c)) { toks.push({ t: "id", v: c }); i++; continue; }
      if (c === "{") { toks.push({ t: "{" }); i++; continue; }
      if (c === "}") { toks.push({ t: "}" }); i++; continue; }
      if (c === "^" || c === "_") { toks.push({ t: c }); i++; continue; }
      toks.push({ t: "op", v: c }); i++;
    }
    return toks;
  }

  function Parser(toks) { this.toks = toks; this.i = 0; }
  Parser.prototype.peek = function () { return this.toks[this.i]; };
  Parser.prototype.next = function () { return this.toks[this.i++]; };
  Parser.prototype.eat = function (t) {
    var k = this.peek();
    if (k && k.t === t) { this.i++; return true; }
    return false;
  };

  // 一个「原子」：命令 / 数字 / 字母 / {…} / 括号 / 运算符
  Parser.prototype.atom = function () {
    var k = this.next();
    if (!k) return "";
    if (k.t === "{") {
      var inner = this.group();
      return "<mrow>" + inner + "</mrow>";
    }
    if (k.t === "cmd") return this.cmd(k.v);
    if (k.t === "num") return "<mn>" + esc(k.v) + "</mn>";
    if (k.t === "id") return "<mi>" + esc(k.v) + "</mi>";
    if (k.t === "op") {
      var ch = k.v;
      if (ch === "<") return "<mo>&lt;</mo>";
      if (ch === ">") return "<mo>&gt;</mo>";
      if (ch === "&") return "<mo>&amp;</mo>";
      if (/[+\-=*,;/:!?()|']/.test(ch)) return "<mo>" + esc(ch) + "</mo>";
      return "<mo>" + esc(ch) + "</mo>";
    }
    if (k.t === "}" || k.t === "^" || k.t === "_") {
      this.i--;  // 交还
      return "";
    }
    return "";
  };

  // 读到配对的 }
  Parser.prototype.group = function () {
    var out = "";
    while (this.i < this.toks.length) {
      var k = this.peek();
      if (k && k.t === "}") { this.i++; break; }
      out += this.term();
    }
    return out;
  };

  Parser.prototype.script = function () {
    var base = this.atom();
    var sub = null, sup = null;
    while (true) {
      if (this.eat("_")) sub = this.atom();
      else if (this.eat("^")) sup = this.atom();
      else break;
    }
    if (sub == null && sup == null) return base;
    var inner = base;
    if (sub != null) inner += "<msub>" + base + "</msub>";
    if (sub != null && sup != null) inner = "<msubsup>" + base + sub + sup + "</msubsup>";
    else if (sup != null) inner = "<msup>" + base + sup + "</msup>";
    return inner;
  };

  Parser.prototype.term = function () { return this.script(); };

  Parser.prototype.run = function () {
    var out = "";
    while (this.i < this.toks.length) {
      var k = this.peek();
      if (k && k.t === "}") break;
      out += this.term();
    }
    return out;
  };

  // ---------- 命令 ----------
  Parser.prototype.cmd = function (name) {
    var self = this;
    if (GREEK[name]) return "<mi>" + GREEK[name] + "</mi>";
    if (SYM[name] !== undefined) {
      var sym = SYM[name];
      if (sym === "\n") return "<mspace/>";
      if (BIGOPS[sym]) {
        // 大运算符：把紧随的 _ ^ 变成 munderover
        var lo = null, hi = null, q;
        while (true) {
          q = self.peek();
          if (q && q.t === "_") { self.next(); lo = self.atom(); }
          else if (q && q.t === "^") { self.next(); hi = self.atom(); }
          else break;
        }
        var base = "<mo>" + sym + "</mo>";
        if (lo && hi) return "<munderover>" + base + lo + hi + "</munderover>";
        if (lo) return "<munder>" + base + lo + "</munder>";
        if (hi) return "<mover>" + base + hi + "</mover>";
        return base;
      }
      if (/^[A-Za-z]+$/.test(sym) && (name === "lim" || name === "log" || name === "ln" ||
          name === "sin" || name === "cos" || name === "tan" || name === "exp" ||
          name === "min" || name === "max" || name === "det" || name === "gcd" ||
          name === "mod" || name === "cot" || name === "sec" || name === "csc")) {
        return "<mi>" + sym + "</mi>";
      }
      if (sym === "") return "";
      return "<mo>" + sym + "</mo>";
    }
    switch (name) {
      case "frac": case "dfrac": case "tfrac": {
        var a = self.atom(), b = self.atom();
        return "<mfrac>" + a + b + "</mfrac>";
      }
      case "sqrt": {
        // 可选开方次数 \sqrt[3]{x}
        var deg = null;
        if (self.peek() && self.peek().t === "op" && self.peek().v === "[") {
          self.next(); deg = "";
          while (self.i < self.toks.length) {
            var k = self.peek();
            if (k && k.t === "op" && k.v === "]") { self.next(); break; }
            deg += self.term();
          }
        }
        var body = self.atom();
        return deg != null ? "<mroot>" + body + deg + "</mroot>" : "<msqrt>" + body + "</msqrt>";
      }
      case "left": {
        var lo = self.delim();
        var inner = "";
        var depth = 0;
        while (self.i < self.toks.length) {
          var k2 = self.peek();
          if (k2 && k2.t === "cmd" && k2.v === "right" && depth === 0) { self.next(); break; }
          if (k2 && k2.t === "cmd" && k2.v === "left") depth++;
          if (k2 && k2.t === "cmd" && k2.v === "right" && depth > 0) depth--;
          inner += self.term();
        }
        var ro = self.delim();
        var o = lo ? "<mo stretchy=\"true\">" + lo + "</mo>" : "";
        var c = ro ? "<mo stretchy=\"true\">" + ro + "</mo>" : "";
        return "<mrow>" + o + inner + c + "</mrow>";
      }
      case "right": return "";
      case "text": case "mathrm": case "mathbf": case "mathit": case "mathcal": case "operatorname": {
        var raw = self.rawGroup();
        var t = esc(raw);
        if (name === "mathbf") return "<mtext mathvariant=\"bold\">" + t + "</mtext>";
        if (name === "mathit") return "<mtext mathvariant=\"italic\">" + t + "</mtext>";
        if (name === "mathcal") return "<mtext mathvariant=\"script\">" + t + "</mtext>";
        return "<mtext>" + t + "</mtext>";
      }
      case "begin": case "end": { self.rawGroup(); return ""; }   // 环境不支持：吃掉名字，不渲染
      case "displaystyle": case "limits": case "nolimits": return "";
      default:
        // 不认识的命令：**照原样显示**（\foo → \foo），绝不吞内容
        return "<mtext>\\" + esc(name) + "</mtext>";
    }
  };

  // 取 {…} 里的**原始文本**（\text{中文} 用，里面不再做数学解析）
  Parser.prototype.rawGroup = function () {
    var k = this.peek();
    if (!k || k.t !== "{") return this.atomRaw ? this.atomRaw() : "";
    this.next();
    var out = "", depth = 1;
    while (this.i < this.toks.length) {
      var q = this.toks[this.i];
      if (q.t === "{") depth++;
      if (q.t === "}") { depth--; if (!depth) { this.i++; break; } }
      if (q.t === "cmd") out += "\\" + q.v;
      else if (q.t === "num") out += q.v;
      else if (q.t === "id") out += q.v;
      else if (q.t === "{") out += "{";
      else if (q.t === "}") out += "}";
      else if (q.t === "^" || q.t === "_") out += q.t;
      else out += (q.v || "");
      this.i++;
    }
    return out;
  };

  Parser.prototype.delim = function () {
    var k = this.peek();
    if (!k) return "";
    if (k.t === "cmd") { this.next(); return DELIM["\\" + k.v] !== undefined ? DELIM["\\" + k.v] : ""; }
    if (k.t === "op") {
      if (k.v === "<") { this.next(); return "&lt;"; }
      if (k.v === ">") { this.next(); return "&gt;"; }
      if (k.v === "|") { this.next(); return "|"; }
      this.next(); return esc(k.v);
    }
    return "";
  };

  // ---------- 对外入口 ----------
  // opts.display === false -> 行内 MathML（不占整行）；默认块级（保持旧行为）
  function toMathML(tex, opts) {
    var t = String(tex == null ? "" : tex).trim();
    if (!t) return "";
    var inline = !!(opts && opts.display === false);
    var body;
    try {
      body = new Parser(tokenize(t)).run();
    } catch (e) {
      return '<span class="md-mathraw">' + esc(t) + "</span>";
    }
    if (!body) body = "<mtext>" + esc(t) + "</mtext>";
    if (inline) {
      return '<math xmlns="http://www.w3.org/1998/Math/MathML" class="md-mathml md-math-inline">' + body + "</math>";
    }
    return '<math xmlns="http://www.w3.org/1998/Math/MathML" display="block" class="md-mathml">' + body + "</math>";
  }

  function toMathMLInline(tex) {
    var t = String(tex == null ? "" : tex).trim();
    if (!t) return "";
    var body;
    try {
      body = new Parser(tokenize(t)).run();
    } catch (e) {
      return '<span class="md-mathraw">' + esc(t) + "</span>";
    }
    if (!body) return "";
    return '<math xmlns="http://www.w3.org/1998/Math/MathML" class="md-mathml">' + body + "</math>";
  }

  global.TeX = { toMathML: toMathML, toMathMLInline: toMathMLInline, esc: esc };
})(window);
