//! L1 通信层：**A2A 边缘端点核心逻辑**（对应 Python `node/l1_transport/a2a/a2a_edge.py`）。
//!
//! Python 原件分两层：纯逻辑（配置/鉴权/agent-card/task_loop/backend 调用形状）与
//! HTTP 装配（ThreadingHTTPServer + web_gateway 路由）。Rust 侧拆成：
//!   * 本模块 —— 纯逻辑（可单测）；
//!   * `hacs-c::bin/a2a_svc` —— HTTP 装配（复用 L1 `web_gateway`，HTTP/1.1 +
//!     `sys_version=""`，`Server: a2a_svc/<VERSION>`）。
//!
//! 关键口径（与 Python 逐条对齐）：
//!   * 鉴权：`Authorization: Bearer <token>` 的 **sha256 反查 peers 表**，恒时比较，
//!     绝不采信请求体 from；
//!   * B2 续跑循环：带 session_id 且未关 task_loop 时连续续跑，终止出口
//!     DONE / BLOCKED / max_rounds(8) / budget（≤ min(timeout, 600)）；
//!   * 上下文自愈：软阈值(0.75)注入压缩指令，硬阈值(0.85)取交接摘要换新 session；
//!   * 会话键：`user = "a2a:<caller>:<session_id>"`；
//!   * 能力端口（backend_of / session_headers / read_usage / pressure / get_routes）
//!     未注入时降级为「不知道」，绝不编造数字。
//!
//! 注意：能力端口在 Rust 侧由 bin 装配时以 `Capabilities` 结构注入（组合根 a2a_svc
//! 后续批次再实现真实能力，当前全部走降级口径）。

use serde_json::{json, Value};

pub const VERSION: &str = "2.2";
pub const PROTOCOL_VERSION: &str = "0.2.5";
pub const DEFAULT_TIMEOUT_S: f64 = 120.0;
pub const DETAIL_MAX: usize = 200;
pub const MAX_BODY_BYTES: usize = 262_144;
pub const SESSION_ID_MAX: usize = 128;

pub const TASK_LOOP_MAX_ROUNDS: u32 = 8;
pub const TASK_LOOP_MAX_TIMEOUT_S: f64 = 600.0;
pub const CTX_SOFT_RATIO: f64 = 0.75;
pub const CTX_HARD_RATIO: f64 = 0.85;
pub const CTX_MIN_TOKENS_FLOOR: i64 = 8000;

pub const TASK_LOOP_CONTINUE_PROMPT: &str =
    "【续跑指令／自动注入】上一轮你只回了一段话就停了。现在**继续执行**：\
本回合不是只回答一次就结束——请真的去调工具把活做完（跑命令、读文件、写代码、重启、实测）。\
每完成一步就在回复里简短记一句；\
全部做完时，在回复最后**单独一行**写 DONE；\
确实卡死、无法推进时，在回复最后**单独一行**写 BLOCKED，并说明卡在哪。\
不要只写承诺，要动手。";

pub const CTX_COMPACT_PROMPT: &str =
    "【上下文自愈／自动注入】本会话上下文已较大，为免撑爆窗口，请**立即把至今的进展压缩成\
一段「交接摘要」**（不调工具，只输出摘要本身，务必简短）：\n\
1) 本次任务目标；2) 已完成的关键步骤与产物路径；3) 待办事项与下一步；\
4) 关键约束/坑。\n摘要在 400 字以内，供新会话无缝接力。";

/// 交接摘要注入（摘要文本嵌进模板）。
pub fn ctx_handoff_prompt(summary: &str) -> String {
    let tpl: &str =
        "【新会话接力／自动注入】上一个会话上下文已满，已切换到这个新会话继续同一任务。\
以下是从上一会话带过来的交接摘要，请据此**接着干**（不要从头重做已完成的步骤）：\n\
---- 交接摘要开始 ----\nSLOT_SUMMARY\n---- 交接摘要结束 ----\n\
请继续执行未完成的动作；全部做完时在最后单独一行写 DONE，卡死则写 BLOCKED。";
    tpl.replace("SLOT_SUMMARY", summary)
}

// ---------------------------------------------------------------- 终止标记判定

/// 终止标记只认「独占一行的标记词」（DONE / BLOCKED，前可有 `任务` 与 Markdown 装饰符）。
/// 与 Python `_TASK_DONE_RE` 同口径：`^\s*(?:[-*>#`\[\]]*\s*)*(?:任务)?\s*WORD\s*(?:...)*$`。
fn strip_markdown_noise(line: &str) -> &str {
    let mut s = line.trim();
    loop {
        let t = s.trim_start_matches(['-', '*', '>', '#', '`', '[', ']', ' ', '\t']);
        s = t.trim_end_matches(['-', '*', '>', '#', '`', '[', ']', ' ', '\t']);
        let t2 = s.trim_start();
        if t2 == s {
            return s;
        }
        s = t2;
    }
}

fn terminal_marker(reply: &str, word: &str) -> bool {
    for line in reply.lines() {
        let mut s = strip_markdown_noise(line);
        if let Some(rest) = s.strip_prefix("任务") {
            s = rest.trim();
        }
        if s.eq_ignore_ascii_case(word) {
            return true;
        }
    }
    false
}

pub fn is_done(reply: &str) -> bool {
    terminal_marker(reply, "DONE")
}

pub fn is_blocked(reply: &str) -> bool {
    terminal_marker(reply, "BLOCKED")
}

// ---------------------------------------------------------------- 成员配置

/// 成员级续跑开关：`task_loop` 缺省=开；bool 直取；字符串按
/// `("0","false","off","no","")` 否认（与 Python `_loop_enabled` 同口径）。
pub fn loop_enabled(agent: &Value) -> bool {
    match agent.get("task_loop") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no" | ""
        ),
        Some(_) => true,
    }
}

/// 回合硬上限收敛：调用方 timeout 与成员 `max_timeout_s` 取小。
pub fn effective_timeout(agent: &Value, timeout_s: f64) -> f64 {
    match agent.get("max_timeout_s").and_then(Value::as_f64) {
        Some(cap) if cap > 0.0 && timeout_s > cap => cap,
        _ => timeout_s,
    }
}

/// 会话键：`a2a:<caller>:<session_id>`（进 OpenAI `user` 字段）。
pub fn session_user_field(caller: &str, session_id: &str) -> String {
    format!(
        "a2a:{}:{}",
        if caller.is_empty() { "unknown" } else { caller },
        session_id
    )
}

// ---------------------------------------------------------------- 配置文件

/// 解析 agents.json → (node_id, agents, advertise_host, advertise_port)。
/// 结构错误返回人话错误串（对应 Python `load_config` 的 raise ValueError）。
pub fn load_config(text: &str) -> Result<(String, Value, String, Option<i64>), String> {
    let cfg: Value =
        serde_json::from_str(text).map_err(|e| format!("agents.json 不是合法 JSON: {e}"))?;
    let obj = cfg.as_object().ok_or("agents.json 顶层必须是对象")?;
    let node_id = obj
        .get("node_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let agents = match obj.get("agents") {
        None | Some(Value::Null) => json!({}),
        Some(v) if v.is_object() => v.clone(),
        Some(_) => return Err("agents.json 的 agents 字段必须是对象".to_string()),
    };
    let host = obj
        .get("host")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let port = obj.get("port").and_then(Value::as_i64);
    Ok((node_id, agents, host, port))
}

/// 解析 peers.json → {sha256hex: 成员id}（digest 归一化小写去空白）。
pub fn load_peers(text: &str) -> Result<Vec<(String, String)>, String> {
    let peers: Value =
        serde_json::from_str(text).map_err(|e| format!("peers.json 不是合法 JSON: {e}"))?;
    let obj = peers.as_object().ok_or("peers.json 顶层必须是对象")?;
    let mut out = Vec::new();
    for (pid, digest) in obj {
        let d = digest.as_str().unwrap_or("").trim().to_ascii_lowercase();
        if !d.is_empty() {
            out.push((d, pid.clone()));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- 鉴权

/// Bearer token → 成员 id。`allow_anon` 直接 "anonymous"。
/// 恒时比较语义：遍历全表不提前退出（防比对次数侧信道）。
pub fn auth_peer(
    header: Option<&str>,
    peer_map: &[(String, String)],
    allow_anon: bool,
) -> Option<String> {
    if allow_anon {
        return Some("anonymous".to_string());
    }
    let header = header?;
    let token = header.strip_prefix("Bearer ")?.trim();
    if token.is_empty() {
        return None;
    }
    let digest = sha256_hex(token.as_bytes());
    let mut matched = None;
    for (known, pid) in peer_map {
        if ct_eq_bytes(digest.as_bytes(), known.as_bytes()) {
            matched = Some(pid.clone());
        }
    }
    matched
}

/// SHA-256（零依赖自实现，与 Python hashlib.sha256 逐字节一致）。
pub fn sha256_hex(data: &[u8]) -> String {
    let h = sha256(data);
    h.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bitlen = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// 恒时字节比较（长度不同的直接 false —— sha256hex 长度恒 64，与 Python 同口径）。
pub fn ct_eq_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------- Agent Card

/// A2A 官方形态 Agent Card（字段名不可改）。
pub fn build_agent_card(
    agent_id: &str,
    cfg: &Value,
    advertise_host: &str,
    advertise_port: u16,
) -> Value {
    let description = cfg.get("description").and_then(Value::as_str).unwrap_or("");
    let skills: Vec<Value> = cfg
        .get("skills")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|s| s.as_str())
                .map(|s| json!({"id": s, "name": s, "description": description}))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "name": agent_id,
        "description": description,
        "url": format!("http://{}:{}/a2a/{}", advertise_host, advertise_port, agent_id),
        "version": VERSION,
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {"streaming": false, "pushNotifications": false},
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": skills,
        "securitySchemes": {"bearer": {"type": "http", "scheme": "bearer"}},
        "security": [{"bearer": []}],
    })
}

// ---------------------------------------------------------------- 请求体校验（POST /a2a/<agent>/messages 的纯函数前半段）

pub struct ParsedMessage {
    pub text: String,
    pub timeout_s: f64,
    pub session_id: Option<String>,
}

impl std::fmt::Debug for ParsedMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedMessage")
            .field("text", &self.text)
            .field("timeout_s", &self.timeout_s)
            .field("session_id", &self.session_id)
            .finish()
    }
}

/// 校验 `{text, timeout_s?, session_id?}`。失败返回 (status, error)。
pub fn parse_message_body(body: &Value) -> Result<ParsedMessage, (u16, &'static str)> {
    let text = match body.get("text").and_then(Value::as_str) {
        Some(t) if !t.trim().is_empty() => t.to_string(),
        _ => return Err((400, "bad_request")),
    };
    let timeout_s = match body.get("timeout_s") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_S,
        Some(v) => {
            let raw = match v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.trim().parse::<f64>().ok(),
                _ => None,
            };
            match raw {
                None => return Err((400, "bad_request")),
                Some(t) if t <= 0.0 => DEFAULT_TIMEOUT_S,
                Some(t) => t,
            }
        }
    };
    let session_id = match body.get("session_id") {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            Some(s.trim().chars().take(SESSION_ID_MAX).collect::<String>())
        }
        _ => None,
    };
    Ok(ParsedMessage {
        text,
        timeout_s,
        session_id,
    })
}

// ---------------------------------------------------------------- backend 调用形状（纯函数部分）

/// `_single_round` 的请求形状：`POST <backend>/chat/completions`。
/// 返回 (url, payload_json)。
pub fn build_single_round_request(
    agent: &Value,
    messages: &[Value],
    user_field: Option<&str>,
) -> Result<(String, Value), (u16, Value)> {
    let backend = agent
        .get("backend")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string();
    if backend.is_empty() {
        return Err((
            502,
            json!({"ok": false, "error": "backend_error", "detail": "backend not configured"}),
        ));
    }
    let mut payload = json!({
        "model": agent.get("model").cloned().unwrap_or(Value::String(String::new())),
        "messages": messages,
        "max_tokens": agent.get("max_tokens").and_then(Value::as_i64).unwrap_or(2000),
    });
    if let Some(u) = user_field {
        payload["user"] = json!(u);
    }
    Ok((format!("{backend}/chat/completions"), payload))
}

/// 从 OpenAI 形状响应里抠 `choices[0].message.content`；抠不到 → None（不编造）。
pub fn extract_openai_reply(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    v.pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

/// 错误 detail 截断（Python `[:DETAIL_MAX]` 按字符）。
pub fn truncate_detail(s: &str) -> String {
    s.chars().take(DETAIL_MAX).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vectors() {
        // python3 -c "import hashlib;print(hashlib.sha256(b'').hexdigest())"
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex("你好".as_bytes()),
            "670d9743542cae3ea7ebe36af56bd53648b0a1126162e78d81a32934a711302e"
        );
    }

    #[test]
    fn auth_peer_bearer_sha256() {
        let token = "secret-token";
        let digest = sha256_hex(token.as_bytes());
        let peers = vec![
            (digest.clone(), "top001".to_string()),
            (sha256_hex(b"other").to_string(), "lobster".to_string()),
        ];
        assert_eq!(
            auth_peer(Some(&format!("Bearer {token}")), &peers, false),
            Some("top001".to_string())
        );
        assert_eq!(auth_peer(Some("Bearer wrong"), &peers, false), None);
        assert_eq!(auth_peer(Some("Basic x"), &peers, false), None);
        assert_eq!(auth_peer(Some("Bearer "), &peers, false), None);
        assert_eq!(auth_peer(None, &peers, false), None);
        assert_eq!(
            auth_peer(Some("Bearer anything"), &peers, true),
            Some("anonymous".to_string())
        );
    }

    #[test]
    fn terminal_markers_exclusive_line() {
        assert!(is_done("干活中...\nDONE"));
        assert!(is_done("- DONE"));
        assert!(is_done("> **DONE**"));
        assert!(is_done("任务 DONE"));
        assert!(is_done("done")); // 大小写不敏感
        assert!(!is_done("I am done with this")); // 非独占一行
        assert!(!is_done("DONE: xxx"));
        assert!(!is_done("任务完成 done 好的"));
        assert!(is_blocked("BLOCKED\n说明"));
        assert!(!is_blocked("blocked by network"));
    }

    #[test]
    fn loop_enabled_string_forms() {
        assert!(loop_enabled(&json!({})));
        assert!(loop_enabled(&json!({"task_loop": true})));
        assert!(!loop_enabled(&json!({"task_loop": false})));
        assert!(!loop_enabled(&json!({"task_loop": "0"})));
        assert!(!loop_enabled(&json!({"task_loop": "off"})));
        assert!(loop_enabled(&json!({"task_loop": "1"})));
        assert!(loop_enabled(&json!({"task_loop": "yes"})));
    }

    #[test]
    fn timeout_cap_converges() {
        let agent = json!({"max_timeout_s": 60});
        assert_eq!(effective_timeout(&agent, 120.0), 60.0);
        assert_eq!(effective_timeout(&agent, 30.0), 30.0);
        assert_eq!(
            effective_timeout(&json!({"max_timeout_s": -1}), 120.0),
            120.0
        );
        assert_eq!(effective_timeout(&json!({}), 120.0), 120.0);
    }

    #[test]
    fn session_user_field_shape() {
        assert_eq!(session_user_field("top001", "meet-1"), "a2a:top001:meet-1");
        assert_eq!(session_user_field("", "s"), "a2a:unknown:s");
    }

    #[test]
    fn config_loading() {
        let (node, agents, host, port) = load_config(
            r#"{"node_id":"thinkbook","host":"tb.example","port":9920,"agents":{"tester":{"description":"d","skills":["s1"]}}}"#,
        )
        .unwrap();
        assert_eq!(node, "thinkbook");
        assert_eq!(host, "tb.example");
        assert_eq!(port, Some(9920));
        assert!(agents.get("tester").is_some());
        assert!(load_config("[]").is_err());
        assert!(load_config(r#"{"agents":[]}"#).is_err());
        let peers = load_peers(r#"{"top001": "  ABCDEF1234  "}"#).unwrap();
        assert_eq!(peers[0].0, "abcdef1234");
        assert!(load_peers("[1]").is_err());
    }

    #[test]
    fn agent_card_shape() {
        let cfg = json!({"description": "测试员", "skills": ["a", "b"]});
        let card = build_agent_card("tester", &cfg, "tb.example", 9920);
        assert_eq!(card["name"], "tester");
        assert_eq!(card["url"], "http://tb.example:9920/a2a/tester");
        assert_eq!(card["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(card["skills"].as_array().unwrap().len(), 2);
        assert_eq!(card["skills"][0]["description"], "测试员");
        assert_eq!(card["securitySchemes"]["bearer"]["scheme"], "bearer");
        assert_eq!(card["capabilities"]["streaming"], false);
    }

    #[test]
    fn message_body_parsing() {
        let ok =
            parse_message_body(&json!({"text": "你好", "timeout_s": 30, "session_id": " s1 "}))
                .unwrap();
        assert_eq!(ok.text, "你好");
        assert_eq!(ok.timeout_s, 30.0);
        assert_eq!(ok.session_id.as_deref(), Some("s1"));
        // session 超 128 截断
        let long: String = "x".repeat(200);
        let ok = parse_message_body(&json!({"text": "t", "session_id": long})).unwrap();
        assert_eq!(ok.session_id.unwrap().len(), SESSION_ID_MAX);
        // 空 text → 400
        assert_eq!(
            parse_message_body(&json!({"text": "  "})).unwrap_err().0,
            400
        );
        assert_eq!(parse_message_body(&json!({})).unwrap_err().0, 400);
        // timeout 非法 → 400；<=0 → 默认
        assert_eq!(
            parse_message_body(&json!({"text": "t", "timeout_s": "abc"}))
                .unwrap_err()
                .0,
            400
        );
        assert_eq!(
            parse_message_body(&json!({"text": "t", "timeout_s": -5}))
                .unwrap()
                .timeout_s,
            DEFAULT_TIMEOUT_S
        );
    }

    #[test]
    fn single_round_request_shape() {
        let agent = json!({"backend": "http://127.0.0.1:9970/", "model": "claude/default", "max_tokens": 500, "api_key": "k1"});
        let (url, payload) = build_single_round_request(
            &agent,
            &[json!({"role": "user", "content": "hi"})],
            Some("a2a:top001:s1"),
        )
        .unwrap();
        assert_eq!(url, "http://127.0.0.1:9970/chat/completions"); // 尾斜杠剥掉
        assert_eq!(payload["max_tokens"], 500);
        assert_eq!(payload["user"], "a2a:top001:s1");
        assert_eq!(payload["messages"].as_array().unwrap().len(), 1);
        // backend 缺失 → 502
        let err = build_single_round_request(&json!({}), &[], None).unwrap_err();
        assert_eq!(err.0, 502);
        assert_eq!(err.1["error"], "backend_error");
    }

    #[test]
    fn openai_reply_extraction() {
        let body = r#"{"choices":[{"message":{"content":"回复正文"}}]}"#.as_bytes();
        assert_eq!(extract_openai_reply(body).as_deref(), Some("回复正文"));
        assert_eq!(extract_openai_reply(br#"{"choices":[]}"#), None);
        assert_eq!(extract_openai_reply(b"not json"), None);
    }

    #[test]
    fn handoff_prompt_contains_summary() {
        let p = ctx_handoff_prompt("摘要内容");
        assert!(p.contains("---- 交接摘要开始 ----"));
        assert!(p.contains("摘要内容"));
        assert!(p.contains("DONE"));
    }
}
