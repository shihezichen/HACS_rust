//! L1 通信层：极简 NATS 客户端（对应 Python `node/l1_transport/nats_client.py`）。
//!
//! 只实现 TEXT 协议里团队平面用得到的那部分：
//! `INFO / CONNECT / PING / PONG / PUB / SUB / UNSUB / MSG / -ERR`。
//!
//! 两个已经踩过的坑（必须保留）：
//! 1. `MSG <subject> <sid> [reply] <size>` —— 带 reply 时字段会错位，size 永远在**最后**
//!    一个字段，必须按字段个数（4 或 5）分别取，不能写死下标 3。
//! 2. `-ERR` 必须落到 `last_err` 并回调给使用者。服务端的权限违规（Permissions Violation）
//!    就是靠 -ERR 报的，静默吞掉会让「消息发不出去」变成查不出来的哑故障。
//!
//! 多地址故障转移：`connect()` 按给定顺序逐个尝试**完整握手**（TCP 连接 + INFO + CONNECT +
//! 等 PONG），任一成功即返回；全部失败则报错，错误信息里按顺序列出每个地址各自的失败原因。
//! 成功后 `host()` / `port()` 更新为**实际连上的那个地址**。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const CRLF: &[u8] = b"\r\n";
pub const DEFAULT_PORT: u16 = 9930;
pub const CLIENT_VERSION: &str = "nats_client-1.0";

/// 连接 / 握手 / 协议层面的错误。
#[derive(Debug, Clone)]
pub struct NatsError(pub String);

impl std::fmt::Display for NatsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for NatsError {}

/// 消息回调：`(payload, reply)`。
pub type MsgCallback = Arc<dyn Fn(&[u8], &str) + Send + Sync>;
/// 错误 / 断开回调。
pub type NoticeCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// 进程级自增序号：收件箱必须**进程内唯一**。
///
/// 踩过的坑：同进程**并发**用多个连接做 request 时，若收件箱只靠「毫秒 + 实例内自增」，
/// 多个连接实例的自增都从 1 开始且毫秒相同 → 收件箱字符串**撞车**，
/// 表现为「所有成员回复内容一模一样」的幽灵应答。
static INBOX_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_inbox() -> String {
    let seq = INBOX_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let pid = std::process::id();
    let tid = thread_id();
    let rand = pseudo_random_hex(12);
    format!("_INBOX.{pid}.{seq}.{tid}.{rand}")
}

/// 不引 rand 依赖：用地址熵 + 纳秒时钟拼一个够用的短随机串（只要求唯一，不要求密码学强度）。
fn pseudo_random_hex(n: usize) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut hasher);
    (&INBOX_SEQ as *const _ as usize).hash(&mut hasher);
    let h = hasher.finish();
    let s = format!("{h:016x}");
    s.chars().cycle().take(n).collect()
}

fn thread_id() -> u64 {
    // Rust 没有稳定的线程 id 取值 API；用地址里的 ASCII 哈希近似（同 Python `get_ident() % 100000`）。
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut hasher);
    hasher.finish() % 100_000
}

/// 连接配置。
#[derive(Debug, Clone)]
pub struct NatsConfig {
    /// 故障转移顺序（host, port）。
    pub hosts: Vec<(String, u16)>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub name: String,
    pub timeout: Duration,
}

impl NatsConfig {
    /// 从逗号分隔的地址串构造（每项可为 `host` 或 `host:port`）。
    pub fn from_hosts_str(spec: &str, default_port: u16) -> Result<Self, NatsError> {
        Ok(Self {
            hosts: parse_addrs(spec, default_port)?,
            user: None,
            password: None,
            name: "nats-client".to_string(),
            timeout: Duration::from_secs_f64(8.0),
        })
    }

    pub fn with_credentials(mut self, user: Option<String>, password: Option<String>) -> Self {
        self.user = user;
        self.password = password;
        self
    }

    pub fn with_name(mut self, name: &str) -> Self {
        self.name = name.to_string();
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// 把地址串解析成 `[(host, port), ...]` 有序候选列表（即故障转移尝试顺序）。
pub fn parse_addrs(spec: &str, default_port: u16) -> Result<Vec<(String, u16)>, NatsError> {
    let mut out = Vec::new();
    for item in spec.split(',') {
        let s = item.trim().strip_prefix("nats://").unwrap_or(item.trim());
        if s.is_empty() {
            continue;
        }
        if let Some((h, p)) = s.rsplit_once(':') {
            if let Ok(port) = p.parse::<u16>() {
                let h = h.trim();
                if !h.is_empty() {
                    out.push((h.to_string(), port));
                    continue;
                }
            }
        }
        out.push((s.to_string(), default_port));
    }
    if out.is_empty() {
        return Err(NatsError("host/hosts 解析结果为空".to_string()));
    }
    Ok(out)
}

struct Inner {
    /// 写侧 socket（读侧由 reader 线程独享自己的 clone）。
    writer: Mutex<Option<TcpStream>>,
    pongs: AtomicU64,
    last_err: Mutex<Option<String>>,
    server_info: Mutex<serde_json::Value>,
    subs: Mutex<HashMap<String, Vec<MsgCallback>>>,
    sub_subjects: Mutex<HashMap<String, String>>,
    send_lock: Mutex<()>,
    running: AtomicBool,
    connected: AtomicBool,
    on_error: Mutex<Option<NoticeCallback>>,
    on_disconnect: Mutex<Option<NoticeCallback>>,
}

impl Inner {
    fn send(&self, data: &[u8]) -> Result<(), NatsError> {
        let _guard = self.send_lock.lock().unwrap();
        let mut writer = self.writer.lock().unwrap();
        let sock = writer
            .as_mut()
            .ok_or_else(|| NatsError("连接未建立".to_string()))?;
        sock.send_all(data)
            .map_err(|e| NatsError(format!("发送失败: {e}")))
    }
}

/// 极简 NATS 客户端。一个连接 = 一个后台 reader 线程。
pub struct NatsClient {
    addrs: Vec<(String, u16)>,
    host: Mutex<String>,
    port: Mutex<u16>,
    cfg: NatsConfig,
    inner: Arc<Inner>,
    reader: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl NatsClient {
    pub fn new(cfg: NatsConfig) -> Self {
        let addrs = cfg.hosts.clone();
        let (h0, p0) = addrs[0].clone();
        Self {
            addrs,
            host: Mutex::new(h0),
            port: Mutex::new(p0),
            cfg,
            inner: Arc::new(Inner {
                writer: Mutex::new(None),
                pongs: AtomicU64::new(0),
                last_err: Mutex::new(None),
                server_info: Mutex::new(serde_json::json!({})),
                subs: Mutex::new(HashMap::new()),
                sub_subjects: Mutex::new(HashMap::new()),
                send_lock: Mutex::new(()),
                running: AtomicBool::new(false),
                connected: AtomicBool::new(false),
                on_error: Mutex::new(None),
                on_disconnect: Mutex::new(None),
            }),
            reader: Mutex::new(None),
        }
    }

    pub fn set_on_error(&self, cb: NoticeCallback) {
        *self.inner.on_error.lock().unwrap() = Some(cb);
    }

    pub fn set_on_disconnect(&self, cb: NoticeCallback) {
        *self.inner.on_disconnect.lock().unwrap() = Some(cb);
    }

    /// 实际连上的地址。
    pub fn host(&self) -> String {
        self.host.lock().unwrap().clone()
    }
    pub fn port(&self) -> u16 {
        *self.port.lock().unwrap()
    }
    pub fn addrs(&self) -> Vec<(String, u16)> {
        self.addrs.clone()
    }

    pub fn is_connected(&self) -> bool {
        self.inner.connected.load(Ordering::SeqCst)
            && self.inner.running.load(Ordering::SeqCst)
            && self.inner.writer.lock().unwrap().is_some()
    }

    pub fn last_err(&self) -> Option<String> {
        self.inner.last_err.lock().unwrap().clone()
    }

    pub fn server_info(&self) -> serde_json::Value {
        self.inner.server_info.lock().unwrap().clone()
    }

    /// 建连 + CONNECT 鉴权握手（多地址故障转移）。
    pub fn connect(&self) -> Result<(), NatsError> {
        let mut errors: Vec<String> = Vec::new();
        for (h, p) in self.addrs.clone() {
            match self.connect_one(&h, p) {
                Ok(()) => {
                    *self.host.lock().unwrap() = h;
                    *self.port.lock().unwrap() = p;
                    self.resend_subs();
                    return Ok(());
                }
                Err(exc) => {
                    errors.push(format!("{h}:{p} —— {exc}"));
                    self.close();
                }
            }
        }
        Err(NatsError(format!(
            "连不上任何 NATS 地址（已尝试 {} 个）：{}",
            self.addrs.len(),
            errors.join("；")
        )))
    }

    /// 对单个地址做一次完整握手：TCP 连接 + 读 INFO + 发 CONNECT + 等 PONG。
    fn connect_one(&self, host: &str, port: u16) -> Result<(), NatsError> {
        self.close();
        *self.inner.last_err.lock().unwrap() = None;
        self.inner.pongs.store(0, Ordering::SeqCst);

        let timeout = self.cfg.timeout;
        let sock = connect_timeout(host, port, timeout)
            .map_err(|e| NatsError(format!("连不上 {host}:{port} —— {e}")))?;
        sock.set_read_timeout(Some(timeout)).ok();
        sock.set_write_timeout(Some(timeout)).ok();

        let mut read_sock = sock
            .try_clone()
            .map_err(|e| NatsError(format!("clone socket: {e}")))?;
        *self.inner.writer.lock().unwrap() = Some(sock);
        self.inner.running.store(true, Ordering::SeqCst);
        self.inner.connected.store(true, Ordering::SeqCst);

        // 读 INFO（reader 线程还没起，用局部缓冲；剩下的一并交给 reader）
        let mut buf: Vec<u8> = Vec::new();
        let info = read_line(&mut read_sock, &mut buf)
            .map_err(|e| NatsError(format!("读 INFO 失败: {e}")))?;
        if !info.starts_with(b"INFO ") {
            let head: String = String::from_utf8_lossy(&info[..info.len().min(80)]).to_string();
            return Err(NatsError(format!("服务端未先发 INFO: {head:?}")));
        }
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&info[5..]) {
            *self.inner.server_info.lock().unwrap() = v;
        }

        let mut connect = serde_json::json!({
            "verbose": false,
            "pedantic": false,
            "name": self.cfg.name,
            "lang": "rs",
            "version": CLIENT_VERSION,
            "protocol": 1
        });
        if let Some(user) = &self.cfg.user {
            connect["user"] = serde_json::Value::String(user.clone());
            connect["pass"] =
                serde_json::Value::String(self.cfg.password.clone().unwrap_or_default());
        }
        let mut msg = Vec::new();
        msg.extend_from_slice(b"CONNECT ");
        msg.extend_from_slice(connect.to_string().as_bytes());
        msg.extend_from_slice(CRLF);
        msg.extend_from_slice(b"PING");
        msg.extend_from_slice(CRLF);
        self.inner.send(&msg)?;

        // 握手完成后转为阻塞读
        read_sock.set_read_timeout(None).ok();

        let inner = self.inner.clone();
        let handle = std::thread::Builder::new()
            .name("nats-reader".to_string())
            .spawn(move || reader_loop(read_sock, buf, inner))
            .map_err(|e| NatsError(format!("起 reader 线程失败: {e}")))?;
        *self.reader.lock().unwrap() = Some(handle);

        if !self.wait_pong(timeout.max(Duration::from_secs(3))) {
            let err = self
                .last_err()
                .unwrap_or_else(|| "无 -ERR，可能是网络不通".to_string());
            return Err(NatsError(format!(
                "CONNECT 后未收到 PONG（鉴权失败或网络不通）：{err}"
            )));
        }
        Ok(())
    }

    fn wait_pong(&self, seconds: Duration) -> bool {
        let deadline = Instant::now() + seconds;
        while Instant::now() < deadline {
            if self.inner.pongs.load(Ordering::SeqCst) > 0 {
                return true;
            }
            if !self.inner.connected.load(Ordering::SeqCst) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.inner.pongs.load(Ordering::SeqCst) > 0
    }

    pub fn subscribe<F>(&self, subject: &str, cb: F, sid: &str) -> Result<(), NatsError>
    where
        F: Fn(&[u8], &str) + Send + Sync + 'static,
    {
        {
            let mut subs = self.inner.subs.lock().unwrap();
            subs.entry(sid.to_string()).or_default().push(Arc::new(cb));
            self.inner
                .sub_subjects
                .lock()
                .unwrap()
                .insert(sid.to_string(), subject.to_string());
        }
        let mut msg = Vec::new();
        msg.extend_from_slice(b"SUB ");
        msg.extend_from_slice(subject.as_bytes());
        msg.push(b' ');
        msg.extend_from_slice(sid.as_bytes());
        msg.extend_from_slice(CRLF);
        self.inner.send(&msg)
    }

    pub fn unsubscribe(&self, sid: &str) -> Result<(), NatsError> {
        {
            self.inner.subs.lock().unwrap().remove(sid);
            self.inner.sub_subjects.lock().unwrap().remove(sid);
        }
        let mut msg = Vec::new();
        msg.extend_from_slice(b"UNSUB ");
        msg.extend_from_slice(sid.as_bytes());
        msg.extend_from_slice(CRLF);
        let _ = self.inner.send(&msg);
        Ok(())
    }

    /// 重连后把历史订阅重新发一遍。
    fn resend_subs(&self) {
        let items: Vec<(String, String)> = self
            .inner
            .sub_subjects
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (sid, subject) in items {
            if sid.starts_with("_INBOX.") {
                continue; // 一次性收件箱不必重建
            }
            let mut msg = Vec::new();
            msg.extend_from_slice(b"SUB ");
            msg.extend_from_slice(subject.as_bytes());
            msg.push(b' ');
            msg.extend_from_slice(sid.as_bytes());
            msg.extend_from_slice(CRLF);
            if self.inner.send(&msg).is_err() {
                return;
            }
        }
    }

    pub fn publish(&self, subject: &str, payload: &[u8], reply: &str) -> Result<(), NatsError> {
        let mut msg = Vec::new();
        msg.extend_from_slice(b"PUB ");
        msg.extend_from_slice(subject.as_bytes());
        msg.push(b' ');
        if !reply.is_empty() {
            msg.extend_from_slice(reply.as_bytes());
            msg.push(b' ');
        }
        msg.extend_from_slice(payload.len().to_string().as_bytes());
        msg.extend_from_slice(CRLF);
        msg.extend_from_slice(payload);
        msg.extend_from_slice(CRLF);
        self.inner.send(&msg)
    }

    /// request/reply：临时收件箱等一条回复；超时返回 `None`。
    pub fn request(&self, subject: &str, payload: &[u8], timeout: Duration) -> Option<Vec<u8>> {
        let inbox = next_inbox();
        let boxed: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
        let slot = boxed.clone();
        let _ = self.subscribe(
            &inbox,
            move |p: &[u8], _r: &str| {
                let mut b = slot.lock().unwrap();
                if b.is_none() {
                    *b = Some(p.to_vec());
                }
            },
            &inbox,
        );
        let _ = self.publish(subject, payload, &inbox);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline && boxed.lock().unwrap().is_none() {
            if !self.inner.connected.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.unsubscribe(&inbox);
        let out = boxed.lock().unwrap().clone();
        out
    }

    /// 发 PING 等 PONG。返回 `false` 说明链路已经不通了（探活用）。
    pub fn flush(&self, timeout: Duration) -> bool {
        if !self.is_connected() {
            return false;
        }
        let base = self.inner.pongs.load(Ordering::SeqCst);
        if let Err(e) = self.inner.send(b"PING\r\n") {
            *self.inner.last_err.lock().unwrap() = Some(format!("flush 发送失败: {e}"));
            self.inner.connected.store(false, Ordering::SeqCst);
            return false;
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.inner.pongs.load(Ordering::SeqCst) > base {
                return true;
            }
            if !self.inner.connected.load(Ordering::SeqCst) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    pub fn close(&self) {
        self.inner.running.store(false, Ordering::SeqCst);
        self.inner.connected.store(false, Ordering::SeqCst);
        let mut writer = self.inner.writer.lock().unwrap();
        if let Some(sock) = writer.take() {
            let _ = sock.shutdown(std::net::Shutdown::Both);
            drop(sock);
        }
    }
}

impl Drop for NatsClient {
    fn drop(&mut self) {
        self.close();
    }
}

fn reader_loop(mut sock: TcpStream, mut buf: Vec<u8>, inner: Arc<Inner>) {
    let mut reason = "正常关闭".to_string();
    while inner.running.load(Ordering::SeqCst) {
        let line = match read_line(&mut sock, &mut buf) {
            Ok(l) => l,
            Err(e) => {
                reason = format!("{e}");
                break;
            }
        };
        if line.starts_with(b"MSG ") {
            handle_msg(&mut sock, &mut buf, &inner, &line);
        } else if line.starts_with(b"PING") {
            let _ = inner.send(b"PONG\r\n");
        } else if line.starts_with(b"PONG") {
            inner.pongs.fetch_add(1, Ordering::SeqCst);
        } else if line.starts_with(b"-ERR") {
            handle_err(&inner, &line);
        } else if line.starts_with(b"INFO ") {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&line[5..]) {
                *inner.server_info.lock().unwrap() = v;
            }
        }
        // +OK 及其他行忽略
    }
    inner.connected.store(false, Ordering::SeqCst);
    if let Some(cb) = inner.on_disconnect.lock().unwrap().clone() {
        cb(&reason);
    }
}

/// `MSG <subject> <sid> [reply] <size>` —— size 永远在最后，按字段个数取。
fn handle_msg(sock: &mut TcpStream, buf: &mut Vec<u8>, inner: &Arc<Inner>, line: &[u8]) {
    let parts: Vec<&[u8]> = line
        .split(|b| *b == b' ')
        .filter(|s| !s.is_empty())
        .collect();
    if parts.len() < 4 {
        return;
    }
    let sid = String::from_utf8_lossy(parts[2]).to_string();
    let (reply, size_str) = if parts.len() >= 5 {
        (
            String::from_utf8_lossy(parts[3]).to_string(),
            String::from_utf8_lossy(parts[4]).to_string(),
        )
    } else {
        (String::new(), String::from_utf8_lossy(parts[3]).to_string())
    };
    let Ok(size) = size_str.parse::<usize>() else {
        return;
    };
    let payload = match read_n(sock, buf, size + 2) {
        Ok(v) => v[..size].to_vec(),
        Err(_) => return,
    };
    let callbacks = inner
        .subs
        .lock()
        .unwrap()
        .get(&sid)
        .cloned()
        .unwrap_or_default();
    for cb in callbacks {
        cb(&payload, &reply);
    }
}

fn handle_err(inner: &Arc<Inner>, line: &[u8]) {
    let text = String::from_utf8_lossy(line).to_string();
    *inner.last_err.lock().unwrap() = Some(text.clone());
    if let Some(cb) = inner.on_error.lock().unwrap().clone() {
        cb(&text);
    }
}

/// 从缓冲 + socket 读一行（不含 CRLF）。
fn read_line(sock: &mut TcpStream, buf: &mut Vec<u8>) -> std::io::Result<Vec<u8>> {
    loop {
        if let Some(pos) = find_crlf(buf) {
            let line = buf[..pos].to_vec();
            buf.drain(..pos + 2);
            return Ok(line);
        }
        let mut chunk = [0u8; 65536];
        let n = sock.read(&mut chunk)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "连接被关闭",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// 从缓冲 + socket 读恰好 n 字节。
fn read_n(sock: &mut TcpStream, buf: &mut Vec<u8>, n: usize) -> std::io::Result<Vec<u8>> {
    while buf.len() < n {
        let mut chunk = [0u8; 65536];
        let got = sock.read(&mut chunk)?;
        if got == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "连接被关闭",
            ));
        }
        buf.extend_from_slice(&chunk[..got]);
    }
    let out = buf[..n].to_vec();
    buf.drain(..n);
    Ok(out)
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

/// 带超时的 TCP 连接（不引 tokio）。
fn connect_timeout(host: &str, port: u16, timeout: Duration) -> std::io::Result<TcpStream> {
    use std::net::ToSocketAddrs;
    let addrs = (host, port).to_socket_addrs()?;
    let mut last_err = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => return Ok(s),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err
        .unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "无可用地址")))
}

/// 小工具：`TcpStream::sendall` 的对应物。
trait SendAll {
    fn send_all(&mut self, data: &[u8]) -> std::io::Result<()>;
}
impl SendAll for TcpStream {
    fn send_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.write_all(data)
    }
}

/// 读口令/令牌文件（去掉首尾空白与 BOM）。找不到给人话报错。
pub fn read_secret_file(path: &str) -> Result<String, NatsError> {
    let full = expand_user(path);
    if !std::path::Path::new(&full).is_file() {
        return Err(NatsError(format!("文件不存在: {full}")));
    }
    let raw = std::fs::read_to_string(&full).map_err(|e| NatsError(format!("读文件失败: {e}")))?;
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    Ok(raw.trim().to_string())
}

/// `~` 展开（对应 Python `os.path.expanduser`）。
fn expand_user(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        let home = crate::node_paths::home();
        if !home.is_empty() {
            return format!("{}{}{}", home, std::path::MAIN_SEPARATOR, rest);
        }
    }
    path.to_string()
}
