//! `group_watch_pending` —— 群守望器 · 待回应账域侧纯逻辑（对应 Python
//! `hub/l3_domain/group_watch_pending.py`，State 对应 L1 `group_watch_delivery.State`）。
//!
//! 口径（=Python，逐条对齐）：
//! * 人的 @ 优先；Agent 发的 @ 也回（--allow-agent 默认开），但每个 (群,成员)
//!   连续链式回话不超过 --max-chain 次，人一说话链条归零。
//! * 每轮每个 (群,成员) 最多回 MAX_PER_ROUND=1 条；先筛可用条目再挑
//!   （不能 pend[:1] 再过滤，否则一条 Agent 待回应会堵死后面人发的）。
//! * 处理过的 (sid,member,seq) 落盘幂等；(sid,member) 有最小 gap。
//! * 回复正文 @ 解析成结构化 mentions（过滤成群内成员；"所有人"→@all），
//!   frm 永远插在 mentions 首位。
//! * 投递（NATS call_one / post_reply HTTP）归 bin 组合根 —— 本件只出意图。

use hacs_x_identity::redact::safe_err;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

pub const CTX_LIMIT: usize = 12; // 喂给 Agent 的上下文条数
pub const MAX_PER_ROUND: usize = 1; // 每轮每个 (群,成员) 最多回几条

pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// "人"的成员 id 列表（env AGENT_TEAM_HUMAN_IDS 优先，逗号分隔）。
pub fn human_ids() -> Vec<String> {
    let raw = std::env::var("AGENT_TEAM_HUMAN_IDS").unwrap_or_else(|_| "<human-id>".to_string());
    raw.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

// ---------------- 状态（done / last / chain 三键单文件落盘，格式=Python） ----------------

#[derive(Debug, Default)]
pub struct WatchState {
    pub path: String,
    pub done: HashMap<String, i64>,  // "sid|member|seq" -> ts
    pub last: HashMap<String, i64>,  // "sid|member" -> ts（限流用）
    pub chain: HashMap<String, i64>, // "sid|member" -> 连续被 Agent 触发的回话次数
    pub dirty: bool,
}

fn key3(sid: &str, member: &str, seq: i64) -> String {
    format!("{sid}|{member}|{seq}")
}

fn key2(sid: &str, member: &str) -> String {
    format!("{sid}|{member}")
}

impl WatchState {
    pub fn load(path: &str) -> Self {
        let mut st = WatchState {
            path: path.to_string(),
            ..Default::default()
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return st; // 读不到 = 首次运行，空状态
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return st;
        };
        let fill = |m: &Value, out: &mut HashMap<String, i64>| {
            if let Some(o) = m.as_object() {
                for (k, val) in o {
                    let ts = val
                        .as_i64()
                        .or_else(|| val.as_str().and_then(|s| s.parse().ok()))
                        .unwrap_or(0);
                    out.insert(k.clone(), ts);
                }
            }
        };
        fill(&v["done"], &mut st.done);
        fill(&v["last"], &mut st.last);
        fill(&v["chain"], &mut st.chain);
        st
    }

    pub fn save(&mut self) {
        let mut o = Map::new();
        let dump = |m: &HashMap<String, i64>| {
            let mut x = Map::new();
            for (k, v) in m {
                x.insert(k.clone(), json!(v));
            }
            Value::Object(x)
        };
        o.insert("done".into(), dump(&self.done));
        o.insert("last".into(), dump(&self.last));
        o.insert("chain".into(), dump(&self.chain));
        if let Some(dir) = std::path::Path::new(&self.path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = format!("{}.tmp", self.path);
        if std::fs::write(&tmp, Value::Object(o).to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
        self.dirty = false;
    }

    pub fn seen(&self, sid: &str, member: &str, seq: i64) -> bool {
        self.done.contains_key(&key3(sid, member, seq))
    }

    pub fn mark(&mut self, sid: &str, member: &str, seq: i64) {
        self.done.insert(key3(sid, member, seq), now_epoch());
        self.dirty = true;
    }

    pub fn gap_ok(&self, sid: &str, member: &str, gap_s: f64) -> bool {
        let k = key2(sid, member);
        let last = self.last.get(&k).copied().unwrap_or(0);
        (now_epoch() as f64 - last as f64) >= gap_s
    }

    pub fn touch(&mut self, sid: &str, member: &str) {
        self.last.insert(key2(sid, member), now_epoch());
        self.dirty = true;
    }

    pub fn chain_count(&self, sid: &str, member: &str) -> i64 {
        self.chain.get(&key2(sid, member)).copied().unwrap_or(0)
    }

    pub fn bump_chain(&mut self, sid: &str, member: &str) -> i64 {
        let k = key2(sid, member);
        let c = self.chain_count(sid, member) + 1;
        self.chain.insert(k, c);
        self.dirty = true;
        c
    }

    pub fn reset_chain(&mut self, sid: &str, member: &str) {
        self.chain.insert(key2(sid, member), 0);
        self.dirty = true;
    }
}

// ---------------- 提示词 ----------------

pub fn build_prompt(
    sid: &str,
    title: &str,
    msgs: &[Value],
    member: &str,
    trigger: &Value,
) -> String {
    let mut lines = vec![format!(
        "【临时群《{}》的群聊记录，所有人可见】",
        if title.is_empty() { sid } else { title }
    )];
    let start = msgs.len().saturating_sub(CTX_LIMIT);
    for m in &msgs[start..] {
        let au = &m["author"];
        let who = au["display"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| au["id"].as_str())
            .unwrap_or("?");
        let kind = if au["type"] == "human" {
            "人"
        } else {
            "Agent"
        };
        let aud = m["meta"]["audience"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        lines.push(format!(
            "#{} {}({}) → {}: {}",
            m["seq"],
            who,
            kind,
            if aud.is_empty() { "-" } else { &aud },
            m["text"].as_str().unwrap_or("")
        ));
    }
    lines.push(String::new());
    let from40: String = trigger["from"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(40)
        .collect();
    lines.push(format!(
        "现在轮到**你（{member}）**回应 #{}（{} 发的）。",
        trigger["seq"], from40
    ));
    lines.push("要求：".into());
    lines.push(format!(
        "1. 直接给出你要发到群里的正文，第一行以 @{} 开头点名他。",
        trigger["from"].as_str().unwrap_or("")
    ));
    lines.push("2. 只说结论/承诺/要点，别复述本提示，别写书面报告。".into());
    lines.push("3. 这是群聊，全员可见 —— 有别的事要讲也在这里讲，不要点对点。".into());
    lines.join("\n")
}

/// 正文里 @ 出结构化 mentions（=Python re.findall(r"@([A-Za-z0-9_.\-]+|所有人)")）。
/// 过滤成群内成员；"所有人"→"@all"；frm 永远插首位；去重保序。
pub fn parse_mentions(reply: &str, frm: &str, members: &[String]) -> Vec<String> {
    let mut want: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let bytes = reply.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' && (i == 0 || bytes[i - 1] != b'\\') {
            // 解析 token：ASCII 名字字符或"所有人"
            let rest = &reply[i + 1..];
            let mut end = 0;
            for (ci, c) in rest.char_indices() {
                if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                    end = ci + c.len_utf8();
                } else {
                    break;
                }
            }
            let mut tok = &rest[..end];
            if tok.is_empty() && rest.starts_with("所有人") {
                tok = "所有人";
                end = "所有人".len();
            }
            if !tok.is_empty() || tok == "所有人" {
                let tid = if tok == "所有人" {
                    "@all".to_string()
                } else {
                    tok.to_string()
                };
                let in_group = tid == "@all" || members.iter().any(|m| m == &tid);
                if in_group && !seen.contains(&tid) {
                    seen.push(tid.clone());
                    want.push(tid);
                }
                i += 1 + end;
                continue;
            }
        }
        i += 1;
    }
    if !want.iter().any(|w| w == frm) {
        want.insert(0, frm.to_string());
    }
    want
}

// ---------------- 单轮：决策（纯函数，不改 state） ----------------

/// 投递意图：`{sid, member, seq, from, payload}`。
#[derive(Debug, Clone)]
pub struct Intent {
    pub sid: String,
    pub member: String,
    pub seq: i64,
    pub from: String,
    pub payload: Vec<u8>,
}

pub struct PlanArgs {
    pub allow_agent: bool,
    pub max_chain: i64,
    pub timeout: f64,
}

pub fn plan_round(
    sid: &str,
    member: &str,
    pend: &[Value],
    msgs: &[Value],
    state: &WatchState,
    args: &PlanArgs,
    title: &str,
    log: &mut dyn FnMut(String),
) -> Vec<Intent> {
    let humans = human_ids();
    // 先筛出所有可用条目 → 人的优先，Agent 发的按 --allow-agent 兜底（带链式上限）
    let usable: Vec<&Value> = pend
        .iter()
        .filter(|p| {
            let seq = p["seq"].as_i64().unwrap_or(0);
            let from = p["from"].as_str().unwrap_or("");
            seq != 0 && !from.is_empty() && from != member && !state.seen(sid, member, seq)
        })
        .collect();
    let is_human = |from: &str| humans.iter().any(|h| h == from);
    let human_trigs: Vec<&&Value> = usable
        .iter()
        .filter(|p| is_human(p["from"].as_str().unwrap_or("")))
        .collect();
    let agent_trigs: Vec<&&Value> = usable
        .iter()
        .filter(|p| !is_human(p["from"].as_str().unwrap_or("")))
        .collect();
    let mut trigs: Vec<&&Value> = human_trigs.iter().take(MAX_PER_ROUND).copied().collect();
    if trigs.is_empty() && !agent_trigs.is_empty() && args.allow_agent {
        if state.chain_count(sid, member) < args.max_chain {
            trigs = agent_trigs.iter().take(MAX_PER_ROUND).copied().collect();
        } else {
            log(format!(
                "链式回话达上限跳过 {sid} {member} 上限 {}",
                args.max_chain
            ));
        }
    }
    if trigs.is_empty() && !usable.is_empty() {
        log(format!(
            "有待回应但按规则不回 {sid} {member} 人发{}/Agent发{}",
            human_trigs.len(),
            agent_trigs.len()
        ));
    }

    let mut intents = Vec::new();
    for p in trigs {
        let prompt = build_prompt(sid, title, msgs, member, p);
        let payload = json!({
            "text": prompt,
            "timeout_s": args.timeout,
            "from": "hermes",
            "session_id": format!("meet:{sid}"),
        })
        .to_string()
        .into_bytes();
        intents.push(Intent {
            sid: sid.to_string(),
            member: member.to_string(),
            seq: p["seq"].as_i64().unwrap_or(0),
            from: p["from"].as_str().unwrap_or("").to_string(),
            payload,
        });
    }
    intents
}

/// 一轮全体扫账（编排壳；deliver/post 由回调注入，bin 组合根接 NATS/HTTP）。
pub struct RoundArgs {
    pub chat_base: String,
    pub gap: f64,
    pub timeout: f64,
    pub include_hermes: bool,
    pub allow_agent: bool,
    pub max_chain: i64,
}

/// 投递回调 trait（bin 注入 NATS/HTTP 实现；单测注入假件）。
pub trait Delivery {
    /// GET chat_base/api/sessions?limit=100 → {"sessions":[...]}
    fn list_sessions(&mut self) -> Option<Value>;
    /// GET chat_base/api/sessions/{sid}/messages?limit=60（X-Agent-Id: member）
    fn fetch_pending(&mut self, sid: &str, member: &str) -> Option<Value>;
    /// NATS 点名 → {"ok":bool,"reply":str,"error":...}
    fn deliver_ask(&mut self, member: &str, payload: &[u8], timeout: f64) -> Value;
    /// 以 member 身份写回群 → {"ok":bool,"error":...}
    fn post_reply(&mut self, sid: &str, member: &str, reply: &str, mentions: &[String]) -> Value;
}

/// 返回本轮已回话条数（=Python one_round 返回值）。
pub fn one_round(
    args: &RoundArgs,
    state: &mut WatchState,
    dlv: &mut dyn Delivery,
    log: &mut dyn FnMut(String),
) -> usize {
    let sess = match dlv.list_sessions() {
        Some(v) => v,
        None => return 0,
    };
    let groups: Vec<Value> = sess["sessions"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|s| s["kind"] == "meet" && s["state"] == "open")
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if groups.is_empty() {
        return 0;
    }
    let humans = human_ids();
    let mut acted = 0usize;
    for g in &groups {
        let sid = g["session_id"].as_str().unwrap_or("").to_string();
        let title = g["title"].as_str().unwrap_or("").to_string();
        let members: Vec<String> = g["members"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|m| {
                        m.as_str()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| m["id"].as_str().unwrap_or("").to_string())
                    })
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        for member in &members {
            if humans.iter().any(|h| h == member) {
                continue; // 人是发起方，不是被点名的 Agent
            }
            if member == "hermes" && !args.include_hermes {
                continue; // 默认不让 hermes 自己回（免自问自答）
            }
            let d = match dlv.fetch_pending(&sid, member) {
                Some(v) => v,
                None => continue,
            };
            let pend: Vec<Value> = d["my_pending"].as_array().cloned().unwrap_or_default();
            if pend.is_empty() {
                continue;
            }
            let msgs: Vec<Value> = d["messages"].as_array().cloned().unwrap_or_default();
            let plan_args = PlanArgs {
                allow_agent: args.allow_agent,
                max_chain: args.max_chain,
                timeout: args.timeout,
            };
            let intents = plan_round(&sid, member, &pend, &msgs, state, &plan_args, &title, log);
            for intent in intents {
                let seq = intent.seq;
                let frm = intent.from.clone();
                if !state.gap_ok(&sid, member, args.gap) {
                    log(format!("限流跳过 {sid} {member} {seq}"));
                    continue;
                }
                if humans.iter().any(|h| h == &frm) {
                    state.reset_chain(&sid, member); // 人说话了，链条归零
                } else {
                    state.bump_chain(&sid, member);
                }
                log(format!("→ 点名 {member} 群 {sid} seq {seq}"));
                let res = dlv.deliver_ask(member, &intent.payload, args.timeout);
                state.mark(&sid, member, seq);
                state.touch(&sid, member);
                state.save();
                let reply_raw = res["reply"].as_str().unwrap_or("").trim().to_string();
                if res["ok"] != json!(true) || reply_raw.is_empty() {
                    log(format!("   × 没拿到回复: {}", res["error"]));
                    continue;
                }
                let mut reply = reply_raw;
                if !reply.starts_with('@') {
                    reply = format!("@{frm} {reply}"); // Agent 发言必须 @（后端 422 硬拦）
                }
                let want = parse_mentions(&reply, &frm, &members);
                let posted = dlv.post_reply(&sid, member, &reply, &want);
                let ok = posted["ok"].as_bool().unwrap_or(true) && posted["error"].is_null();
                if ok {
                    log(format!(
                        "   ✓ 已回话 {member} → {frm} ({} 字)",
                        reply.chars().count()
                    ));
                    acted += 1;
                } else {
                    log(format!(
                        "   × 写回群失败: {}",
                        safe_err(&posted.to_string())
                    ));
                }
            }
        }
    }
    acted
}

// ---------------- 单测 ----------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn st() -> WatchState {
        WatchState {
            path: "/tmp/gw_test_state.json".into(),
            ..Default::default()
        }
    }

    fn plan_args() -> PlanArgs {
        PlanArgs {
            allow_agent: true,
            max_chain: 2,
            timeout: 600.0,
        }
    }

    #[test]
    fn test_state_roundtrip() {
        let mut s = st();
        assert!(!s.seen("g1", "alpha", 5));
        s.mark("g1", "alpha", 5);
        s.touch("g1", "alpha");
        s.bump_chain("g1", "alpha");
        assert!(s.seen("g1", "alpha", 5));
        assert_eq!(s.chain_count("g1", "alpha"), 1);
        s.save();
        let s2 = WatchState::load("/tmp/gw_test_state.json");
        assert!(s2.seen("g1", "alpha", 5));
        assert_eq!(s2.chain_count("g1", "alpha"), 1);
        assert!(s2.gap_ok("g1", "alpha", 0.0));
        assert!(!s2.gap_ok("g1", "alpha", 3600.0));
    }

    #[test]
    fn test_gap_ok_missing_key() {
        let s = st();
        assert!(s.gap_ok("g9", "beta", 30.0)); // 从没回过 → 允许
    }

    #[test]
    fn test_plan_human_priority_and_agent_chain() {
        std::env::set_var("AGENT_TEAM_HUMAN_IDS", "boss");
        let mut s = st();
        let pend = vec![
            json!({"seq": 2, "from": "agentx"}),
            json!({"seq": 1, "from": "boss"}),
        ];
        let msgs = vec![
            json!({"seq": 1, "author": {"id": "boss", "type": "human", "display": "老板"}, "text": "在吗", "meta": {"audience": ["alpha"]}}),
        ];
        let intents = plan_round(
            "g1",
            "alpha",
            &pend,
            &msgs,
            &s,
            &plan_args(),
            "测试群",
            &mut |_| {},
        );
        // 人的优先：先回 boss 的 #1，不取 pend[:1]
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].seq, 1);
        assert_eq!(intents[0].from, "boss");
        let payload = String::from_utf8(intents[0].payload.clone()).unwrap();
        let v: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["from"], "hermes");
        assert_eq!(v["session_id"], "meet:g1");
        assert!(v["text"].as_str().unwrap().contains("@boss 开头点名"));
        assert!(v["text"]
            .as_str()
            .unwrap()
            .contains("#1 老板(人) → alpha: 在吗"));

        // 人发的不在了 → Agent 顶上（chain < max_chain）
        let pend2 = vec![json!({"seq": 2, "from": "agentx"})];
        let intents = plan_round(
            "g1",
            "alpha",
            &pend2,
            &msgs,
            &s,
            &plan_args(),
            "测试群",
            &mut |_| {},
        );
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].from, "agentx");

        // 链式达上限 → 不回
        s.chain.insert("g1|alpha".into(), 2);
        let intents = plan_round(
            "g1",
            "alpha",
            &pend2,
            &msgs,
            &s,
            &plan_args(),
            "测试群",
            &mut |_| {},
        );
        assert!(intents.is_empty());

        // --no-allow-agent → 不回 Agent 发的
        let args2 = PlanArgs {
            allow_agent: false,
            max_chain: 2,
            timeout: 600.0,
        };
        s.chain.insert("g1|alpha".into(), 0);
        let intents = plan_round(
            "g1",
            "alpha",
            &pend2,
            &msgs,
            &s,
            &args2,
            "测试群",
            &mut |_| {},
        );
        assert!(intents.is_empty());
    }

    #[test]
    fn test_plan_seen_filter_and_self_skip() {
        std::env::set_var("AGENT_TEAM_HUMAN_IDS", "boss");
        let mut s = st();
        s.mark("g1", "alpha", 1);
        let pend = vec![json!({"seq": 1, "from": "boss"})];
        let intents = plan_round("g1", "alpha", &pend, &[], &s, &plan_args(), "", &mut |_| {});
        assert!(intents.is_empty()); // 已处理过 → 不重发
        let pend = vec![json!({"seq": 3, "from": "alpha"})]; // 自己发的
        let intents = plan_round("g1", "alpha", &pend, &[], &s, &plan_args(), "", &mut |_| {});
        assert!(intents.is_empty());
    }

    #[test]
    fn test_parse_mentions() {
        let members = vec!["boss".to_string(), "alpha".to_string(), "gamma".to_string()];
        // 正文 @ + frm 补位 + 去重 + 群外过滤
        let m = parse_mentions("@boss 收到，@alpha @外人 看看 @boss", "gamma", &members);
        assert_eq!(m, vec!["gamma", "boss", "alpha"]);
        // 无 @ → 只补 frm
        assert_eq!(parse_mentions("好的", "alpha", &members), vec!["alpha"]);
        // 所有人 → @all
        assert_eq!(
            parse_mentions("@所有人 注意", "alpha", &members),
            vec!["alpha", "@all"]
        );
        // 回复本来就以 @frm 开头 → 不重复插
        assert_eq!(
            parse_mentions("@alpha done", "alpha", &members),
            vec!["alpha"]
        );
    }

    #[test]
    fn test_prompt_ctx_limit() {
        std::env::set_var("AGENT_TEAM_HUMAN_IDS", "boss");
        let mut msgs = Vec::new();
        for i in 0..20 {
            msgs.push(
                json!({"seq": i, "author": {"id": "u", "type": "human"}, "text": format!("m{i}")}),
            );
        }
        let trig = json!({"seq": 19, "from": "boss"});
        let p = build_prompt("g1", "T", &msgs, "alpha", &trig);
        // 只带最近 12 条
        assert!(p.contains("#8 u(人) → -: m8"));
        assert!(!p.contains("#7 u(人)"));
        // 空标题回退 sid
        let p2 = build_prompt("gX", "", &[], "alpha", &trig);
        assert!(p2.contains("《gX》"));
    }

    struct FakeDlv {
        sessions: Value,
        pending: HashMap<String, Value>,
        asks: Vec<String>,
        posted: Vec<(String, String)>,
        reply: String,
    }

    impl FakeDlv {
        fn new() -> Self {
            let mut pending = HashMap::new();
            pending.insert(
                "g1|alpha".to_string(),
                json!({
                    "my_pending": [{"seq": 4, "from": "boss"}],
                    "messages": [{"seq": 4, "author": {"id": "boss", "type": "human"}, "text": "开会", "meta": {}}]
                }),
            );
            FakeDlv {
                sessions: json!({"sessions": [
                    {"session_id": "g1", "kind": "meet", "state": "open",
                     "title": "临时会", "members": ["boss", "alpha", "hermes"]},
                    {"session_id": "g2", "kind": "chat", "state": "open", "members": []}
                ]}),
                pending,
                asks: Vec::new(),
                posted: Vec::new(),
                reply: "@boss 收到".to_string(),
            }
        }
    }

    impl Delivery for FakeDlv {
        fn list_sessions(&mut self) -> Option<Value> {
            Some(self.sessions.clone())
        }
        fn fetch_pending(&mut self, sid: &str, member: &str) -> Option<Value> {
            self.pending.get(&format!("{sid}|{member}")).cloned()
        }
        fn deliver_ask(&mut self, member: &str, _payload: &[u8], _timeout: f64) -> Value {
            self.asks.push(member.to_string());
            json!({"ok": true, "reply": self.reply})
        }
        fn post_reply(
            &mut self,
            sid: &str,
            member: &str,
            reply: &str,
            mentions: &[String],
        ) -> Value {
            self.posted.push((
                sid.to_string(),
                format!("{member}|{reply}|{}", mentions.join(",")),
            ));
            json!({"ok": true})
        }
    }

    #[test]
    fn test_one_round_e2e() {
        std::env::set_var("AGENT_TEAM_HUMAN_IDS", "boss");
        let tmp = format!("/tmp/gw_one_round_{}.json", std::process::id());
        let _ = std::fs::remove_file(&tmp);
        let mut state = WatchState::load(&tmp);
        let mut dlv = FakeDlv::new();
        let args = RoundArgs {
            chat_base: "http://x".into(),
            gap: 0.0,
            timeout: 600.0,
            include_hermes: false,
            allow_agent: true,
            max_chain: 2,
        };
        let mut log = |_: String| {};
        let n = one_round(&args, &mut state, &mut dlv, &mut log);
        assert_eq!(n, 1);
        assert_eq!(dlv.asks, vec!["alpha"]); // boss 是人、hermes 默认跳过
                                             // 回复补 @ + mentions：frm=boss 在首
        assert_eq!(
            dlv.posted[0],
            ("g1".to_string(), "alpha|@boss 收到|boss".to_string())
        );
        assert!(state.seen("g1", "alpha", 4));
        // 第二轮：已 mark → 幂等不再回
        let n2 = one_round(&args, &mut state, &mut dlv, &mut log);
        assert_eq!(n2, 0);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_one_round_no_reply() {
        std::env::set_var("AGENT_TEAM_HUMAN_IDS", "boss");
        let tmp = format!("/tmp/gw_one_round2_{}.json", std::process::id());
        let _ = std::fs::remove_file(&tmp);
        let mut state = WatchState::load(&tmp);
        let mut dlv = FakeDlv::new();
        dlv.reply = String::new(); // 空回复 → 不写回群
        let args = RoundArgs {
            chat_base: "http://x".into(),
            gap: 0.0,
            timeout: 600.0,
            include_hermes: false,
            allow_agent: true,
            max_chain: 2,
        };
        let n = one_round(&args, &mut state, &mut dlv, &mut |_| {});
        assert_eq!(n, 0);
        assert!(dlv.posted.is_empty());
        assert!(state.seen("g1", "alpha", 4)); // 但已记账（=Python 先 mark 再看回复）
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_one_round_gap_blocks() {
        std::env::set_var("AGENT_TEAM_HUMAN_IDS", "boss");
        let tmp = format!("/tmp/gw_one_round3_{}.json", std::process::id());
        let _ = std::fs::remove_file(&tmp);
        let mut state = WatchState::load(&tmp);
        state.mark("g1", "alpha", 99); // 塞个 last 不行——touch 才写 last
        state.touch("g1", "alpha");
        let mut dlv = FakeDlv::new();
        let args = RoundArgs {
            chat_base: "http://x".into(),
            gap: 3600.0,
            timeout: 600.0,
            include_hermes: false,
            allow_agent: true,
            max_chain: 2,
        };
        let n = one_round(&args, &mut state, &mut dlv, &mut |_| {});
        assert_eq!(n, 0); // 限流跳过
        assert!(dlv.posted.is_empty());
        let _ = std::fs::remove_file(&tmp);
    }
}
