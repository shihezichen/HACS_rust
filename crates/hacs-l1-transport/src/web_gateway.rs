//! L1 通信层：HTTP 入口件（**传输不含语义**）。对应 Python `node/l1_transport/web_gateway.py`。
//!
//! 为什么要有它：分层前 7~8 个件各自造 HTTP handler，把「解析请求、鉴权、JSON 编解码、
//! 错误信封、路径归一化」各写了一遍。本模块把这些**与业务无关**的传输职责收成一处，
//! 各域件只登记「路径 → 处理函数」。
//!
//! 设计约束：
//! 1. **传输不含语义**：本模块不出现群 / 会话 / 待办 / 会议等业务词。
//! 2. **零行为改变**：默认回复的 `Content-Type` / `Content-Length` / 状态码 / 路径归一化
//!    与既有各件逐字一致。**未注册的 HTTP 方法保持 501 行为**（不自行发明 405 信封）。
//! 3. **鉴权可插拔**：只提供「路由前调一次钩子」的位置与「短路回复」的契约。
//! 4. **不持业务状态**：本模块不读配置、不连库、不碰文件。
//!
//! 手写 socket 而非引入 HTTP 框架，是刻意的：既有各件的响应形态是
//! `http.server` 的形状（HTTP/1.0 状态行 / `Server:`+`Date:` 头序 / `Content-Length` 显式），
//! 框架会把这些细节改掉；且零框架依赖直接服务于「减少内存占用」这个目标。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

/// 短路回复：直接返回一个响应。
#[derive(Debug, Clone)]
pub struct HttpError {
    pub status: u16,
    pub body: Body,
    pub headers: Vec<(String, String)>,
}

impl HttpError {
    pub fn new(status: u16, body: Body) -> Self {
        Self {
            status,
            body,
            headers: Vec::new(),
        }
    }
    pub fn json(status: u16, value: serde_json::Value) -> Self {
        Self::new(status, Body::Json(value))
    }
    /// `{"ok": false, "error": <code>}` —— 与既有各件口径一致的错误信封。
    pub fn error(status: u16, code: &str) -> Self {
        Self::json(status, serde_json::json!({"ok": false, "error": code}))
    }
}

/// 响应体：`Bytes` 按原样发出（调用方自带 `Content-Type`）；`Json` 按 Python 默认分隔符编码；
/// `Empty` 是空体。
#[derive(Debug, Clone)]
pub enum Body {
    Bytes(Vec<u8>),
    Json(serde_json::Value),
    Empty,
}

/// 处理函数的返回值。
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Body,
    pub headers: Vec<(String, String)>,
}

impl Reply {
    pub fn json(status: u16, value: serde_json::Value) -> Self {
        Self {
            status,
            body: Body::Json(value),
            headers: Vec::new(),
        }
    }
    pub fn json_with(
        status: u16,
        value: serde_json::Value,
        headers: Vec<(String, String)>,
    ) -> Self {
        Self {
            status,
            body: Body::Json(value),
            headers,
        }
    }
    pub fn bytes(status: u16, content_type: &str, data: Vec<u8>) -> Self {
        Self {
            status,
            body: Body::Bytes(data),
            headers: vec![("Content-Type".to_string(), content_type.to_string())],
        }
    }
    pub fn empty(status: u16) -> Self {
        Self {
            status,
            body: Body::Empty,
            headers: Vec::new(),
        }
    }
}

/// Python `json.dumps(obj, ensure_ascii=False)` 的**默认分隔符**（`, ` 与 `: `）。
///
/// serde_json 默认是紧凑分隔符（`,` `:`），逐字节对照会不一致 —— 这里显式对齐。
struct PyFormatter;

impl serde_json::ser::Formatter for PyFormatter {
    fn begin_array_value<W>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()>
    where
        W: ?Sized + Write,
    {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }
    fn begin_object_key<W>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()>
    where
        W: ?Sized + Write,
    {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }
    fn begin_object_value<W>(&mut self, writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + Write,
    {
        writer.write_all(b": ")
    }
}

/// 按 Python 口径序列化：UTF-8 原样输出（`ensure_ascii=False`）+ `, ` / `: ` 分隔符。
pub fn dumps_python(value: &serde_json::Value) -> Vec<u8> {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, PyFormatter);
    serde::Serialize::serialize(value, &mut ser).expect("serialize to Vec never fails");
    out
}

/// 把处理函数的返回值化成 `(status, body_bytes, headers)`。本模块为 L1 契约面。
pub fn envelope(reply: Reply) -> (u16, Vec<u8>, Vec<(String, String)>) {
    let mut headers = reply.headers;
    let body = match reply.body {
        Body::Bytes(b) => b,
        Body::Empty => Vec::new(),
        Body::Json(v) => {
            if !headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("Content-Type"))
            {
                headers.push((
                    "Content-Type".to_string(),
                    "application/json; charset=utf-8".to_string(),
                ));
            }
            dumps_python(&v)
        }
    };
    (reply.status, body, headers)
}

/// 一次请求的只读视图。
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub raw_path: String,
    /// 路径归一化：`rstrip("/")`，空则 `/`（登记与分发用**同一套归一化**）。
    pub path: String,
    pub query: String,
    headers: Vec<(String, String)>,
    pub client: String,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 读 JSON 请求体；超限或非法一律 400/413（与既有各件口径一致）。
    pub fn json_body(&self, max_bytes: usize) -> Result<serde_json::Value, HttpError> {
        if self.body.len() > max_bytes {
            return Err(HttpError::error(413, "payload_too_large"));
        }
        if self.body.is_empty() {
            return Ok(serde_json::json!({}));
        }
        match std::str::from_utf8(&self.body) {
            Ok(s) => serde_json::from_str(s).map_err(|_| HttpError::error(400, "bad_json")),
            Err(_) => Err(HttpError::error(400, "bad_json")),
        }
    }
}

type Handler = Arc<dyn Fn(&Request) -> Result<Reply, HttpError> + Send + Sync>;
type Hook = Arc<dyn Fn(&Request) -> Result<Option<Reply>, HttpError> + Send + Sync>;

/// 一个端口的 HTTP 入口：路由表 + 前置钩子 + 回复信封。
#[derive(Clone)]
pub struct Gateway {
    pub name: String,
    pub version: Option<String>,
    pub quiet: bool,
    /// `sys_version` 拼在 `Server:` 头里（对应 Python `BaseHTTPRequestHandler.sys_version`）。
    pub sys_version: String,
    /// `HTTP/1.0`（默认，同基类）或 `HTTP/1.1`（同既有一些子类）。
    pub protocol_version: String,
    exact: HashMap<(String, String), Handler>,
    prefix: Vec<(String, String, Handler)>,
    pre: Option<Hook>,
    auth: Option<Hook>,
    not_found: Option<Handler>,
}

impl Gateway {
    pub fn new(name: &str, version: Option<&str>, quiet: bool) -> Self {
        Self {
            name: name.to_string(),
            version: version.map(|v| v.to_string()),
            quiet,
            sys_version: String::new(),
            protocol_version: "HTTP/1.0".to_string(),
            exact: HashMap::new(),
            prefix: Vec::new(),
            pre: None,
            auth: None,
            not_found: None,
        }
    }

    fn server_version(&self) -> String {
        match &self.version {
            Some(v) => format!("{}/{}", self.name, v),
            None => self.name.clone(),
        }
    }

    fn version_string(&self) -> String {
        format!("{} {}", self.server_version(), self.sys_version)
    }

    /// 登记一条路由。`prefix=true` 时按前缀匹配（更长者优先）。
    pub fn route<F>(&mut self, path: &str, methods: &[&str], prefix: bool, handler: F)
    where
        F: Fn(&Request) -> Result<Reply, HttpError> + Send + Sync + 'static,
    {
        // 登记路径与分发路径用**同一套归一化**：否则 "/api/" 这种带尾斜杠的登记永远
        // 匹配不上（分发侧已 rstrip），会变成**静默死路由**。
        let key_path = normalize_path(path);
        let handler: Handler = Arc::new(handler);
        for m in methods {
            let key = m.to_uppercase();
            if prefix {
                self.prefix.push((key, key_path.clone(), handler.clone()));
            } else {
                self.exact.insert((key, key_path.clone()), handler.clone());
            }
        }
        self.prefix
            .sort_by_key(|entry| std::cmp::Reverse(entry.1.len()));
    }

    /// 路由前钩子（如「未就绪统一 503」）。返回回复即短路。
    pub fn pre<F>(&mut self, hook: F)
    where
        F: Fn(&Request) -> Result<Option<Reply>, HttpError> + Send + Sync + 'static,
    {
        self.pre = Some(Arc::new(hook));
    }

    /// 鉴权钩子：放行返回 `Ok(None)`。
    pub fn auth<F>(&mut self, hook: F)
    where
        F: Fn(&Request) -> Result<Option<Reply>, HttpError> + Send + Sync + 'static,
    {
        self.auth = Some(Arc::new(hook));
    }

    /// 自定义 404。不给时返回统一的 not_found 信封。
    pub fn not_found<F>(&mut self, handler: F)
    where
        F: Fn(&Request) -> Result<Reply, HttpError> + Send + Sync + 'static,
    {
        self.not_found = Some(Arc::new(handler));
    }

    fn default_not_found(req: &Request) -> Reply {
        Reply::json(
            404,
            serde_json::json!({"ok": false, "error": "not_found", "path": req.path}),
        )
    }

    /// 该 HTTP 方法是否有任何登记（决定 501 还是继续分派）。
    fn method_registered(&self, method: &str) -> bool {
        self.exact.keys().any(|(m, _)| m == method)
            || self.prefix.iter().any(|(m, _, _)| m == method)
    }

    /// 分发一次请求，返回 `(status, body_bytes, headers)`。
    pub fn dispatch(&self, req: &Request) -> (u16, Vec<u8>, Vec<(String, String)>) {
        for hook in [&self.pre, &self.auth] {
            let Some(hook) = hook else { continue };
            match hook(req) {
                Err(exc) => {
                    return envelope(Reply {
                        status: exc.status,
                        body: exc.body,
                        headers: exc.headers,
                    })
                }
                Ok(Some(reply)) => return envelope(reply),
                Ok(None) => {}
            }
        }
        let mut found = self
            .exact
            .get(&(req.method.clone(), req.path.clone()))
            .cloned();
        if found.is_none() {
            for (method, prefix, handler) in &self.prefix {
                if *method == req.method
                    && (req.path == *prefix || req.path.starts_with(&format!("{prefix}/")))
                {
                    found = Some(handler.clone());
                    break;
                }
            }
        }
        let handler = found;
        let result = match handler {
            Some(h) => h(req),
            None => match &self.not_found {
                Some(nf) => nf(req),
                None => Ok(Self::default_not_found(req)),
            },
        };
        match result {
            Ok(reply) => envelope(reply),
            Err(exc) => envelope(Reply {
                status: exc.status,
                body: exc.body,
                headers: exc.headers,
            }),
        }
    }

    /// 起服务：阻塞 accept 循环，每连接一线程（对应 Python `ThreadingHTTPServer`）。
    pub fn serve(self, host: &str, port: u16) -> std::io::Result<()> {
        let listener = TcpListener::bind((host, port))?;
        let gw = Arc::new(self);
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let gw = gw.clone();
            std::thread::spawn(move || {
                let _ = gw.handle_conn(stream);
            });
        }
        Ok(())
    }

    /// 起在后台线程里，返回**实际绑定的端口**（便于测试与同机双实例对照）。
    pub fn serve_in_background(self, host: &str, port: u16) -> std::io::Result<u16> {
        let listener = TcpListener::bind((host, port))?;
        let bound = listener.local_addr()?.port();
        let gw = Arc::new(self);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let gw = gw.clone();
                std::thread::spawn(move || {
                    let _ = gw.handle_conn(stream);
                });
            }
        });
        Ok(bound)
    }

    fn handle_conn(&self, stream: TcpStream) -> std::io::Result<()> {
        let peer = stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_default();
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut writer = stream;
        loop {
            let Some(head) = read_request_head(&mut reader)? else {
                return Ok(());
            };
            let (method, target, headers) = head;
            let body = read_body(&mut reader, &headers)?;
            let (raw_path, path, query) = split_target(&target);
            let req = Request {
                method: method.clone(),
                raw_path,
                path,
                query,
                headers: headers.clone(),
                client: peer.clone(),
                body,
            };

            // 未登记的方法：保持基类行为（501），不自行发明 405 信封。
            if !self.method_registered(&method) {
                let msg = format!("Unsupported method ('{method}')");
                let (status, body_bytes, hs) = self.render_501(&msg);
                write_response(
                    &mut writer,
                    &self.protocol_version,
                    &self.version_string(),
                    status,
                    &msg,
                    &body_bytes,
                    &hs,
                )?;
                return Ok(());
            }

            let (status, body_bytes, hs) = self.dispatch(&req);
            let reason = reason_phrase(status);
            write_response(
                &mut writer,
                &self.protocol_version,
                &self.version_string(),
                status,
                reason,
                &body_bytes,
                &hs,
            )?;

            // 连接复用：仅 HTTP/1.1 且双方都未要求关闭时继续。
            if self.protocol_version != "HTTP/1.1" {
                return Ok(());
            }
            let client_close = headers.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close")
            });
            let server_close = hs.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close")
            });
            if client_close || server_close {
                return Ok(());
            }
        }
    }

    /// 501 的响应形态（对应 Python `BaseHTTPRequestHandler.send_error`）。
    fn render_501(&self, message: &str) -> (u16, Vec<u8>, Vec<(String, String)>) {
        let escaped = html_escape(message);
        let body = format!(
            "<!DOCTYPE HTML>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>Error response</title>\n</head>\n<body>\n<h1>Error response</h1>\n<p>Error code: 501</p>\n<p>Message: {escaped}.</p>\n<p>Error code explanation: 501 - Server does not support this operation.</p>\n</body>\n</html>\n"
        )
        .into_bytes();
        let headers = vec![
            ("Connection".to_string(), "close".to_string()),
            (
                "Content-Type".to_string(),
                "text/html;charset=utf-8".to_string(),
            ),
            ("Content-Length".to_string(), body.len().to_string()),
        ];
        (501, body, headers)
    }
}

fn normalize_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn split_target(target: &str) -> (String, String, String) {
    let raw = if target.is_empty() { "/" } else { target };
    let (path_part, query) = match raw.split_once('?') {
        Some((p, q)) => (p, q.to_string()),
        None => (raw, String::new()),
    };
    (raw.to_string(), normalize_path(path_part), query)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// RFC 1123 时间戳（`Sun, 06 Nov 1994 08:49:37 GMT`）—— 对应 `http.server` 的 `Date:` 头。
///
/// 不引 chrono/时间库：只用 `SystemTime` + 民用历反算（Howard Hinnant 算法）。
pub fn rfc1123_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    let weekday =
        ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][(days + 4).rem_euclid(7) as usize];
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(mo - 1) as usize];
    format!("{weekday}, {d:02} {month} {y} {h:02}:{mi:02}:{s:02} GMT")
}

/// 天数（自 1970-01-01）→ `(年, 月, 日)`。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 一次请求的头：`(method, target, headers)`。
type RequestHead = (String, String, Vec<(String, String)>);

/// 读请求头：请求行 + 头，直到空行。连接关闭返回 `Ok(None)`。
fn read_request_head(reader: &mut BufReader<TcpStream>) -> std::io::Result<Option<RequestHead>> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        if !line.trim().is_empty() {
            break;
        }
    }
    let mut parts = line.trim_end().split(' ');
    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut hline = String::new();
        let n = reader.read_line(&mut hline)?;
        if n == 0 {
            break;
        }
        let trimmed = hline.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(Some((method, target, headers)))
}

fn read_body(
    reader: &mut BufReader<TcpStream>,
    headers: &[(String, String)],
) -> std::io::Result<Vec<u8>> {
    let len = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    if len == 0 {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

fn write_response(
    writer: &mut TcpStream,
    protocol: &str,
    server_version: &str,
    status: u16,
    reason: &str,
    body: &[u8],
    headers: &[(String, String)],
) -> std::io::Result<()> {
    let mut out: Vec<u8> = Vec::with_capacity(body.len() + 256);
    if reason.is_empty() {
        out.extend_from_slice(format!("{protocol} {status} \r\n").as_bytes());
    } else {
        out.extend_from_slice(format!("{protocol} {status} {reason}\r\n").as_bytes());
    }
    // 头序与 http.server 一致：Server / Date 先，随后是调用方给的头。
    out.extend_from_slice(format!("Server: {server_version}\r\n").as_bytes());
    out.extend_from_slice(format!("Date: {}\r\n", rfc1123_now()).as_bytes());
    let has_len = headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-length"));
    for (k, v) in headers {
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    if !has_len {
        out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    writer.write_all(&out)?;
    writer.flush()
}

/// 状态码 → 原因短语（`http.server` 的 `BaseHTTPRequestHandler.responses` 子集）。
pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Request Entity Too Large",
        414 => "Request-URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Requested Range Not Satisfiable",
        417 => "Expectation Failed",
        418 => "I'm a teapot",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}
