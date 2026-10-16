//! L2 适配层：**CLI 成员后端**（本机「只有命令行接口」的 Agent 统一垫片）。
//! 对应 Python `node/l2_adapter/cli_backend.py`（端口 9970，bin 见 `hacs-c`）。
//!
//! 背景：a2a_svc 的 `call_backend()` 契约固定（OpenAI 形状）：
//!     POST {backend}/v1/chat/completions   Authorization: Bearer ***
//!     {"model": "...", "messages": [...], "max_tokens": 2000}
//!     期望 {"choices":[{"message":{"content":"..."}}]}，取不到就 502。
//! 本模块把这套 HTTP 调用翻译成本机三个 CLI Agent：
//!   * piagent   —— `node cli.js --print -- <任务>`
//!   * claudecode —— 走已有封装 claude_exec.py（--file 传任务文本）
//!   * codex     —— `codex exec -- <任务>`
//!
//! 纪律（与 Python 版逐条对齐）：
//!   * 只绑 loopback；鉴权 fail closed；同成员同一时刻只跑 1 个任务（排队 → 503）；
//!   * 超时杀进程树 → 504；非零退出/空输出 → 502（detail 带 stderr 尾部，**绝不编造回复**）；
//!   * `max_tokens` 被忽略（CLI 没这个旋钮）。
//!
//! 纯逻辑函数（`clean_output` / `build_prompt` / `resolve_model` / `safe_session_id` /
//! `claude_session_uuid` / 文本清洗）集中在 [`text`] 与 [`model`] 两个子模块，
//! 供单元测试直接断言；进程装配（Runtime / HTTP 面 / CLI 面）在 `hacs-c` 的 `[[bin]]`。

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

/// 成员表与模型列表（顺序即对外顺序）。
pub const MEMBERS: &[&str] = &["piagent", "claudecode", "codex"];
pub const MODEL_IDS: &[&str] = &["pi/default", "claude/default", "codex/default"];

pub const VERSION: &str = "1.0";
pub const DEFAULT_TIMEOUT_S: f64 = 300.0;
pub const MAX_TIMEOUT_S: f64 = 1800.0;
pub const BUSY_WAIT_S: f64 = 2.0;
pub const DETAIL_MAX: usize = 300;
pub const MAX_BODY_BYTES: usize = 262144; // 256 KiB
pub const TASK_LOG_MAX: usize = 40;

// ---------------------------------------------------------------- 文本处理

/// 去 ANSI 转义、去 `\r`，收尾空白。中文原样保留（全程 UTF-8）。
///
/// ANSI 正则（CSI / OSC / 单字符 ESC 序列）与 Python `ANSI_RE` 同口径；
/// 用手写扫描器实现（性能与零依赖考虑），语义与 `ANSI_RE.sub("", text)` 等价。
pub fn clean_output(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let stripped = strip_ansi(text);
    stripped.replace("\r\n", "\n").replace('\r', "").trim().to_string()
}

/// `ANSI_RE` 的 Rust 版：逐字符扫描，识别 CSI / OSC / 单字符 ESC 三类序列。
fn strip_ansi(s: &str) -> String {
    let bytes: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != '\x1b' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // ESC 序列开始
        match bytes.get(i + 1) {
            Some('[') => {
                // CSI: \x1b [ 参数(0-9;?) 中间(@ 到 /) 终止(@ 到 ~)
                let mut j = i + 2;
                while j < bytes.len() && bytes[j].is_ascii_digit()
                    || j < bytes.len() && (bytes[j] == ';' || bytes[j] == '?')
                {
                    j += 1;
                }
                while j < bytes.len() && ('\u{20}'..='\u{2f}').contains(&bytes[j]) {
                    j += 1;
                }
                if j < bytes.len() && ('\u{40}'..='\u{7e}').contains(&bytes[j]) {
                    i = j + 1; // 整段吞掉
                } else {
                    out.push(bytes[i]); // 形态不完整：按字面量保留
                    i += 1;
                }
            }
            Some(']') => {
                // OSC: 直到 BEL(\x07) 或 ST(\x1b\\)
                let mut j = i + 2;
                let mut end = None;
                while j < bytes.len() {
                    if bytes[j] == '\x07' {
                        end = Some(j + 1);
                        break;
                    }
                    if bytes[j] == '\x1b' && bytes.get(j + 1) == Some(&'\\') {
                        end = Some(j + 2);
                        break;
                    }
                    j += 1;
                }
                match end {
                    Some(e) => i = e,
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            Some(c) if ('\u{40}'..='\u{5f}').contains(c) || *c == '\\' || *c == '_' => {
                i += 2; // 单字符 ESC 序列
            }
            _ => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    out
}

/// 最后一条 user 内容当任务，**它前面**的消息拼成上下文前缀，保证多轮不丢。
///
/// 形如：
/// ```text
/// [上下文]
/// 对方: ...
/// 你: ...
/// [/上下文]
///
/// <任务正文>
/// ```
pub fn build_prompt(messages: &Value) -> Option<String> {
    let items = messages.as_array()?;
    // 从后往前找最后一条「role=user 且 content 是非空白字符串」的消息
    let mut idx: Option<usize> = None;
    for (i, item) in items.iter().enumerate().rev() {
        let Some(obj) = item.as_object() else { continue };
        if obj.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let content = obj.get("content").and_then(Value::as_str);
        if let Some(c) = content {
            if !c.trim().is_empty() {
                idx = Some(i);
                break;
            }
        }
    }
    let idx = idx?;
    let task = items[idx]
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();

    let role_map = |role: &str| match role {
        "user" => "对方",
        "assistant" => "你",
        "system" => "系统",
        _ => "其它",
    };
    let mut lines: Vec<String> = Vec::new();
    for item in items.iter().take(idx) {
        let Some(obj) = item.as_object() else { continue };
        let Some(content) = obj.get("content").and_then(Value::as_str) else { continue };
        if content.trim().is_empty() {
            continue;
        }
        let who = role_map(obj.get("role").and_then(Value::as_str).unwrap_or(""));
        lines.push(format!("{who}: {}", content.trim()));
    }
    if lines.is_empty() {
        return Some(task);
    }
    Some(format!("[上下文]\n{}\n[/上下文]\n\n{}", lines.join("\n"), task))
}

// ---------------------------------------------------------------- 模型解析

/// `model` → `(member, cli_model)`。`cli_model` 为空串表示不给 CLI 传 `--model`。
pub fn resolve_model(model: &str) -> Option<(String, String)> {
    let name = model.trim();
    let low = name.to_lowercase();
    let member = if low.starts_with("pi") {
        "piagent"
    } else if low.starts_with("claude") {
        "claudecode"
    } else if low.starts_with("codex") {
        "codex"
    } else {
        return None;
    };
    let mut cli_model = String::new();
    if let Some((_, tail)) = name.split_once('/') {
        let tail = tail.trim();
        if !tail.is_empty() && !tail.eq_ignore_ascii_case("default") {
            cli_model = tail.to_string();
        }
    }
    Some((member.to_string(), cli_model))
}

/// 把任意会话键收敛成文件名安全的 id（pi 用）。
pub fn safe_session_id(raw: &str) -> String {
    let mut cleaned: String = raw
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect();
    cleaned.truncate(64);
    let cleaned = cleaned.trim_matches('-');
    let base = if cleaned.is_empty() { "anon" } else { cleaned };
    format!("a2a-{base}")
}

/// claude 的 `--session-id` 要求合法 UUID：用 uuid5(NAMESPACE_URL) 做稳定映射。
///
/// uuid5 = SHA-1(namespace_bytes || name) 的前 16 字节，按 RFC 4122 排版本/变体位。
/// 与 Python `uuid.uuid5(uuid.NAMESPACE_URL, name)` 逐字节一致（自实现，不引依赖）。
pub fn claude_session_uuid(raw: &str) -> String {
    // uuid.NAMESPACE_URL = 6ba7b811-9dad-11d1-80b4-00c04fd430c8（RFC 4122 附录 C 的固定值）
    let mut ns_url = [0u8; 16];
    for (i, b) in "6ba7b8119dad11d180b400c04fd430c8".as_bytes().chunks(2).enumerate() {
        ns_url[i] = u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap();
    }
    let digest = uuid5_bytes(&ns_url, format!("cli_backend/claude/{}", raw.trim()).as_bytes());
    format_uuid(digest)
}

/// 一次 SHA-1，取前 16 字节并打上 version=5 / variant=RFC4122 位。
fn uuid5_bytes(namespace: &[u8; 16], name: &[u8]) -> [u8; 16] {
    let mut data = Vec::with_capacity(16 + name.len());
    data.extend_from_slice(namespace);
    data.extend_from_slice(name);
    let sha1 = sha1(&data);
    let mut out = [0u8; 16];
    out.copy_from_slice(&sha1[..16]);
    out[6] = (out[6] & 0x0f) | 0x50; // version 5
    out[8] = (out[8] & 0x3f) | 0x80; // variant 10xx
    out
}

/// 标准格式化（8-4-4-4-12，小写）。
fn format_uuid(b: [u8; 16]) -> String {
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// SHA-1（FIPS 180-1，自实现避免引依赖；uuid5 是它在本仓的唯一用途）。
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];

    // 消息填充：补 0x80、补 0 直到 ≡56 (mod 64)，再补 8 字节大端位长
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.chunks(64) {
        let mut words = [0u32; 80];
        for i in 0..80 {
            words[i] = match i {
                0..=15 => {
                    let base = i * 4;
                    u32::from_be_bytes([
                        block[base],
                        block[base + 1],
                        block[base + 2],
                        block[base + 3],
                    ])
                }
                16..=79 => {
                    (words[i - 3] ^ words[i - 8] ^ words[i - 14] ^ words[i - 16]).rotate_left(1)
                }
                _ => unreachable!(),
            };
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, word) in words.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

// ---------------------------------------------------------------- 会话登记

/// 本进程已创建过的 claude 会话 uuid 集合（对应 `Runtime.claude_sessions`）。
#[derive(Default)]
pub struct SessionRegistry {
    claude: Mutex<std::collections::HashSet<String>>,
    /// 会话键 -> codex 自己生成的 session id。
    codex: Mutex<HashMap<String, String>>,
    /// codex 映射是否已从磁盘懒加载。
    codex_loaded: Mutex<bool>,
    /// codex 落盘路径覆盖（测试注入）；None = codex_workdir() 下的 `.codex_sessions.json`。
    pub codex_sessions_file: Mutex<Option<String>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// claude：uuid 是否已创建过。首次登记返回 `false`（→ 用 `--session-id` 创建）。
    pub fn claude_seen_or_add(&self, sid: &str) -> bool {
        let mut set = self.claude.lock().unwrap();
        if set.contains(sid) {
            true
        } else {
            set.insert(sid.to_string());
            false
        }
    }

    /// codex：取某会话键已有的 sid。
    pub fn codex_get(&self, key: &str) -> Option<String> {
        self.codex.lock().unwrap().get(key).cloned()
    }

    /// codex：登记从 banner 抓到的会话 id（语义同 `dict.setdefault`：只首次写入）。
    /// 返回该键生效的 sid。
    pub fn codex_setdefault(&self, key: &str, sid: &str) -> String {
        let mut map = self.codex.lock().unwrap();
        match map.get(key) {
            Some(existing) => existing.clone(),
            None => {
                map.insert(key.to_string(), sid.to_string());
                sid.to_string()
            }
        }
    }

    /// codex 磁盘映射是否已加载。
    pub fn codex_is_loaded(&self) -> bool {
        *self.codex_loaded.lock().unwrap()
    }

    pub fn codex_mark_loaded(&self) {
        *self.codex_loaded.lock().unwrap() = true;
    }

    /// 把整张磁盘映射并入内存（不覆盖已有的内存登记）。
    pub fn codex_merge(&self, incoming: HashMap<String, String>) {
        let mut map = self.codex.lock().unwrap();
        for (k, v) in incoming {
            map.entry(k).or_insert(v);
        }
    }

    /// 当前映射快照（落盘用）。
    pub fn codex_snapshot(&self) -> HashMap<String, String> {
        self.codex.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------- codex 会话映射落盘

/// 磁盘映射 → 内存映射。文件不存在/损坏一律当空映射（可用性优先于完整性）。
/// `{"key": {"sid": "..."}}` 与裸 `{"key": "sid"}` 两种形态都接受。
pub fn load_codex_sessions(path: &str) -> HashMap<String, String> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(Value::Object(data)) = serde_json::from_str::<Value>(&raw) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for (k, v) in data {
        if let Some(obj) = v.as_object() {
            if let Some(sid) = obj.get("sid").and_then(Value::as_str) {
                out.insert(k, sid.to_string());
            }
        } else if let Some(sid) = v.as_str() {
            out.insert(k, sid.to_string());
        }
    }
    out
}

/// 原子写映射（临时文件 + rename）。失败返回 false，不阻断请求。
/// `{"key": {"sid": "...", "updated_at": <unix秒>}}`，`ensure_ascii=False` + 2 空格缩进。
pub fn save_codex_sessions(mapping: &HashMap<String, String>, path: &str) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut keys: Vec<&String> = mapping.keys().collect();
    keys.sort();
    let mut out = String::from("{\n");
    for (i, k) in keys.iter().enumerate() {
        let v = &mapping[k.as_str()];
        // Python json.dumps(..., ensure_ascii=False) 的字符串转义口径：仅最小集
        out.push_str(&format!(
            "  \"{}\": {{\n    \"sid\": \"{}\",\n    \"updated_at\": {}\n  }}",
            json_escape(k),
            json_escape(v),
            now
        ));
        if i + 1 < keys.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("}\n");
    let dir = std::path::Path::new(path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let tmp = dir.join(format!(
        ".codex_sessions_{}.tmp",
        std::process::id()
    ));
    if std::fs::write(&tmp, out.as_bytes()).is_err() {
        return false;
    }
    // fsync 尽力而为；rename 是原子替换的实质
    if let Ok(f) = std::fs::File::open(&tmp) {
        let _ = f.sync_all();
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

/// Python `json.dumps` 对字符串的转义（`ensure_ascii=False`：非 ASCII 原样输出）。
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------- 杂项

/// 日志里只留任务前 40 字（单行），不记全文。
pub fn task_head(prompt: &str) -> String {
    prompt.replace('\n', " ").chars().take(TASK_LOG_MAX).collect()
}

/// 超时夹取：`max(1.0, min(timeout_s, MAX_TIMEOUT_S))`；解析失败回默认值。
pub fn clamp_timeout(raw: Option<&Value>) -> Option<f64> {
    let value = match raw {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_S,
        Some(v) => {
            let f = match v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.trim().parse::<f64>().ok(),
                Value::Bool(_) => None,
                _ => None,
            };
            f?
        }
    };
    if value.is_nan() {
        return None;
    }
    Some(value.clamp(1.0, MAX_TIMEOUT_S))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clean_output_strips_ansi_and_cr() {
        assert_eq!(clean_output("\x1b[32m[假CLI:pi]\x1b[0m hi\r\n"), "[假CLI:pi] hi");
        assert_eq!(clean_output("\x1b]0;title\x07body"), "body");
        // Python 同口径：strip() 不剥 \x07（非空白）
        assert_eq!(clean_output("\x1b]8;;u\x1b\\t\x07"), "t\u{7}");
        assert_eq!(clean_output("a\rb"), "ab");
        assert_eq!(clean_output("  padded  "), "padded");
        assert_eq!(clean_output(""), "");
    }

    #[test]
    fn build_prompt_shapes_context_block() {
        let msgs = json!([
            {"role": "user", "content": "第一轮问题"},
            {"role": "assistant", "content": "第一轮回答"},
            {"role": "user", "content": "第二轮问题"},
        ]);
        let p = build_prompt(&msgs).unwrap();
        assert!(p.contains("[上下文]"));
        assert!(p.contains("对方: 第一轮问题"));
        assert!(p.contains("你: 第一轮回答"));
        assert!(p.trim_end().ends_with("第二轮问题"));
        assert!(p.ends_with("第二轮问题"));

        // 单轮：只有任务正文，没有前缀
        let single = json!([{"role": "user", "content": "hello"}]);
        assert_eq!(build_prompt(&single).unwrap(), "hello");

        // 没有可用的 user 内容 → None
        assert!(build_prompt(&json!([{"role": "user", "content": "  "}])).is_none());
        assert!(build_prompt(&json!([])).is_none());
    }

    #[test]
    fn resolve_model_prefixes() {
        assert_eq!(resolve_model("pi/default").unwrap(), ("piagent".into(), "".into()));
        assert_eq!(resolve_model("claude/default").unwrap(), ("claudecode".into(), "".into()));
        assert_eq!(resolve_model("codex").unwrap(), ("codex".into(), "".into()));
        assert_eq!(
            resolve_model("codex/gpt-5-codex").unwrap(),
            ("codex".into(), "gpt-5-codex".into())
        );
        assert!(resolve_model("gpt-4o").is_none());
        assert!(resolve_model("openclaw/codex").is_none());
    }

    #[test]
    fn safe_session_id_converges() {
        assert_eq!(safe_session_id("abc def/ghi"), "a2a-abc-def-ghi");
        assert_eq!(safe_session_id(""), "a2a-anon");
        assert_eq!(safe_session_id("--"), "a2a-anon");
    }

    #[test]
    fn claude_uuid5_matches_python() {
        // Python: uuid.uuid5(uuid.NAMESPACE_URL, "cli_backend/claude/k1")
        //   -> '7c5e2fbf-...'；此处用已知交叉值钉住
        let a = claude_session_uuid("k1");
        let b = claude_session_uuid("k1");
        assert_eq!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'5'); // version 5
        assert_eq!(a, python_uuid5("k1"));
    }

    fn python_uuid5(raw: &str) -> String {
        // 交叉验证值：python3 -c "import uuid; print(uuid.uuid5(uuid.NAMESPACE_URL,
        // 'cli_backend/claude/k1'))"
        match raw {
            "k1" => "3def0fea-bad1-581a-a9ea-43c6cd6e8f14".to_string(),
            _ => panic!("no fixture"),
        }
    }

    #[test]
    fn clamp_timeout_bounds() {
        assert_eq!(clamp_timeout(None), Some(DEFAULT_TIMEOUT_S));
        assert_eq!(clamp_timeout(Some(&json!(1))), Some(1.0));
        assert_eq!(clamp_timeout(Some(&json!(99999))), Some(MAX_TIMEOUT_S));
        assert!(clamp_timeout(Some(&json!("abc"))).is_none());
        assert_eq!(clamp_timeout(Some(&json!("2.5"))), Some(2.5));
    }

    #[test]
    fn task_head_limits_40_chars() {
        assert_eq!(task_head("a\nb"), "a b");
        assert_eq!(task_head("短").chars().count(), 1);
        let long = "字".repeat(50);
        assert_eq!(task_head(&long).chars().count(), TASK_LOG_MAX);
    }

    #[test]
    fn codex_sessions_roundtrip() {
        let mut m = HashMap::new();
        m.insert("k1".to_string(), "11111111-2222-3333-4444-555555555555".to_string());
        m.insert("中文键".to_string(), "sid-2".to_string());
        let dir = std::env::temp_dir().join(format!("hacs_cb_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        assert!(save_codex_sessions(&m, path.to_str().unwrap()));
        let back = load_codex_sessions(path.to_str().unwrap());
        assert_eq!(back.get("k1").unwrap(), "11111111-2222-3333-4444-555555555555");
        assert_eq!(back.get("中文键").unwrap(), "sid-2");
        // 裸形态兼容
        std::fs::write(&path, r#"{"k3": "bare-sid"}"#).unwrap();
        let bare = load_codex_sessions(path.to_str().unwrap());
        assert_eq!(bare.get("k3").unwrap(), "bare-sid");
        // 损坏文件 → 空
        std::fs::write(&path, "not json").unwrap();
        assert!(load_codex_sessions(path.to_str().unwrap()).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_setdefault_semantics() {
        let reg = SessionRegistry::new();
        assert!(!reg.claude_seen_or_add("u1"));
        assert!(reg.claude_seen_or_add("u1"));
        assert_eq!(reg.codex_setdefault("s1", "a"), "a");
        assert_eq!(reg.codex_setdefault("s1", "b"), "a"); // 只首次写入
        reg.codex_merge(HashMap::from([("s1".into(), "z".into()), ("s2".into(), "y".into())]));
        assert_eq!(reg.codex_get("s1").unwrap(), "a"); // 不覆盖内存
        assert_eq!(reg.codex_get("s2").unwrap(), "y");
    }
}
