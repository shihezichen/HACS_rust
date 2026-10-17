//! `blackboard_view` —— 团队黑板·只读看板 HTTP 装配（对应 Python
//! `hub/l3_domain/blackboard_view.py` 的 Handler + main，端口 9941）。
//!
//! 路由面（与 Python 逐条对齐，全部只读 + 唯一写口 POST /login）：
//! * GET/HEAD `/`（portal 卡片墙）与 `/portal`、`/board`、`/healthz`、`/favicon.ico`；
//! * GET `/api/health|entries|search|sessions|tracking|actions|issues|shared|shared/search`；
//! * GET `/api/sessions/<id>`（恰好三段）；POST `/login`（唯一写入口）；
//! * 未登记读路径 → 404 文案；一切其他写方法 → 405（只读纪律，不看登录状态）。
//!
//! 认证：`--mode public`（默认）= 一律要登录；`tailnet` = 不装认证。
//! 门禁只看**对端 IP** 判 tailnet 免登录；锁定键 / 审计 IP 用 `_client_ip()`
//! （对端落在 --trust-proxy 里才采信 CF-Connecting-IP / X-Forwarded-For）。
#![allow(clippy::too_many_arguments)]

use hacs_l0_host::node_profile;
use hacs_l1_transport::http_client;
use hacs_l1_transport::web_gateway::{Gateway, Reply, Request};
use hacs_l3_domain::blackboard_view as bv;
use hacs_l3_domain::blackboard_view::Auth;
use hacs_x_identity::view_auth as va;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

const VERSION: &str = bv::VERSION;
const DEFAULT_PORT: u16 = 9941;
const UPSTREAM_TIMEOUT_S: u64 = 5;
const UPSTREAM_MAX_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 50;
const QUERY_MAX: usize = 2048;
const DETAIL_MAX: usize = 200;
const FORM_MAX_BYTES: usize = 8192;
const NEXT_MAX: usize = 512;
const TRUST_HEADER_MAX: usize = 64;

const JSON_CT: &str = "application/json; charset=utf-8";
const HTML_CT: &str = "text/html; charset=utf-8";
const TEXT_CT: &str = "text/plain; charset=utf-8";

const LOGIN_PATH: &str = "/login";
const ACCOUNT_PATH: &str = "/account";
const BOARD_PATH: &str = "/board";

const CSRF_FIELD: &str = "csrf";
const LOGIN_ACTIONS: [&str; 3] = ["login", "passwd", "logout"];

/// 公网面的固定安全头集合（顺序即字节顺序，一个都不许动）。
const SECURITY_HEADERS: [(&str, &str); 7] = [
    ("Cache-Control", "no-store"),
    ("X-Content-Type-Options", "nosniff"),
    ("Referrer-Policy", "no-referrer"),
    (
        "Strict-Transport-Security",
        "max-age=31536000; includeSubDomains",
    ),
    ("X-Frame-Options", "DENY"),
    ("X-Robots-Tag", "noindex, nofollow"),
    (
        "Content-Security-Policy",
        "default-src 'self'; style-src 'self' 'unsafe-inline'; \
         script-src 'self' 'unsafe-inline'; connect-src 'self'; \
         img-src 'self' data:; object-src 'none'; base-uri 'none'; \
         form-action 'self'; frame-ancestors 'none'",
    ),
];

// ================================================================ 运行时

struct Runtime {
    upstream: String,
    token: String,
    token_loaded: bool,
    refresh_seconds: i64,
    mode: String, // public | tailnet
    auth: Option<Auth>,
    secure_cookie: bool,
    domain_scope: String,
}

/// 一次请求背后的访问者（Python `Visitor`：role 允许 None —— tailnet 不遮蔽）。
struct Visitor {
    username: Option<String>,
    role: Option<String>,
    source: String, // session | tailnet
}

impl Visitor {
    fn tailnet() -> Self {
        Visitor {
            username: None,
            role: None,
            source: "tailnet".into(),
        }
    }
}

// ================================================================ 小工具

fn log_line(msg: &str) {
    println!("{msg}");
}

fn json_bytes_py(payload: &Value) -> Vec<u8> {
    // JSON 出口：< > & 转 \uXXXX（浏览器 JSON.parse 原样解回，但字节流里没有裸 <script>）。
    let text = hacs_l1_transport::web_gateway::dumps_python(payload);
    let s = String::from_utf8_lossy(&text)
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    s.into_bytes()
}

fn html_esc(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn scrub(text: &str, token: &str) -> String {
    let mut out = text.to_string();
    if !token.is_empty() {
        out = out.replace(token, "***");
    }
    // 兜底："Bearer xxx" 整体打码
    out = bearer_scrub(&out);
    truncate_chars(&out, DETAIL_MAX)
}

fn bearer_scrub(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(idx) = rest.to_ascii_lowercase().find("bearer ") {
        let after = &rest[idx + 7..];
        let tok_len = after
            .find(|c: char| c.is_whitespace())
            .unwrap_or(after.len());
        if tok_len == 0 {
            // "bearer " 后面没东西，跳过本处避免死循环
            out.push_str(&rest[..idx + 7]);
            rest = &rest[idx + 7..];
            continue;
        }
        out.push_str(&rest[..idx]);
        out.push_str("Bearer ***");
        rest = &after[tok_len..];
    }
    out.push_str(rest);
    out
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut cut: String = text.chars().take(max).collect();
        cut.push_str("...");
        cut
    }
}

/// `?next=` 收敛成站内相对路径（防开放跳转；Python `safe_next`）。
fn safe_next(raw: &str, domain_scope: &str) -> String {
    let text = raw.trim();
    if text.is_empty() || text.chars().count() > NEXT_MAX {
        return "/".into();
    }
    if !text.starts_with('/') && !text.to_ascii_lowercase().starts_with("https://") {
        return "/".into();
    }
    if text.starts_with("//") || text.starts_with("/\\") {
        return "/".into();
    }
    for ch in text.chars() {
        if (ch as u32) < 0x20 || ch == '\u{7f}' || ch == '\\' {
            return "/".into();
        }
    }
    // 相对路径（无 scheme://）：只拒回登录页
    if !text.contains("://") {
        if path_of(text) == LOGIN_PATH {
            return "/".into();
        }
        return text.to_string();
    }
    // https:// 形态：只放自有域（scope 或其子域）
    let rest = text.strip_prefix("https://").unwrap_or(text);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let allow = !host.is_empty()
        && !domain_scope.is_empty()
        && (host == domain_scope || host.ends_with(&format!(".{domain_scope}")));
    if !allow || path_of(text) == LOGIN_PATH {
        return "/".into();
    }
    text.to_string()
}

fn path_of(url: &str) -> String {
    let no_scheme = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    let after_host = match no_scheme.find('/') {
        Some(i) => &no_scheme[i..],
        None => return "/".into(),
    };
    let path = after_host.split(['?', '#']).next().unwrap_or("/");
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".into()
    } else {
        trimmed.to_string()
    }
}

/// 按请求 Host 推导 Cookie 域：只对自有域生效；IP / localhost 一律不带 Domain。
fn cookie_domain(host: &str, domain_scope: &str) -> Option<String> {
    let h = host
        .split(':')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if h.is_empty() || domain_scope.is_empty() {
        return None;
    }
    if h == domain_scope || h.ends_with(&format!(".{domain_scope}")) {
        Some(format!(".{domain_scope}"))
    } else {
        None
    }
}

fn cookie_header(
    name: &str,
    value: &str,
    secure: bool,
    max_age: Option<i64>,
    delete: bool,
    domain: Option<&str>,
) -> (String, String) {
    let mut parts = vec![format!("{name}={value}"), "Path=/".to_string()];
    if let Some(d) = domain {
        parts.push(format!("Domain={d}"));
    }
    parts.push("HttpOnly".into());
    parts.push("SameSite=Lax".into());
    if secure {
        parts.push("Secure".into());
    }
    if delete {
        parts.push("Max-Age=0".into());
        parts.push("Expires=Thu, 01 Jan 1970 00:00:00 GMT".into());
    } else if let Some(ma) = max_age {
        parts.push(format!("Max-Age={ma}"));
    }
    ("Set-Cookie".into(), parts.join("; "))
}

/// limit 夹在 1..max；非数字退回默认。
fn clamp_limit(raw: Option<&str>, default: i64) -> i64 {
    let Some(raw) = raw else { return default };
    let raw = raw.trim();
    if raw.is_empty() {
        return default;
    }
    match raw.parse::<i64>() {
        Ok(v) => v.clamp(1, MAX_LIMIT),
        Err(_) => default,
    }
}

fn clamp_refresh(value: i64) -> i64 {
    if value <= 0 {
        0
    } else {
        value.max(5)
    }
}

/// query 串 → (k, v) 多值列表（`&` 分隔，k=v URL 解码）。
fn parse_query(query: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.push((url_decode(k), url_decode(v)));
    }
    out
}

fn url_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn first(query: &[(String, String)], name: &str) -> Option<String> {
    query
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.trim().to_string())
}

/// 请求里的 cookie 值（畸形 cookie 当没有）。
fn request_cookie(req: &Request, name: &str) -> String {
    let Some(raw) = req.header("Cookie") else {
        return String::new();
    };
    for piece in raw.split(';') {
        let piece = piece.trim();
        if let Some((k, v)) = piece.split_once('=') {
            if k.trim() == name {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

/// --trust-proxy 的逗号分隔串 → (ip 或 cidr, prefix_len) 列表；看不懂的段丢掉。
fn parse_trust_proxy(raw: &str) -> Vec<(va::IpAddr, u8)> {
    let mut out = Vec::new();
    for piece in raw.split(',') {
        let text = piece.trim();
        if text.is_empty() {
            continue;
        }
        let (addr_part, prefix) = match text.split_once('/') {
            Some((a, p)) => (a, p.parse::<u8>().ok()),
            None => (text, None),
        };
        if let Some(addr) = va::parse_ip(addr_part) {
            let max = match addr {
                va::IpAddr::V4(_) => 32,
                va::IpAddr::V6(_) => 128,
            };
            let plen = prefix.unwrap_or(max).min(max);
            out.push((addr, plen));
        } else {
            log_line(&format!(
                "WARN --trust-proxy 里这一段看不懂，已忽略：{text}"
            ));
        }
    }
    out
}

fn ip_in_nets(ip: &str, nets: &[(va::IpAddr, u8)]) -> bool {
    if nets.is_empty() {
        return false;
    }
    let Some(addr) = va::parse_ip(ip) else {
        return false;
    };
    for (net, plen) in nets {
        if addr_in_net(&addr, net, *plen) {
            return true;
        }
    }
    false
}

fn addr_in_net(addr: &va::IpAddr, net: &va::IpAddr, plen: u8) -> bool {
    match (addr, net) {
        (va::IpAddr::V4(a), va::IpAddr::V4(b)) => {
            let a = u32::from_be_bytes(*a);
            let b = u32::from_be_bytes(*b);
            if plen == 0 {
                return true;
            }
            let mask = if plen >= 32 {
                u32::MAX
            } else {
                u32::MAX << (32 - plen)
            };
            (a & mask) == (b & mask)
        }
        (va::IpAddr::V6(a), va::IpAddr::V6(b)) => {
            // v4 映射地址归一到 v4 判断（Python parse_ip 的 ipv4_mapped 语义）
            if let (Some(a4), Some(b4)) = (v4_mapped(a), v4_mapped(b)) {
                let a = u32::from_be_bytes(a4);
                let b = u32::from_be_bytes(b4);
                if plen == 0 {
                    return true;
                }
                let mask = if plen >= 32 {
                    u32::MAX
                } else {
                    u32::MAX << (32 - plen)
                };
                return (a & mask) == (b & mask);
            }
            let mut ab = [0u8; 16];
            let mut bb = [0u8; 16];
            for (i, w) in a.iter().enumerate() {
                ab[i * 2] = (w >> 8) as u8;
                ab[i * 2 + 1] = (w & 0xff) as u8;
            }
            for (i, w) in b.iter().enumerate() {
                bb[i * 2] = (w >> 8) as u8;
                bb[i * 2 + 1] = (w & 0xff) as u8;
            }
            let a = u128::from_be_bytes(ab);
            let b = u128::from_be_bytes(bb);
            if plen == 0 {
                return true;
            }
            let mask = if plen >= 128 {
                u128::MAX
            } else {
                u128::MAX << (128 - plen)
            };
            (a & mask) == (b & mask)
        }
        _ => false,
    }
}

fn v4_mapped(v6: &[u16; 8]) -> Option<[u8; 4]> {
    if v6[..5] == [0, 0, 0, 0, 0xffff] {
        let hi = v6[5];
        let lo = v6[6];
        return Some([
            (hi >> 8) as u8,
            (hi & 0xff) as u8,
            (lo >> 8) as u8,
            (lo & 0xff) as u8,
        ]);
    }
    None
}

// ================================================================ 渲染

fn render_page(refresh_seconds: i64, userbar: &str) -> String {
    let cfg = json!({"refresh": refresh_seconds, "version": VERSION});
    let page = include_str!("../../../hacs-c/assets/view_page.html").replace(
        "__CFG_JSON__",
        &hacs_l1_transport::web_gateway::dumps_python(&cfg)
            .iter()
            .map(|b| char::from(*b))
            .collect::<String>(),
    );
    if userbar.is_empty() {
        page
    } else {
        page.replacen("<body>", &format!("<body>\n{userbar}"), 1)
    }
}

fn render_portal(userbar: &str, domain: &str) -> String {
    let chat_url =
        std::env::var("AGENT_TEAM_CHAT_URL").unwrap_or_else(|_| format!("https://chat.{domain}/"));
    let card = |icon: &str, title: &str, desc: &str, href: &str, cta: &str| {
        format!(
            "<a class=\"card\" href=\"{href}\">\n  <div class=\"ico\" aria-hidden=\"true\">{icon}</div>\n  <div class=\"ttl\">{title}<span class=\"badge\">已开通</span></div>\n  <p class=\"desc\">{desc}</p>\n  <div class=\"cta\">{cta}</div>\n</a>",
            icon = icon,
            title = html_esc(title),
            desc = html_esc(desc),
            href = html_esc(href),
            cta = html_esc(cta),
        )
    };
    let cards = [
        card(
            "🗂",
            "团队看板",
            "白板 · 会议 · 共享内容 · 跟踪。只读，看的是黑板上的留痕与结论。",
            BOARD_PATH,
            "进入看板",
        ),
        card(
            "💬",
            "人机对话台",
            "群聊：和人、和各路 Agent 在同一个会话里说话，会议也在里面开。",
            &chat_url,
            "进入对话台",
        ),
    ];
    let page = include_str!("../../../hacs-c/assets/view_portal.html")
        .replacen("__CARDS__", &cards.join("\n"), 1)
        .replacen("__USERBAR__", userbar, 1);
    page.replace("example.com", domain)
}

fn shell_html(title: &str, body: &str) -> String {
    include_str!("../../../hacs-c/assets/view_shell.html")
        .replace("__TITLE__", &html_esc(title))
        .replace("__BODY__", body)
}

fn banner(text: &str, good: bool) -> String {
    if text.is_empty() {
        return String::new();
    }
    format!(
        "<p class=\"banner {}\">{}</p>",
        if good { "good" } else { "bad" },
        html_esc(text)
    )
}

fn render_login(
    csrf_token: &str,
    next_url: &str,
    error: &str,
    notice: &str,
    username: &str,
    domain_scope: &str,
) -> String {
    let body = "<h1>团队门户 · 登录</h1>\n\
<p class=\"sub\">HACS 平台 · 一个入口，进各自的系统</p>\n".to_string()
        + &banner(notice, true)
        + &banner(error, false)
        + &format!(
            "<form method=\"post\" action=\"{LOGIN_PATH}\" accept-charset=\"utf-8\">\n\
<input type=\"hidden\" name=\"action\" value=\"login\">\n\
<input type=\"hidden\" name=\"next\" value=\"{}\">\n\
<input type=\"hidden\" name=\"{CSRF_FIELD}\" value=\"{}\">\n\
<label for=\"user\">账号</label>\n\
<input id=\"user\" name=\"user\" type=\"text\" value=\"{}\" required autofocus\n       autocapitalize=\"off\" autocorrect=\"off\" spellcheck=\"false\"\n       autocomplete=\"username\" maxlength=\"64\">\n\
<label for=\"password\">口令</label>\n\
<input id=\"password\" name=\"password\" type=\"password\" required\n       autocomplete=\"current-password\" maxlength=\"256\">\n\
<button type=\"submit\">登录</button>\n\
</form>\n\
<p class=\"foot\">门户只做入口、只读：登录后进的是看板与人机对话台，写黑板的入口这里一个都没有。忘了口令请找主人用 <code>view_auth.py passwd</code> 重置。</p>",
            html_esc(&safe_next(next_url, domain_scope)),
            html_esc(csrf_token),
            html_esc(username),
        );
    shell_html("团队门户 · 登录", &body)
}

fn render_account(
    username: &str,
    role: &str,
    csrf_token: &str,
    notice: &str,
    error: &str,
) -> String {
    let role_cn = va::role_cn(role);
    let hidden = format!(
        "<input type=\"hidden\" name=\"{CSRF_FIELD}\" value=\"{}\">\n",
        html_esc(csrf_token)
    );
    let body = "<h1>账号</h1>\n".to_string()
        + &format!(
            "<p class=\"sub\">当前登录：{}（{}）</p>\n",
            html_esc(username),
            html_esc(role_cn)
        )
        + &banner(notice, true)
        + &banner(error, false)
        + &format!("<form method=\"post\" action=\"{LOGIN_PATH}\" accept-charset=\"utf-8\">\n<input type=\"hidden\" name=\"action\" value=\"passwd\">\n{hidden}<label for=\"old\">原口令</label>\n<input id=\"old\" name=\"old\" type=\"password\" required autocomplete=\"current-password\">\n<label for=\"new\">新口令（至少 {} 位）</label>\n<input id=\"new\" name=\"new\" type=\"password\" required autocomplete=\"new-password\"\n       minlength=\"{}\" maxlength=\"{}\">\n<button type=\"submit\">修改口令</button>\n</form>\n<hr>\n<form method=\"post\" action=\"{LOGIN_PATH}\" accept-charset=\"utf-8\">\n<input type=\"hidden\" name=\"action\" value=\"logout\">\n{hidden}<button type=\"submit\" class=\"ghost\">退出登录</button>\n</form>\n<p class=\"foot\"><a href=\"{BOARD_PATH}\">← 回看板</a>　改密会立刻作废你所有的登录（包括手上这台设备），改完要用新口令重新登录。</p>",
            va::PASSWORD_MIN,
            va::PASSWORD_MIN,
            va::PASSWORD_MAX,
        );
    shell_html("团队黑板 · 账号", &body)
}

fn render_userbar(name: &str, role_cn: &str, csrf_token: &str) -> String {
    let bar_style = "position:sticky;top:0;z-index:50;display:flex;gap:12px;align-items:center;\
justify-content:flex-end;flex-wrap:wrap;padding:6px 14px;background:#1c212b;\
border-bottom:1px solid #2a3140;font-size:13px;color:#98a2b3";
    let bar_link = "color:#5b9cff;text-decoration:none;min-height:24px;line-height:24px";
    let bar_btn = "background:transparent;border:1px solid #2a3140;border-radius:8px;\
color:#e7eaf0;padding:2px 12px;min-height:28px;font-size:13px;cursor:pointer";
    let who = format!("<span>{} · {}</span>", html_esc(name), html_esc(role_cn));
    if csrf_token.is_empty() {
        return format!("<div style=\"{bar_style}\">{who}</div>");
    }
    format!(
        "<form method=\"post\" action=\"{LOGIN_PATH}\" accept-charset=\"utf-8\" style=\"{bar_style}\">\n{who}\n<a href=\"/\" style=\"{bar_link}\">门户</a>\n<a href=\"{ACCOUNT_PATH}\" style=\"{bar_link}\">改密</a>\n<input type=\"hidden\" name=\"action\" value=\"logout\">\n<input type=\"hidden\" name=\"{CSRF_FIELD}\" value=\"{}\">\n<button type=\"submit\" style=\"{bar_btn}\">退出</button>\n</form>",
        html_esc(csrf_token),
    )
}

// ================================================================ Handler

struct Handler {
    rt: Runtime,
    trust_proxy: Vec<(va::IpAddr, u8)>,
}

struct Ctx<'a> {
    h: &'a Handler,
    req: &'a Request,
    peer_ip: String,
    client_ip: String,
    ua: String,
    visitor: Option<Visitor>,
    started: Instant,
}

impl<'a> Ctx<'a> {
    fn auth_on(&self) -> bool {
        self.h.rt.mode == "public" && self.h.rt.auth.is_some()
    }

    fn role(&self) -> Option<&str> {
        self.visitor.as_ref().and_then(|v| v.role.as_deref())
    }

    fn query(&self) -> Vec<(String, String)> {
        parse_query(&self.req.query)
    }

    fn secure(&self) -> bool {
        if self.h.rt.secure_cookie {
            return true;
        }
        self.req
            .header("X-Forwarded-Proto")
            .unwrap_or("")
            .split(',')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("https")
    }

    fn cookie_domain(&self) -> Option<String> {
        cookie_domain(
            self.req.header("Host").unwrap_or(""),
            &self.h.rt.domain_scope,
        )
    }

    /// 外层反代声称的访客 IP（原样，不校验不截断）。
    fn claimed_ip(&self) -> String {
        if let Some(cf) = self.req.header("CF-Connecting-IP") {
            return cf.trim().to_string();
        }
        self.req
            .header("X-Forwarded-For")
            .unwrap_or("")
            .split(',')
            .next()
            .unwrap_or("")
            .trim()
            .to_string()
    }

    fn audit(&self, event: &str, username: Option<&str>, detail: &str) {
        let Some(auth) = &self.h.rt.auth else { return };
        let mut detail = detail.to_string();
        let shown = self.client_ip.clone();
        let peer = self.peer_ip.clone();
        if shown != peer {
            let note = format!(
                "来源 {peer}(via {})",
                truncate_chars(&self.claimed_ip(), TRUST_HEADER_MAX)
            );
            detail = if detail.is_empty() {
                note
            } else {
                format!("{detail} ｜ {note}")
            };
        }
        auth.audit(event, username, &self.client_ip, &self.ua, &detail);
    }

    fn resolve_visitor(&mut self) {
        // ⚠️ 用对端 IP（不看任何头）；127.0.0.1 不在免登录名单里。
        if va::in_tailnet(&self.peer_ip) {
            self.visitor = Some(Visitor::tailnet());
            return;
        }
        let cookie = request_cookie(self.req, va::SESSION_COOKIE);
        if cookie.is_empty() {
            return;
        }
        let Some(auth) = &self.h.rt.auth else { return };
        let (principal, _reason) = auth.check_cookie(&cookie, &self.client_ip, &self.ua);
        if let Some(p) = principal {
            self.visitor = Some(Visitor {
                username: Some(p.username),
                role: Some(p.role),
                source: "session".into(),
            });
        }
    }

    fn reply_bytes(
        &self,
        status: u16,
        content_type: &str,
        body: Vec<u8>,
        extra: Vec<(String, String)>,
    ) -> Reply {
        let mut headers = vec![("Content-Type".to_string(), content_type.to_string())];
        for (k, v) in SECURITY_HEADERS {
            headers.push((k.to_string(), v.to_string()));
        }
        headers.extend(extra);
        Reply {
            status,
            body: hacs_l1_transport::web_gateway::Body::Bytes(body),
            headers,
        }
    }

    fn json(&self, status: u16, payload: &Value) -> Reply {
        let mut masked = payload.clone();
        bv::apply_role(&mut masked, self.role());
        self.reply_bytes(status, JSON_CT, json_bytes_py(&masked), vec![])
    }

    fn html(&self, status: u16, text: &str, extra: Vec<(String, String)>) -> Reply {
        self.reply_bytes(status, HTML_CT, text.as_bytes().to_vec(), extra)
    }

    fn redirect(&self, location: &str, extra: Vec<(String, String)>) -> Reply {
        let mut headers = vec![("Location".to_string(), location.to_string())];
        headers.extend(extra);
        self.reply_bytes(302, TEXT_CT, Vec::new(), headers)
    }

    fn reject_write(&self) -> Reply {
        self.audit(
            "write_blocked",
            None,
            &format!("拦下写请求 {} {}", self.req.method, self.req.path),
        );
        self.json(
            405,
            &json!({
                "ok": false,
                "error": "method_not_allowed",
                "detail": "看板只读：只接受 GET / HEAD，不提供任何写入黑板的入口",
            }),
        )
    }

    // --- 上游反代 -------------------------------------------------------

    fn upstream_url(&self, path: &str, params: &[(&str, String)]) -> String {
        let query: Vec<String> = params
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| format!("{}={}", http_client::quote(k), http_client::quote(v)))
            .collect();
        let mut url = format!(
            "{}/{}",
            self.h.rt.upstream.trim_end_matches('/'),
            path.trim_start_matches('/')
        );
        if !query.is_empty() {
            url.push('?');
            url.push_str(&query.join("&"));
        }
        url
    }

    fn upstream_get(
        &self,
        path: &str,
        params: &[(&str, String)],
        allow_status: &[u16],
    ) -> (u16, Value) {
        let url = self.upstream_url(path, params);
        let mut headers: Vec<(&str, &str)> = vec![("Accept", "application/json")];
        let bearer;
        if !self.h.rt.token.is_empty() {
            bearer = format!("Bearer {}", self.h.rt.token);
            headers.push(("Authorization", bearer.as_str()));
        }
        let started = Instant::now();
        match http_client::get(&url, &headers, Duration::from_secs(UPSTREAM_TIMEOUT_S)) {
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("超时") || msg.contains("timed out") {
                    return (
                        502,
                        json!({"ok": false, "error": "upstream_timeout",
                               "detail": format!("上游 {UPSTREAM_TIMEOUT_S}s 内没有响应")}),
                    );
                }
                (
                    502,
                    json!({"ok": false, "error": "upstream_unreachable",
                           "detail": scrub(&format!("连不上黑板：{msg}"), &self.h.rt.token)}),
                )
            }
            Ok(resp) => {
                if allow_status.contains(&resp.status) {
                    let payload: Value = serde_json::from_slice(&resp.body).unwrap_or(json!({}));
                    let payload = if payload.is_object() {
                        payload
                    } else {
                        json!({"ok": false, "error": format!("upstream_status_{}", resp.status)})
                    };
                    let mut p = payload;
                    if p.get("ok").is_none() {
                        p["ok"] = json!(false);
                    }
                    return (resp.status, p);
                }
                if resp.status == 401 || resp.status == 403 {
                    return (
                        502,
                        json!({"ok": false, "error": "upstream_error", "upstream_status": resp.status,
                               "detail": format!("上游拒绝了看板的凭据（{}）：检查 --token-file 指向的 token", resp.status)}),
                    );
                }
                if resp.body.len() > UPSTREAM_MAX_BYTES {
                    return (
                        502,
                        json!({"ok": false, "error": "upstream_too_large",
                               "detail": format!("上游响应超过 {UPSTREAM_MAX_BYTES} 字节")}),
                    );
                }
                let payload: Result<Value, _> = serde_json::from_slice(&resp.body);
                match payload {
                    Err(e) => (
                        502,
                        json!({"ok": false, "error": "upstream_bad_json",
                               "detail": scrub(&format!("上游返回的不是 JSON：{e}"), &self.h.rt.token)}),
                    ),
                    Ok(p) => {
                        if !p.is_object() {
                            return (
                                502,
                                json!({"ok": false, "error": "upstream_bad_json",
                                       "detail": "上游返回的 JSON 不是对象"}),
                            );
                        }
                        let mut p = p;
                        if resp.status != 200 {
                            p["detail"] = json!(scrub(
                                &format!("上游返回 {}：{}", resp.status, ""),
                                &self.h.rt.token
                            ));
                            return (502, p);
                        }
                        p["upstream_ms"] = json!(started.elapsed().as_millis() as i64);
                        (200, p)
                    }
                }
            }
        }
    }

    // --- 登录页 / 账号 ---------------------------------------------------

    fn csrf(&self) -> (String, Vec<(String, String)>) {
        let Some(auth) = &self.h.rt.auth else {
            return (String::new(), vec![]);
        };
        let current = request_cookie(self.req, va::CSRF_COOKIE);
        if !current.is_empty() && va::unsign_token(&current, &auth.key, b"csrf").is_some() {
            return (current, vec![]);
        }
        let token = va::issue_csrf(&auth.key);
        let header = cookie_header(
            va::CSRF_COOKIE,
            &token,
            self.secure(),
            None,
            false,
            self.cookie_domain().as_deref(),
        );
        (token, vec![header])
    }

    fn userbar(&self) -> (String, Vec<(String, String)>) {
        let Some(visitor) = &self.visitor else {
            return (String::new(), vec![]);
        };
        if visitor.source == "tailnet" {
            return (render_userbar("内网直连", "tailnet 免登录", ""), vec![]);
        }
        let (token, headers) = self.csrf();
        let who = va::role_cn(visitor.role.as_deref().unwrap_or(""));
        (
            render_userbar(visitor.username.as_deref().unwrap_or(""), who, &token),
            headers,
        )
    }

    fn login_page(
        &self,
        code: u16,
        error: &str,
        notice: &str,
        next_url: &str,
        username: &str,
    ) -> Reply {
        let (token, headers) = self.csrf();
        self.html(
            code,
            &render_login(
                &token,
                next_url,
                error,
                notice,
                username,
                &self.h.rt.domain_scope,
            ),
            headers,
        )
    }

    fn read_form(&self) -> Option<Vec<(String, String)>> {
        if self.req.body.is_empty() || self.req.body.len() > FORM_MAX_BYTES {
            return None;
        }
        let ctype = self
            .req
            .header("Content-Type")
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if ctype != "application/x-www-form-urlencoded" {
            return None;
        }
        let text = String::from_utf8_lossy(&self.req.body);
        Some(parse_query(&text))
    }

    // --- 各路由 ---------------------------------------------------------

    fn portal(&self) -> Reply {
        let (bar, headers) = self.userbar();
        self.html(200, &render_portal(&bar, &self.h.rt.domain_scope), headers)
    }

    fn board(&self) -> Reply {
        let (bar, headers) = self.userbar();
        self.html(200, &render_page(self.h.rt.refresh_seconds, &bar), headers)
    }

    fn healthz(&self) -> Reply {
        self.json(
            200,
            &json!({
                "ok": true, "version": VERSION, "service": "blackboard_view",
                "upstream": self.h.rt.upstream,
                "token_loaded": self.h.rt.token_loaded,
                "refresh_seconds": self.h.rt.refresh_seconds,
            }),
        )
    }

    fn favicon(&self) -> Reply {
        self.reply_bytes(204, "image/x-icon", Vec::new(), vec![])
    }

    fn api_health(&self) -> Reply {
        let (code, payload) = self.upstream_get("/healthz", &[], &[]);
        self.json(code, &payload)
    }

    fn api_entries(&self) -> Reply {
        let q = self.query();
        let params = [
            ("topic", first(&q, "topic").unwrap_or_default()),
            ("kind", first(&q, "kind").unwrap_or_default()),
            ("author", first(&q, "author").unwrap_or_default()),
            ("since", first(&q, "since").unwrap_or_default()),
            (
                "limit",
                clamp_limit(first(&q, "limit").as_deref(), DEFAULT_LIMIT).to_string(),
            ),
        ];
        let (mut code, mut payload) = self.upstream_get("/entries", &params, &[]);
        if code == 200 {
            payload["mode"] = json!("entries");
        }
        self.json(code, &payload)
    }

    fn api_search(&self) -> Reply {
        let q = self.query();
        let q_raw = first(&q, "q").unwrap_or_default();
        let q_raw = truncate_chars(&q_raw, QUERY_MAX);
        let limit = clamp_limit(first(&q, "limit").as_deref(), DEFAULT_LIMIT);
        let topic = first(&q, "topic").unwrap_or_default();
        if q_raw.is_empty() {
            let (code, mut payload) = self.upstream_get(
                "/entries",
                &[("topic", topic), ("limit", limit.to_string())],
                &[],
            );
            if code == 200 {
                payload["mode"] = json!("entries");
            }
            return self.json(code, &payload);
        }
        let (mut code, mut payload) = self.upstream_get(
            "/search",
            &[("q", q_raw), ("topic", topic), ("limit", limit.to_string())],
            &[],
        );
        if code == 200 && payload.get("mode").is_none() {
            payload["mode"] = json!("search");
        }
        self.json(code, &payload)
    }

    fn api_sessions(&self) -> Reply {
        let q = self.query();
        let params = [
            ("state", first(&q, "state").unwrap_or_default()),
            ("topic", first(&q, "topic").unwrap_or_default()),
            ("since", first(&q, "since").unwrap_or_default()),
            (
                "limit",
                clamp_limit(first(&q, "limit").as_deref(), DEFAULT_LIMIT).to_string(),
            ),
        ];
        let (code, payload) = self.upstream_get("/sessions", &params, &[]);
        self.json(code, &payload)
    }

    fn api_session_detail(&self, sid: &str) -> Reply {
        let sid = url_decode(sid).trim().to_string();
        if sid.is_empty() {
            return self.json(
                404,
                &json!({"ok": false, "error": "not_found", "detail": "没有这场会议"}),
            );
        }
        let limit = clamp_limit(first(&self.query(), "limit").as_deref(), MAX_LIMIT);
        let path = format!("/sessions/{}", http_client::quote(&sid));
        let (code, payload) = self.upstream_get(&path, &[("limit", limit.to_string())], &[404]);
        self.json(code, &payload)
    }

    fn track_params(&self) -> Vec<(&'static str, String)> {
        let q = self.query();
        let overdue = first(&q, "overdue")
            .unwrap_or_default()
            .to_ascii_lowercase();
        vec![
            ("status", first(&q, "status").unwrap_or_default()),
            ("owner", first(&q, "owner").unwrap_or_default()),
            ("kind", first(&q, "kind").unwrap_or_default()),
            (
                "overdue",
                if ["1", "true", "yes", "on"].contains(&overdue.as_str()) {
                    "1".into()
                } else {
                    String::new()
                },
            ),
            (
                "limit",
                clamp_limit(first(&q, "limit").as_deref(), MAX_LIMIT).to_string(),
            ),
        ]
    }

    fn api_tracking(&self) -> Reply {
        let mut params = self.track_params();
        params.retain(|(k, _)| *k != "status" && *k != "kind");
        let (code, payload) = self.upstream_get("/tracking", &params, &[]);
        self.json(code, &payload)
    }

    fn api_tracking_list(&self, kind: &str) -> Reply {
        let mut params = self.track_params();
        if kind != "issues" {
            params.retain(|(k, _)| *k != "kind");
        }
        let (code, payload) = self.upstream_get(&format!("/{kind}"), &params, &[]);
        self.json(code, &payload)
    }

    fn shared_params(&self) -> Vec<(&'static str, String)> {
        let q = self.query();
        let mut scope = first(&q, "scope").unwrap_or_default();
        if scope != "task" && scope != "permanent" {
            scope = String::new();
        }
        vec![
            ("scope", scope),
            ("kind", first(&q, "kind").unwrap_or_default()),
            ("owner", first(&q, "owner").unwrap_or_default()),
            ("tag", first(&q, "tag").unwrap_or_default()),
            (
                "limit",
                clamp_limit(first(&q, "limit").as_deref(), MAX_LIMIT).to_string(),
            ),
        ]
    }

    fn shared_out(&self, code: u16, mut payload: Value) -> Reply {
        if code == 404 {
            return self.json(
                502,
                &json!({"ok": false, "error": "shared_unavailable",
                        "detail": "上游还没有 /shared 端点：共享内容服务尚未上线"}),
            );
        }
        if code == 200 {
            bv::mask_shared(&mut payload, self.role());
        }
        self.json(code, &payload)
    }

    fn api_shared(&self) -> Reply {
        let (code, payload) = self.upstream_get("/shared", &self.shared_params(), &[404]);
        self.shared_out(code, payload)
    }

    fn api_shared_search(&self) -> Reply {
        let q = self.query();
        let q_raw = truncate_chars(&first(&q, "q").unwrap_or_default(), QUERY_MAX);
        let mut params = self.shared_params();
        if q_raw.is_empty() {
            let (code, payload) = self.upstream_get("/shared", &params, &[404]);
            return self.shared_out(code, payload);
        }
        let semantic = first(&q, "semantic")
            .unwrap_or_default()
            .to_ascii_lowercase();
        params.push(("q", q_raw));
        params.push((
            "semantic",
            if ["1", "true", "yes", "on"].contains(&semantic.as_str()) {
                "1".into()
            } else {
                String::new()
            },
        ));
        let (code, payload) = self.upstream_get("/shared/search", &params, &[404]);
        self.shared_out(code, payload)
    }

    fn login_get(&mut self) -> Reply {
        self.resolve_visitor();
        let next_url = safe_next(
            &first(&self.query(), "next").unwrap_or_default(),
            &self.h.rt.domain_scope,
        );
        let notice;
        let username;
        if let Some(v) = &self.visitor {
            if v.source == "session" {
                let name = v.username.clone().unwrap_or_default();
                notice = format!("当前已登录为 {name}，可直接进看板，或在下面换个账号登录");
                username = name;
                return self.login_page(200, "", &notice, &next_url, &username);
            }
        }
        notice = String::new();
        username = String::new();
        self.login_page(200, "", &notice, &next_url, &username)
    }

    fn account_get(&mut self) -> Reply {
        self.resolve_visitor();
        let ok = self
            .visitor
            .as_ref()
            .map(|v| v.source == "session")
            .unwrap_or(false);
        if !ok {
            return self.redirect(
                &format!("{LOGIN_PATH}?next={}", http_client::quote(ACCOUNT_PATH)),
                vec![],
            );
        }
        let (token, headers) = self.csrf();
        let v = self.visitor.as_ref().unwrap();
        let msg = first(&self.query(), "msg").unwrap_or_default();
        self.html(
            200,
            &render_account(
                v.username.as_deref().unwrap_or(""),
                v.role.as_deref().unwrap_or(""),
                &token,
                &msg,
                "",
            ),
            headers,
        )
    }

    fn login_post(&mut self) -> Reply {
        let Some(form) = self.read_form() else {
            self.audit(
                "login_fail",
                None,
                &format!("表单被拒：类型不对或超过 {FORM_MAX_BYTES} 字节"),
            );
            return self.login_page(400, "表单格式不对，请从登录页重新提交", "", "/", "");
        };
        let get = |name: &str| -> String { first(&form, name).unwrap_or_default() };
        let mut action = get("action").trim().to_ascii_lowercase();
        if !LOGIN_ACTIONS.contains(&action.as_str()) {
            action = "login".into();
        }
        let username = truncate_chars(get("user").trim(), 64);
        let next_url = safe_next(&get("next"), &self.h.rt.domain_scope);

        let Some(auth) = &self.h.rt.auth else {
            return self.reject_write();
        };
        let csrf_ok = va::check_csrf(
            &request_cookie(self.req, va::CSRF_COOKIE),
            &get(CSRF_FIELD),
            &auth.key,
        );
        if !csrf_ok {
            let who = if username.is_empty() {
                None
            } else {
                Some(username.as_str())
            };
            self.audit(
                "csrf_reject",
                who,
                &format!("CSRF 校验不通过：action={action}"),
            );
            return self.login_page(
                403,
                "表单已过期或来源不对，请重新提交",
                "",
                &next_url,
                &username,
            );
        }

        if action == "logout" {
            auth.logout(
                &request_cookie(self.req, va::SESSION_COOKIE),
                &self.client_ip,
                &self.ua,
            );
            let header = cookie_header(
                va::SESSION_COOKIE,
                "",
                self.secure(),
                None,
                true,
                self.cookie_domain().as_deref(),
            );
            return self.redirect(LOGIN_PATH, vec![header]);
        }

        if action == "passwd" {
            self.resolve_visitor();
            let ok = self
                .visitor
                .as_ref()
                .map(|v| v.source == "session")
                .unwrap_or(false);
            if !ok {
                return self.redirect(
                    &format!("{LOGIN_PATH}?next={}", http_client::quote(ACCOUNT_PATH)),
                    vec![],
                );
            }
            let who = self
                .visitor
                .as_ref()
                .unwrap()
                .username
                .clone()
                .unwrap_or_default();
            let (ok, message) =
                auth.change_password(&who, &get("old"), &get("new"), &self.client_ip, &self.ua);
            if !ok {
                let (token, headers) = self.csrf();
                let v = self.visitor.as_ref().unwrap();
                return self.html(
                    200,
                    &render_account(
                        v.username.as_deref().unwrap_or(""),
                        v.role.as_deref().unwrap_or(""),
                        &token,
                        "",
                        &message,
                    ),
                    headers,
                );
            }
            // 改密成功 = 所有会话（含自己手上这张）作废，cookie 一起清掉
            let (token, mut headers) = self.csrf();
            headers.push(cookie_header(
                va::SESSION_COOKIE,
                "",
                self.secure(),
                None,
                true,
                self.cookie_domain().as_deref(),
            ));
            return self.html(
                200,
                &render_login(&token, "/", "", &message, "", &self.h.rt.domain_scope),
                headers,
            );
        }

        // action == login
        let (ok, cookie_value, reason) =
            auth.login(&username, &get("password"), &self.client_ip, &self.ua);
        if !ok {
            return self.login_page(
                200,
                if reason.is_empty() {
                    "账号或口令不正确"
                } else {
                    &reason
                },
                "",
                &next_url,
                &username,
            );
        }
        let header = cookie_header(
            va::SESSION_COOKIE,
            cookie_value.as_deref().unwrap_or(""),
            self.secure(),
            Some(va::SESSION_TTL_S),
            false,
            self.cookie_domain().as_deref(),
        );
        self.redirect(&next_url, vec![header])
    }

    fn route_404(&self) -> Reply {
        self.json(
            404,
            &json!({"ok": false, "error": "not_found",
                    "detail": "看板只有 / 、/api/health、/api/entries、/api/search、/api/sessions、/api/tracking、/api/actions、/api/issues、/api/shared、/api/shared/search"}),
        )
    }
}

// ================================================================ 网关装配

/// 门禁：只对读方法生效；写方法由路由表兜到 405。
fn auth_gate(h: &Handler, ctx: &mut Ctx) -> Option<Reply> {
    if !["GET", "HEAD"].contains(&ctx.req.method.as_str()) {
        return None;
    }
    ctx.visitor = None;
    if !ctx.auth_on() {
        return None;
    }
    if ctx.req.path == "/healthz" {
        return None; // 探活免登录
    }
    if ctx.req.path == LOGIN_PATH {
        return Some(ctx.login_get());
    }
    ctx.resolve_visitor();
    if ctx.visitor.is_none() {
        if ctx.req.path != "/favicon.ico" {
            ctx.audit(
                "access_denied",
                None,
                &format!("未登录访问 {} {}，已跳登录页", ctx.req.method, ctx.req.path),
            );
        }
        let next = http_client::quote(&safe_next(
            &format!("{}?{}", ctx.req.raw_path, ctx.req.query),
            &h.rt.domain_scope,
        ));
        return Some(ctx.redirect(&format!("{LOGIN_PATH}?next={next}"), vec![]));
    }
    if ctx.req.path == ACCOUNT_PATH {
        return Some(ctx.account_get());
    }
    None
}

fn make_ctx<'a>(h: &'a Handler, req: &'a Request) -> Ctx<'a> {
    Ctx {
        h,
        req,
        peer_ip: peer_ip_of(req),
        client_ip: client_ip_of(h, req),
        ua: ua_of(req),
        visitor: None,
        started: Instant::now(),
    }
}

fn build_gateway(rt: Runtime, trust_proxy: Vec<(va::IpAddr, u8)>) -> Gateway {
    let handler = Arc::new(Handler { rt, trust_proxy });
    let mut gw = Gateway::new("blackboard_view", Some(VERSION), true);
    gw.protocol_version = "HTTP/1.1".into();

    let h2 = handler.clone();
    gw.auth(move |req| {
        let mut ctx = make_ctx(&h2, req);
        Ok(auth_gate(&h2, &mut ctx))
    });

    // 路由登记：读方法共用一套处理函数；POST /login 是唯一写入口；
    // 根路径上的写方法登记兜出 405（不是 501）。
    macro_rules! route {
        ($path:expr, $f:expr) => {
            let h = handler.clone();
            gw.route($path, &["GET", "HEAD"], false, move |req| {
                // auth_gate 的 Ctx 与这里是两个对象，visitor 解析结果带不过来，
                // 所以各路由自己再解析一次（Python 在 do_GET 里只解析一次，
                // 这里多一次会话查库，语义等价）。
                let mut ctx = make_ctx(&h, req);
                ctx.resolve_visitor();
                Ok($f(&ctx))
            });
        };
    }
    route!("/", |c: &Ctx| c.portal());
    route!("/portal", |c: &Ctx| c.portal());
    route!("/board", |c: &Ctx| c.board());
    route!("/healthz", |c: &Ctx| c.healthz());
    route!("/favicon.ico", |c: &Ctx| c.favicon());
    route!("/api/health", |c: &Ctx| c.api_health());
    route!("/api/entries", |c: &Ctx| c.api_entries());
    route!("/api/search", |c: &Ctx| c.api_search());
    route!("/api/sessions", |c: &Ctx| c.api_sessions());
    route!("/api/tracking", |c: &Ctx| c.api_tracking());
    route!("/api/actions", |c: &Ctx| c.api_tracking_list("actions"));
    route!("/api/issues", |c: &Ctx| c.api_tracking_list("issues"));
    route!("/api/shared", |c: &Ctx| c.api_shared());
    route!("/api/shared/search", |c: &Ctx| c.api_shared_search());
    // /api/sessions/<id>：恰好三段
    let h = handler.clone();
    gw.route("/api/sessions", &["GET", "HEAD"], true, move |req| {
        let parts: Vec<&str> = req.path.split('/').filter(|p| !p.is_empty()).collect();
        let ctx = make_ctx(&h, req);
        if parts.len() != 3 {
            return Ok(ctx.route_404());
        }
        Ok(ctx.api_session_detail(parts[2]))
    });
    // POST /login（唯一写入口）：认证没装（tailnet）时 405
    let h = handler.clone();
    gw.route(LOGIN_PATH, &["POST"], false, move |req| {
        let mut ctx = make_ctx(&h, req);
        if ctx.auth_on() {
            Ok(ctx.login_post())
        } else {
            Ok(ctx.reject_write())
        }
    });
    // 其余路径上的写方法：405（只读纪律，不看登录状态）
    let h = handler.clone();
    gw.route(
        "/",
        &["PUT", "PATCH", "DELETE", "OPTIONS"],
        false,
        move |_req| {
            let ctx = Ctx {
                h: &h,
                req: _req,
                peer_ip: peer_ip_of(_req),
                client_ip: client_ip_of(&h, _req),
                ua: ua_of(_req),
                visitor: None,
                started: Instant::now(),
            };
            Ok(ctx.reject_write())
        },
    );
    // 未登记的读路径：404 文案
    let h = handler.clone();
    gw.not_found(move |req| {
        let ctx = make_ctx(&h, req);
        if ["GET", "HEAD"].contains(&req.method.as_str()) {
            Ok(ctx.route_404())
        } else {
            Ok(ctx.reject_write())
        }
    });
    gw
}

fn peer_ip_of(req: &Request) -> String {
    // web_gateway 的 client 字段即 "IP:port"（这里剥端口）
    req.client
        .rsplit_once(':')
        .map(|(ip, _)| ip.trim_start_matches('[').trim_end_matches(']').to_string())
        .unwrap_or_else(|| req.client.clone())
}

fn ua_of(req: &Request) -> String {
    truncate_chars(req.header("User-Agent").unwrap_or(""), 300)
}

fn client_ip_of(h: &Handler, req: &Request) -> String {
    let peer = peer_ip_of(req);
    if !ip_in_nets(&peer, &h.trust_proxy) {
        return peer;
    }
    // 外层声称的头
    let claimed = req
        .header("CF-Connecting-IP")
        .map(str::to_string)
        .unwrap_or_else(|| {
            req.header("X-Forwarded-For")
                .unwrap_or("")
                .split(',')
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        });
    if claimed.is_empty() || claimed.chars().count() > TRUST_HEADER_MAX {
        return peer;
    }
    match va::parse_ip(&claimed) {
        Some(va::IpAddr::V4(o)) => o
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join("."),
        Some(va::IpAddr::V6(w)) => w
            .iter()
            .map(|g| format!("{g:x}"))
            .collect::<Vec<_>>()
            .join(":"),
        None => peer,
    }
}

fn read_dsn_file(path: &str) -> String {
    // 文件里形如 BB_PG_DSN=host=... port=...（shell env 形态），取等号后那段
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("BB_PG_DSN=") {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    String::new()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut host, mut port) = ("127.0.0.1".to_string(), DEFAULT_PORT);
    let mut mode = "public".to_string();
    let mut upstream = node_profile::blackboard_base_default();
    let mut token_file: Option<String> = None;
    let mut session_key_file: Option<String> = None;
    let mut dsn_file: Option<String> = None;
    let mut secure_cookie = false;
    let mut trust_proxy_raw = String::new();
    let mut refresh_seconds = 30i64;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--host" => host = args.next().unwrap_or(host),
            "--port" => port = args.next().and_then(|v| v.parse().ok()).unwrap_or(port),
            "--mode" => mode = args.next().unwrap_or(mode),
            "--upstream" => upstream = args.next().unwrap_or(upstream),
            "--token-file" => token_file = args.next(),
            "--session-key-file" => session_key_file = args.next(),
            "--dsn-file" => dsn_file = args.next(),
            "--trust-proxy" => trust_proxy_raw = args.next().unwrap_or_default(),
            "--refresh-seconds" => {
                refresh_seconds = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(refresh_seconds)
            }
            "--secure-cookie" => secure_cookie = true,
            _ => {}
        }
    }
    if !upstream.starts_with("http://") && !upstream.starts_with("https://") {
        eprintln!("--upstream 必须是 http/https 地址，拿到的是 {upstream:?}");
        std::process::exit(2);
    }
    let refresh_seconds = clamp_refresh(refresh_seconds);
    let domain_scope = std::env::var("BBVIEW_COOKIE_DOMAIN")
        .or_else(|_| std::env::var("AGENT_TEAM_DOMAIN"))
        .unwrap_or_else(|_| "example.com".into())
        .trim()
        .to_ascii_lowercase();

    let trust_proxy = parse_trust_proxy(&trust_proxy_raw);
    if !trust_proxy.is_empty() {
        log_line(&format!(
            "INFO 受信反代：{} 段 —— 锁定键与审计 IP 采信 CF-Connecting-IP / X-Forwarded-For（免登录白名单仍只看对端 IP）",
            trust_proxy.len()
        ));
    }

    // token 读不到不退出：页面照样开得起来，只是每个请求都亮"上游拒绝凭据"的黄条。
    let token = token_file
        .as_deref()
        .map(|p| {
            std::fs::read_to_string(p)
                .map(|t| t.trim().to_string())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let token_loaded = !token.is_empty();
    if !token_loaded {
        log_line("WARN token 为空，上游会一律 401，本服务对应返回 502");
    }

    let mut auth = None;
    if mode != "public" {
        log_line(
            "WARN --mode tailnet：不装认证，任何能连上这个端口的人都能看全量内容。\
             只有确认端口只在内网/本机可达时才该这么跑。",
        );
    } else {
        match setup_auth(session_key_file.as_deref(), dsn_file.as_deref()) {
            Ok(a) => {
                log_line("INFO 认证已装配：mode=public");
                auth = Some(a);
            }
            Err(e) => {
                eprintln!("public 模式装配认证失败，拒绝启动：{e}");
                eprintln!(
                    "先跑 view_auth.py init-key / adduser 把密钥和账号建好，\
                     或者确认 --dsn-file / --session-key-file 指对了地方"
                );
                std::process::exit(2);
            }
        }
    }

    let rt = Runtime {
        upstream: upstream.trim_end_matches('/').to_string(),
        token,
        token_loaded,
        refresh_seconds,
        mode,
        auth,
        secure_cookie,
        domain_scope,
    };
    let gw = build_gateway(rt, trust_proxy);
    log_line(&format!(
        "INFO blackboard_view {VERSION} (Rust) listening on {host}:{port} upstream={upstream} token={} refresh={refresh_seconds}s",
        if token_loaded { "loaded" } else { "MISSING" },
    ));
    if let Err(e) = gw.serve(&host, port) {
        eprintln!("监听失败 {host}:{port}: {e}");
        std::process::exit(2);
    }
}

/// 按 --mode 装配认证：public 模式装不上时宁可不启动。
fn setup_auth(key_file: Option<&str>, dsn_file: Option<&str>) -> Result<Auth, String> {
    let key = va::load_session_key(key_file)?;
    // DSN 来源：--dsn-file 优先，读不到再退 BB_PG_DSN 环境变量（与 Python open_store 同向）。
    let dsn = dsn_file
        .map(read_dsn_file)
        .filter(|d| !d.is_empty())
        .or_else(|| std::env::var("BB_PG_DSN").ok())
        .unwrap_or_default();
    let store = bv::ViewStore::connect(&dsn).map_err(|e| e.0)?;
    Ok(Auth::new(Arc::new(store), key, va::SESSION_TTL_S))
}
