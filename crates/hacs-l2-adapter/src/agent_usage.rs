//! L2 适配层：**上下文水位真源**（单一权威口径）。对应 Python `node/l2_adapter/agent_usage.py`。
//!
//! 职责（只此一件）：把「某个 Agent 在某个会话上用了多少上下文、分母是多少」按不同后端的
//! 特征算出唯一答案。**不含任何投递/会话/轮转逻辑**（那是 L1），也**不写任何文件**（只读）。
//!
//! 契约：
//! - [`read_usage`] → `(used, window)`；后端分派 hermes / openclaw / other。读不到一律 `None`。
//! - [`read_watermark`] → `{"used","window","pct","text"}`；本机直读 → 远端 `/ctx-usage` 回退。
//! - [`pressure`] → `used / window`；低于下限返回 `0.0`；读不到 `None`。
//! - [`window_for`] → 分母。**唯一口径链**：Hermes 生效配置 → 模型目录 → 兜底表 → 别名兜底。
//!
//! 零硬编码 · 零存储句柄：路径/凭据一律**调用时**向 L0 节点档案要，本层只留适配口径。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use hacs_l0_host::node_profile as npf;
use hacs_l1_transport::http_client;
use serde_json::Value;

/// 兜底表（取不到生效值时用）。键为 model 子串（小写，**最长匹配优先**）。
///
/// ⚠️ 这张表是**统一后的唯一口径**：分母必须是 Agent「实际生效」的窗口，不是模型目录里的
/// 理论上限。实测依据：Hermes 模型目录对 deepseek-v4-flash 写 1,000,000，而本机生效配置
/// 把窗口覆盖成 200,000，Hermes 自己的压缩阈值就按 200,000 算。自愈若按 1,000,000 算，
/// 水位被低估 5 倍 —— 等于把自愈静默关掉。
pub const CTX_WINDOW_FALLBACK: &[(&str, i64)] = &[
    ("deepseek-v4-flash", 200_000),
    ("deepseek-v4-pro", 200_000),
    ("deepseek", 128_000),
];

/// A2A 会话在 Hermes 侧的稳定会话键格式。**改格式 = 换会话身份 = 记忆域断裂**，别动。
pub const HERMES_SESSION_KEY_FMT: &str = "a2a:%s:%s";

// ---------------------------------------------------------------- 路径与凭据（薄委托）

/// OpenClaw 会话档案路径（未配置 → 该后端降级为不自愈）。
pub fn sessions_json_path() -> String {
    npf::openclaw_sessions_json()
}

/// Hermes `state.db` 路径（未配置 → Hermes 后端降级为不自愈）。
pub fn hermes_state_db() -> String {
    npf::hermes_state_db()
}

/// 本节点 a2a 服务基址（远端水位回退用）。
pub fn a2a_remote_base() -> String {
    npf::a2a_probe_base()
}

/// a2a 服务令牌：先 env，再令牌文件。
pub fn a2a_remote_token() -> String {
    npf::a2a_probe_token()
}

/// Hermes 主目录（读不到返回空串）。
pub fn hermes_home() -> String {
    npf::hermes_home()
}

// ---------------------------------------------------------------- 窗口分母

/// Hermes 部署生效窗口：`config.yaml` 的 `model:` 段里声明的 `(模型名, context_length)`。
///
/// `model.context_length` **只对 `model.default` 那一个模型生效**，不是全局分母。
/// 读不到返回 `("", 0)`。**降级口径留在本层** —— 读/解析出错一律吞成 `("", 0)` 并记 debug。
pub fn effective_window_from_config() -> (String, i64) {
    match npf::hermes_config_model_window() {
        Ok(v) => v,
        Err(exc) => {
            crate::logx::debug(
                "agent_usage",
                &format!("读 Hermes config 失败({exc})，跳过生效窗口覆盖"),
            );
            (String::new(), 0)
        }
    }
}

/// 窗口分母，一条链四个来源（先权威、后兜底）。全都不命中返回 `0` = 「不知道」。
///
/// ① Hermes **部署生效**窗口（`config.yaml` 的 `model.context_length`，仅当模型名命中）；
/// ② Hermes 自带模型目录 `DEFAULT_CONTEXT_LENGTHS` —— ⚠️ **Rust 侧跳过**：它要靠 import
///    Python 包取（见 `docs/PARITY.md` 的已知偏差）；
/// ③ 本模块兜底表（保守值，偏小 = 自愈偏早，属安全方向）；
/// ④ **Hermes 别名兜底**（仅 `backend=hermes`）：`agents.json` 里的 `model` 常写部署别名，
///    目录里查不到；此时该实例的 config 生效窗口才是真分母。
pub fn window_for(model: &str, backend: &str) -> i64 {
    let m = model.to_lowercase();
    if m.is_empty() {
        return 0;
    }
    if backend.is_empty() || backend == "hermes" {
        let (cfg_model, cfg_window) = effective_window_from_config();
        let cm = cfg_model.to_lowercase();
        if cfg_window > 0 && !cm.is_empty() && (cm == m || m.contains(&cm)) {
            return cfg_window;
        }
    }
    // ② 跳过（见上）
    let mut best_key = "";
    let mut best_val = 0i64;
    for (k, v) in CTX_WINDOW_FALLBACK {
        if m.contains(k) && k.len() > best_key.len() {
            best_key = k;
            best_val = *v;
        }
    }
    if best_val > 0 {
        return best_val;
    }
    if backend.is_empty() || backend == "hermes" {
        let (_, bw) = effective_window_from_config();
        if bw > 0 {
            return bw;
        }
    }
    0
}

/// ---- Hermes 估算公式参数（对齐 Hermes 0.21.3 `agent/model_metadata.py`）----
/// ⚠️ 近似值（±15%）；阈值有缓冲。Hermes 升级若改公式，这里必须同步。
const CTX_CHARS_PER_TOKEN: usize = 4;

/// CJK 密集字符区间（与 Python 侧正则同一组区间）。
const CJK_DENSE_RANGES: &[(u32, u32)] = &[
    (0x1100, 0x11FF),
    (0x2E80, 0x9FFF),
    (0xA960, 0xA97F),
    (0xAC00, 0xD7AF),
    (0xF900, 0xFAFF),
    (0xFF00, 0xFFEF),
];

fn is_cjk_dense(c: char) -> bool {
    let cp = c as u32;
    CJK_DENSE_RANGES
        .iter()
        .any(|(lo, hi)| cp >= *lo && cp <= *hi)
}

/// 对齐 Hermes `estimate_tokens_rough`：CJK 密集字符 ≈1 token，其余 `ceil(UTF-8 字节 / 4)`。
pub fn estimate_tokens_rough(text: &str) -> i64 {
    if text.is_empty() {
        return 0;
    }
    if text.is_ascii() {
        return ((text.len() + 3) / CTX_CHARS_PER_TOKEN) as i64;
    }
    let mut dense: i64 = 0;
    let mut stripped = String::new();
    for c in text.chars() {
        if is_cjk_dense(c) {
            dense += 1;
        } else {
            stripped.push(c);
        }
    }
    dense + ((stripped.len() + 3) / CTX_CHARS_PER_TOKEN) as i64
}

// ---------------------------------------------------------------- 后端判定与会话头

fn cfg_str(agent_cfg: Option<&Value>, key: &str) -> String {
    agent_cfg
        .and_then(|c| c.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase()
}

/// 按后端特征判定 `openclaw` / `hermes` / `other`（分派水位读法用）。
///
/// 判定顺序：端口/URL 关键词 → 模型名关键词 → 会话档案兜底。
/// **不能只认端口**：各机 OpenClaw 网关端口并不统一，只认端口会把它们误判成 other，
/// 连带把上下文自愈整个关掉。
pub fn backend_kind(agent_cfg: Option<&Value>) -> String {
    let base = cfg_str(agent_cfg, "backend");
    let model = cfg_str(agent_cfg, "model");
    if base.contains("8642") || base.contains("hermes") {
        return "hermes".to_string();
    }
    if base.contains("18789") || base.contains("openclaw") || model.contains("openclaw") {
        return "openclaw".to_string();
    }
    if npf::openclaw_sessions_present() {
        return "openclaw".to_string();
    }
    "other".to_string()
}

/// 会话键 = `a2a:<caller>:<session_id>`（会话头与 state.db 查询用的是同一个键）。
pub fn session_key(caller: &str, session_id: Option<&str>) -> String {
    let caller = if caller.is_empty() { "unknown" } else { caller };
    HERMES_SESSION_KEY_FMT
        .replacen("%s", caller, 1)
        .replacen("%s", session_id.unwrap_or(""), 1)
}

/// Hermes OpenAI 兼容路由认的会话头。
///
/// - `X-Hermes-Session-Key` —— 会话/记忆域键（跨轮复用同一会话）
/// - `X-Hermes-Session-Id`  —— 显式接续/新建某个 transcript
///
/// `session_id` 为空时不注入任何头（一次性会话，与原行为一致）。
pub fn hermes_session_headers(caller: &str, session_id: Option<&str>) -> Vec<(String, String)> {
    match session_id {
        None | Some("") => Vec::new(),
        Some(sid) => {
            let key = session_key(caller, Some(sid));
            vec![
                ("X-Hermes-Session-Key".to_string(), key.clone()),
                ("X-Hermes-Session-Id".to_string(), key),
            ]
        }
    }
}

// ---------------------------------------------------------------- 读数：Hermes

/// 口径 B：只读 `state.db`，估算该会话「当前上下文占用」→ `(占用tokens, 窗口)`。
///
/// 取 `active = 1`（当前生效上下文，随压缩轮转会变，**切勿缓存**）。读不到返回 `None`。
fn read_hermes_ctx_b(caller: &str, session_id: &str, model: &str) -> Option<(i64, i64)> {
    if session_id.is_empty() {
        return None;
    }
    let got = match npf::hermes_active_messages(&session_key(caller, Some(session_id)), None) {
        Ok(v) => v,
        Err(exc) => {
            crate::logx::debug(
                "agent_usage",
                &format!("口径B 读 Hermes messages 失败({exc})，降级"),
            );
            return None;
        }
    };
    let (rows, row_model) = got?;
    let mut used: i64 = 0;
    for (content, tool_calls, reasoning) in rows {
        used += estimate_tokens_rough(content.as_deref().unwrap_or(""));
        used += estimate_tokens_rough(tool_calls.as_deref().unwrap_or(""));
        used += estimate_tokens_rough(reasoning.as_deref().unwrap_or(""));
    }
    let win = window_for(if model.is_empty() { &row_model } else { model }, "hermes");
    if win <= 0 || used <= 0 {
        return None;
    }
    Some((used, win))
}

/// 口径 A：`sessions` 表累计 token / 调用次数 = 单轮典型占用 → `(占用, 窗口)`。
fn read_hermes_usage_a(caller: &str, session_id: &str, model: &str) -> Option<(i64, i64)> {
    if session_id.is_empty() {
        return None;
    }
    let row = match npf::hermes_session_usage_row(&session_key(caller, Some(session_id)), None) {
        Ok(v) => v,
        Err(exc) => {
            crate::logx::debug(
                "agent_usage",
                &format!("口径A 读 Hermes state.db 失败({exc})，降级为不自愈"),
            );
            return None;
        }
    };
    let (inp, cache_read, calls, row_model) = row?;
    let (inp, cache_read, calls) = (
        inp.unwrap_or(0),
        cache_read.unwrap_or(0),
        calls.unwrap_or(0),
    );
    if calls <= 0 {
        return None;
    }
    let typical = (inp + cache_read) as f64 / calls as f64;
    let row_model = row_model.unwrap_or_default();
    let win = window_for(if model.is_empty() { &row_model } else { model }, "hermes");
    if win <= 0 {
        return None;
    }
    Some((typical as i64, win))
}

/// Hermes 水位总入口：口径 B 优先，拿不到回退口径 A。
pub fn read_hermes(caller: &str, session_id: &str, model: &str) -> Option<(i64, i64)> {
    read_hermes_ctx_b(caller, session_id, model)
        .or_else(|| read_hermes_usage_a(caller, session_id, model))
}

// ---------------------------------------------------------------- 读数：OpenClaw

/// 读 OpenClaw 会话档案，key = `agent:main:openai-user:a2a:<caller>:<session_id>`。
///
/// 本层只认 `totalTokens` / `contextTokens` 这两个**用量字段**的口径与降级。
pub fn read_openclaw(caller: &str, session_id: &str) -> Option<(i64, i64)> {
    if session_id.is_empty() {
        return None;
    }
    let entry =
        match npf::openclaw_session_entry(&npf::openclaw_session_key(caller, session_id), None) {
            Ok(v) => v,
            Err(exc) => {
                crate::logx::debug(
                    "agent_usage",
                    &format!("读 sessions.json 失败({exc})，降级为不自愈"),
                );
                return None;
            }
        };
    let entry = entry?;
    let total = as_i64(entry.get("totalTokens"))?;
    let window = as_i64(entry.get("contextTokens"))?;
    if window <= 0 {
        return None;
    }
    Some((total, window))
}

/// JSON 数值 → 整数（对应 Python `int(x)`：浮点向零截断）。
fn as_i64(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::Bool(b) => Some(if *b { 1 } else { 0 }),
        Value::String(s) => s.trim().parse::<f64>().ok().map(|f| f.trunc() as i64),
        _ => None,
    }
}

// ---------------------------------------------------------------- 读数：总入口

/// 按后端分派读水位，返回 `(占用, 窗口)` 或 `None`。
///
/// `agent_cfg` 为 `None` 时按 `openclaw` 试（与原默认一致）。
/// 返回 `None` 表示读不到（未配置/文件不存在/无权限/key 不在/字段缺失）——
/// 调用方应降级为「不压缩、照常续跑」，不阻塞主流程。本模块绝不写文件。
pub fn read_usage(
    caller: &str,
    session_id: &str,
    agent_cfg: Option<&Value>,
    model: &str,
) -> Option<(i64, i64)> {
    let kind = match agent_cfg {
        Some(cfg) => backend_kind(Some(cfg)),
        None => "openclaw".to_string(),
    };
    if kind == "hermes" {
        let m = if model.is_empty() {
            cfg_str(agent_cfg, "model")
        } else {
            model.to_string()
        };
        return read_hermes(caller, session_id, &m);
    }
    if kind == "other" {
        return None; // 未知后端：无水位数据源，降级不自愈
    }
    read_openclaw(caller, session_id)
}

/// 该 session 的上下文水位（0.0~1.0+）；读不到返回 `None`；低于下限返回 `0.0`。
pub fn pressure(
    caller: &str,
    session_id: &str,
    agent_cfg: Option<&Value>,
    min_tokens_floor: i64,
) -> Option<f64> {
    let (total, window) = read_usage(caller, session_id, agent_cfg, "")?;
    if total < min_tokens_floor {
        return Some(0.0);
    }
    Some(total as f64 / window as f64)
}

// ---------------------------------------------------------------- 渲染

/// 渲染成群里「📚 上下文」那行用的结构。
///
/// `pct` 用 Python 的 `round()`（**四舍六入五成双**），不是 Rust 的 `round()`（五away）。
pub fn fmt(used: i64, window: i64) -> Value {
    let pct = py_round(100.0 * used as f64 / window as f64) as i64;
    serde_json::json!({
        "used": used,
        "window": window,
        "pct": pct,
        "text": format!(
            "上下文：约 {}k/{}k ({}%)",
            used.div_euclid(1000),
            window.div_euclid(1000),
            pct
        ),
    })
}

/// Python `round()` 的对应物：**四舍六入五成双**（banker's rounding）。
fn py_round(x: f64) -> f64 {
    let floor = x.floor();
    let diff = x - floor;
    // 大于半 → 进位；正好半 → 进到偶数（floor 为奇才进位）；小于半 → 退位
    if diff > 0.5 || (diff == 0.5 && (floor as i64) % 2 != 0) {
        floor + 1.0
    } else {
        floor
    }
}

// ---------------------------------------------------------------- 远端回退的护栏
// 事故背景：对端不可达时 `timeout=5.0` 逐条累加，读接口整体挂起 →
// 三层护栏（只作用于**远端**回退；本机直读不受影响）：
//   ① 短超时：默认 1.5s（`CTX_A2A_TIMEOUT_S` 可覆盖，允许 0.2~30）；
//   ② 熔断器：一次网络失败后 20s 内直接返回 None；
//   ③ 结果缓存：命中 30s / 未命中 10s。
// 熔断只由**异常**触发：对端活着只是没数据（ok=false）不算故障。
const REMOTE_TIMEOUT_S: f64 = 1.5;
const REMOTE_BREAKER_S: f64 = 20.0;
const REMOTE_TTL_HIT_S: f64 = 30.0;
const REMOTE_TTL_MISS_S: f64 = 10.0;
const REMOTE_CACHE_MAX: usize = 512;

struct RemoteState {
    cache: HashMap<(String, String), (f64, Option<Value>)>,
    break_until: f64,
}

fn remote_state() -> &'static Mutex<RemoteState> {
    static STATE: OnceLock<Mutex<RemoteState>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(RemoteState {
            cache: HashMap::new(),
            break_until: 0.0,
        })
    })
}

fn now_secs() -> f64 {
    // 与 Python `time.time()` 同口径（自 epoch 起的秒）。用 `Instant` 无法与 env 配的
    // 绝对窗口比较，故取系统时钟。
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 远端回退的单次超时（秒）；配错值一律回落到默认，绝不放任长超时。
pub fn remote_timeout() -> Duration {
    let value = hacs_l0_host::node_paths::env_stripped("CTX_A2A_TIMEOUT_S")
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(REMOTE_TIMEOUT_S);
    let value = if (0.2..=30.0).contains(&value) {
        value
    } else {
        REMOTE_TIMEOUT_S
    };
    Duration::from_secs_f64(value)
}

/// 调本节点 a2a 服务的 `/ctx-usage/<agent>`（同机同源）拿真源水位。读不到返回 `None`。
///
/// 带护栏：短超时 + 失败熔断 + TTL 缓存。任何异常一律降级为 `None` ——
/// 读不到就不渲染，**绝不编造数字**，也绝不拖慢调用方。
pub fn read_remote(caller: &str, session_id: &str) -> Option<Value> {
    let base = a2a_remote_base();
    let token = a2a_remote_token();
    if base.is_empty() || token.is_empty() {
        return None;
    }
    let key = (caller.to_string(), session_id.to_string());
    let now = now_secs();
    {
        let state = remote_state().lock().unwrap();
        if let Some((age, val)) = state.cache.get(&key) {
            let ttl = if val.is_some() {
                REMOTE_TTL_HIT_S
            } else {
                REMOTE_TTL_MISS_S
            };
            if *age > 0.0 && now - *age < ttl {
                return val.clone();
            }
        }
        if now < state.break_until {
            return None; // 熔断窗口内：不对死对端砸网络，直接降级
        }
    }

    let url = format!(
        "{}/ctx-usage/{}?session_id={}",
        base.trim_end_matches('/'),
        http_client::quote(caller),
        http_client::quote(session_id)
    );
    let auth = format!("Bearer {token}");
    let mut result: Option<Value> = None;
    match http_client::get_json(&url, &[("Authorization", auth.as_str())], remote_timeout()) {
        Ok((status, data)) => {
            if status == 200 && data.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                let used = as_i64(data.get("used")).unwrap_or(0);
                let window = as_i64(data.get("window")).unwrap_or(0);
                if window > 0 && used >= 0 {
                    result = Some(fmt(used, window));
                }
            }
        }
        Err(exc) => {
            crate::logx::debug("agent_usage", &format!("远端 ctx-usage 失败({exc})"));
            let mut state = remote_state().lock().unwrap();
            state.break_until = now_secs() + REMOTE_BREAKER_S;
            result = None;
        }
    }

    let mut state = remote_state().lock().unwrap();
    if state.cache.len() >= REMOTE_CACHE_MAX {
        // 超上限先丢最老的一半（无界增长会把长跑进程拖成内存洞）
        let mut entries: Vec<((String, String), f64)> = state
            .cache
            .iter()
            .map(|(k, (ts, _))| (k.clone(), *ts))
            .collect();
        entries.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        for (k, _) in entries.into_iter().take(REMOTE_CACHE_MAX / 2) {
            state.cache.remove(&k);
        }
    }
    state.cache.insert(key, (now_secs(), result.clone()));
    drop(state);
    result
}

/// 读真源水位 → dict；读不到返回 `None`（不渲染，绝不编造数字）。
///
/// 两级：① 本机直读（本机就是该 agent 所在节点）；② 远端 a2a `/ctx-usage/<agent>`。
pub fn read_watermark(caller: &str, session_id: &str, agent_cfg: Option<&Value>) -> Option<Value> {
    if caller.is_empty() || session_id.is_empty() {
        return None;
    }
    if let Some((used, window)) = read_usage(caller, session_id, agent_cfg, "") {
        if window > 0 {
            return Some(fmt(used, window));
        }
    }
    read_remote(caller, session_id)
}
