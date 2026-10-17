//! `blackboard_svc` —— 团队黑板·记忆网关 HTTP 装配（对应 Python
//! `hub/l3_domain/blackboard_svc.py` 的 Handler + main，端口 9940）。
//!
//! 路由面（与 Python 逐条对齐）：
//! * GET  `/healthz`（不鉴权）；
//! * GET/POST `/msg`、GET/POST `/msg/<id>`、POST `/msg/<id>/ack|done`；
//! * GET/POST `/entries`；GET `/search`；
//! * GET `/sessions`、`/sessions/<id>`（圆桌会议，只读）；
//! * GET `/actions`、`/issues`、`/tracking`；
//! * GET/POST `/tmp`、GET/DELETE `/tmp/<id>`；
//! * GET/POST `/sess`、POST `/sess/<id>/close|turns`、GET `/sess/<id>`、`/sess/<id>/turns`；
//! * GET `/api/shared[/tasks|/search|/<id>]` 与 `/shared[/...]`（写请求 405）；
//! * 未登记：GET/POST 裸 404（不鉴权），其余方法 501。
//!
//! 鉴权：`Authorization: Bearer <token>` 的 sha256 反查 peers.json（恒时比较）。
#![allow(clippy::too_many_arguments)]

use hacs_l1_transport::web_gateway::{Gateway, Reply};
use hacs_l3_domain::blackboard_svc as bb;
use hacs_l3_domain::blackboard_svc::{ApiError, BbSvc};
use hacs_l1_transport::a2a_edge::{auth_peer, load_peers};
use serde_json::{json, Value};
use std::sync::Arc;

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut host, mut port, mut admin_port) = ("127.0.0.1".to_string(), bb::DEFAULT_PORT, 0u16);
    let mut agents_json: Option<String> = None;
    let mut peers_json: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--host" => host = args.next().unwrap_or(host),
            "--port" => port = args.next().and_then(|v| v.parse().ok()).unwrap_or(port),
            "--admin-port" => admin_port = args.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--agents-json" => agents_json = args.next(),
            "--peers-json" => peers_json = args.next(),
            _ => {}
        }
    }
    let runtime = Arc::new(bb::Runtime::new());
    if let Some(p) = &agents_json {
        match std::fs::read_to_string(p).map_err(|e| e.to_string()).and_then(|t| {
            let v: Value = serde_json::from_str(&t).map_err(|e| e.to_string())?;
            v.get("node_id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| "agents.json 顶层必须是对象".to_string())
        }) {
            Ok(nid) => *runtime.node_id.lock().unwrap() = nid,
            Err(e) => {
                eprintln!("加载 agents.json 失败: {}", bb::safe_err(&e));
                std::process::exit(2);
            }
        }
    }
    let peer_map = match &peers_json {
        None => vec![],
        Some(p) => match std::fs::read_to_string(p) {
            Ok(t) => match load_peers(&t) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("加载 peers.json 失败: {}", bb::safe_err(&e));
                    std::process::exit(2);
                }
            },
            Err(e) => {
                eprintln!("加载 peers.json 失败: {}", bb::safe_err(&e.to_string()));
                std::process::exit(2);
            }
        },
    };
    if peer_map.is_empty() {
        eprintln!("peers 表为空，所有业务请求都会 401");
    }

    // 口令只走环境变量；连不上不退出（healthz 503、业务口 503）。
    let dsn = std::env::var("BB_PG_DSN").unwrap_or_default();
    let store: Option<Arc<bb::BbStore>> = if dsn.is_empty() {
        eprintln!("环境变量 BB_PG_DSN 未设置，进入降级模式（业务口一律 503）");
        None
    } else {
        match bb::BbStore::connect(&dsn) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                eprintln!("数据库后端不可用，进入降级模式: {}", bb::safe_err(&e.0));
                None
            }
        }
    };
    let _ = PEER_MAP.set(peer_map.clone());
    let svc = Arc::new(BbSvc::new(
        store.unwrap_or_else(|| Arc::new(bb::BbStore::connect("host=127.0.0.1 port=1 dbname=none user=none connect_timeout=1").expect("占位存储"))),
        runtime.clone(),
        peer_map,
    ));
    let db_configured = !dsn.is_empty();

    // ---- 兜底 sweep 线程（tmp + sess + msg，300s 一轮，互不连坐）----------
    {
        let svc2 = svc.clone();
        std::thread::spawn(move || loop {
            // tmp
            match svc2.tmp_sweep_once() {
                Ok(n) => {
                    *svc2.runtime.tmp_swept_at.lock().unwrap() = Some(bb::iso_z(bb::now_micros()));
                    *svc2.runtime.tmp_sweep_error.lock().unwrap() = None;
                    svc2.runtime.tmp_sweeps.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n > 0 {
                        println!("tmp 兜底 sweep 销毁 {} 条", n);
                    }
                }
                Err(e) => {
                    *svc2.runtime.tmp_sweep_error.lock().unwrap() = Some(bb::safe_err(&e.0));
                    println!("tmp 兜底 sweep 失败: {}", bb::safe_err(&e.0));
                }
            }
            // sess
            match svc2.sess_sweep_once() {
                Ok(n) => {
                    *svc2.runtime.sess_swept_at.lock().unwrap() = Some(bb::iso_z(bb::now_micros()));
                    *svc2.runtime.sess_sweep_error.lock().unwrap() = None;
                    svc2.runtime.sess_sweeps.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    svc2.runtime.sess_archived.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                    if n > 0 {
                        println!("session 兜底 sweep 归档 {} 场", n);
                    }
                }
                Err(e) => {
                    *svc2.runtime.sess_sweep_error.lock().unwrap() = Some(bb::safe_err(&e.0));
                    println!("session 兜底 sweep 失败: {}", bb::safe_err(&e.0));
                }
            }
            // msg
            match svc2.msg_sweep_once() {
                Ok(_n) => {
                    *svc2.runtime.msg_swept_at.lock().unwrap() = Some(bb::iso_z(bb::now_micros()));
                    *svc2.runtime.msg_sweep_error.lock().unwrap() = None;
                    svc2.runtime.msg_sweeps.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(e) => {
                    if bb::is_missing_schema(&e.0) {
                        // 表还没建：静默跳过
                    } else {
                        *svc2.runtime.msg_sweep_error.lock().unwrap() = Some(bb::safe_err(&e.0));
                        println!("msg 兜底 sweep 线程异常: {}", bb::safe_err(&e.0));
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(bb::TMP_SWEEP_INTERVAL_S));
        });
    }
    eprintln!(
        "blackboard_svc {} node={} listening on {}:{} db={}",
        bb::VERSION,
        runtime.node_id(),
        host,
        port,
        if db_configured { "on" } else { "degraded" }
    );

    let mut gw = Gateway::new("blackboard_svc", Some(bb::VERSION), true);
    gw.protocol_version = "HTTP/1.1".to_string();

    macro_rules! peer_or {
        ($req:expr) => {
            match auth_peer($req.header("Authorization"), peer_map_global(), false) {
                Some(p) => p,
                None => return Ok(Reply::json(401, json!({"ok": false, "error": "unauthorized"}))),
            }
        };
    }
    macro_rules! db_or_503 {
        ($res:expr, $peer:expr) => {
            match $res {
                Ok(v) => v,
                Err(e) => {
                    return Ok(Reply::json(
                        503,
                        json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}),
                    ))
                }
            }
        };
    }






    fn int_field(value: Option<&Value>, field: &str) -> Result<Option<i64>, ApiError> {
        match value {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(_)) => Err(ApiError::new(400, "bad_request").detail(format!("{} 必须是整数", field))),
            Some(Value::Number(n)) if n.is_i64() => Ok(Some(n.as_i64().unwrap())),
            Some(_) => Err(ApiError::new(400, "bad_request").detail(format!("{} 必须是整数", field))),
        }
    }
    fn is_shared_path(parts: &[&str]) -> bool {
        if parts.len() >= 2 && parts[0] == "api" && parts[1] == "shared" {
            return true;
        }
        !parts.is_empty() && parts[0] == "shared"
    }

    // ---- GET /healthz（不鉴权）-------------------------------------------
    {
        let rt_h = svc.clone();
        gw.route("/healthz", &["GET"], false, move |_req| {
            let (code, payload) = bb::health_payload(&rt_h);
            Ok(Reply::json(code, payload))
        });
    }

    // ---- GET /msg（收件箱）与 /msg/<id> -----------------------------------
    {
        let rt_h = svc.clone();
        gw.route("/msg", &["GET"], false, move |req| {
            let peer = peer_or!(req);
            let to = {
                let t = one(&req.query, "to");
                if t.is_empty() { peer.clone() } else { t }
            };
            if to != peer {
                return Ok(Reply::json(403, json!({"ok": false, "error": "forbidden", "detail": "只能查自己的收件箱"})));
            }
            let raw_states = {
                let s = one(&req.query, "state");
                if s.is_empty() { bb::MSG_OPEN_STATES.join(",") } else { s }
            };
            let states: Vec<String> = raw_states.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
            let bad: Vec<&str> = states.iter().map(|s| s.as_str()).filter(|s| !bb::MSG_STATES.contains(s)).collect();
            if !bad.is_empty() {
                return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": format!("未知 state: {}", bad.join(","))})));
            }
            let limit: i64 = match one(&req.query, "limit").parse::<i64>() {
                Ok(v) => v.clamp(1, bb::MSG_LIST_MAX_LIMIT),
                Err(_) if one(&req.query, "limit").is_empty() => bb::MSG_LIST_DEFAULT_LIMIT,
                Err(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "limit 必须是整数"}))),
            };
            let items = db_or_503!(rt_h.list_inbox(&peer, &states, limit), peer);
            Ok(Reply::json(200, json!({"ok": true, "count": items.len(), "to": peer, "states": states, "messages": items})))
        });
    }
    {
        let rt_h = svc.clone();
        gw.route("/msg", &["GET"], true, move |req| {
            let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
            if parts.len() != 2 {
                return Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})));
            }
            let peer = peer_or!(req);
            let raw_id = &parts[1];
            if !bb::msg_id_ok(raw_id) {
                return Ok(Reply::json(404, json!({"ok": false, "error": "not_found", "detail": "msg_id 形状不对"})));
            }
            let msg = db_or_503!(rt_h.get_msg(raw_id), peer);
            let Some(msg) = msg else {
                return Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})));
            };
            if peer != msg.get("from").and_then(Value::as_str).unwrap_or("")
                && peer != msg.get("to").and_then(Value::as_str).unwrap_or("")
            {
                return Ok(Reply::json(403, json!({"ok": false, "error": "forbidden", "detail": "只允许发件人或收件人查看"})));
            }
            Ok(Reply::json(200, json!({"ok": true, "msg": msg})))
        });
    }
    // POST /msg 落库；POST /msg/<id>/ack|done
    {
        let rt_h = svc.clone();
        gw.route("/msg", &["POST"], false, move |req| {
            let peer = peer_or!(req);
            let body = req.body.clone();
            let v: Value = match serde_json::from_slice::<Value>(&body) {
                Ok(v) if v.is_object() => v,
                _ => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "body 不是合法 JSON"}))),
            };
            // 校验载荷（validate_msg_payload）
            let to = match v.get("to") {
                Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
                _ => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "to 必填（收件成员名）"}))),
            };
            if to.chars().count() > bb::MSG_MEMBER_MAX {
                return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": format!("to 超长（上限 {}）", bb::MSG_MEMBER_MAX)})));
            }
            let text = v.get("text").or(v.get("body"));
            let ref_v = v.get("content_ref");
            if text.is_none() && ref_v.is_none() {
                return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "text 与 content_ref 至少给一个（长文本请走 content_ref 临时共享区）"})));
            }
            let text_s: Option<String> = match text {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "text 必须是字符串"}))),
            };
            if let Some(t) = &text_s {
                if t.len() > bb::MSG_BODY_MAX {
                    return Ok(Reply::json(413, json!({"ok": false, "error": "payload_too_large", "detail": format!("text 超过 {} 字节（长文本请走 content_ref 临时共享区）", bb::MSG_BODY_MAX)})));
                }
            }
            if ref_v.is_some() && !ref_v.unwrap().is_object() {
                return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "content_ref 必须是对象"})));
            }
            let topic = match v.get("topic") {
                None | Some(Value::Null) => bb::MSG_DEFAULT_TOPIC.to_string(),
                Some(Value::String(s)) if !s.trim().is_empty() && s.chars().count() <= bb::MSG_TOPIC_MAX => s.trim().to_string(),
                _ => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": format!("topic 非法（非空字符串，长度上限 {}）", bb::MSG_TOPIC_MAX)}))),
            };
            let session_id = match v.get("session_id") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "session_id 必须是字符串"}))),
            };
            let ttl = match int_field(v.get("ttl_s"), "ttl_s") {
                Ok(None) => bb::MSG_DEFAULT_TTL_S,
                Ok(Some(t)) if (bb::MSG_MIN_TTL_S..=bb::MSG_MAX_TTL_S).contains(&t) => t,
                Ok(Some(_)) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": format!("ttl_s 必须在 {}..{} 之间", bb::MSG_MIN_TTL_S, bb::MSG_MAX_TTL_S)}))),
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let mid = match v.get("msg_id") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if bb::msg_id_ok(s) => Some(s.clone()),
                Some(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "msg_id 形状不合法（应为 m-XXXXXXXX）"}))),
            };
            let result = db_or_503!(rt_h.insert_msg(&peer, &to, text_s.as_deref(), ref_v.filter(|r| r.is_object()), session_id.as_deref(), &topic, ttl, mid.as_deref()), peer);
            let mut out = json!({"ok": true, "delivered": true});
            if let (Value::Object(m), Value::Object(r)) = (&mut out, &result) {
                for (k, val) in r {
                    m.insert(k.clone(), val.clone());
                }
            }
            Ok(Reply::json(200, out))
        });
    }
    {
        let rt_h = svc.clone();
        gw.route("/msg", &["POST"], true, move |req| {
            let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
            if parts.len() != 3 || (parts[2] != "ack" && parts[2] != "done") {
                return Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})));
            }
            let peer = peer_or!(req);
            let raw_id = parts[1].to_string();
            if !bb::msg_id_ok(&raw_id) {
                return Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})));
            }
            if parts[2] == "ack" {
                let current = db_or_503!(rt_h.get_msg(&raw_id), peer);
                let Some(current) = current else {
                    return Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})));
                };
                if current.get("to").and_then(Value::as_str) != Some(peer.as_str()) {
                    return Ok(Reply::json(403, json!({"ok": false, "error": "forbidden", "detail": "只有收件人可以 ack"})));
                }
                match rt_h.msg_ack(&raw_id) {
                    Err(e) => Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}))),
                    Ok(None) => Ok(Reply::json(200, json!({"ok": true, "acked": current.get("acked"), "state": current.get("state"), "repeat": true, "msg": current}))),
                    Ok(Some(msg)) => Ok(Reply::json(200, json!({"ok": true, "acked": true, "repeat": false, "msg": msg}))),
                }
            } else {
                // done
                let body = req.body.clone();
                let mut obj = Value::Null;
                if !body.is_empty() {
                    obj = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
                }
                if !obj.is_null() && !obj.is_object() {
                    obj = Value::Null;
                }
                let ok_flag = match obj.get("ok") {
                    None | Some(Value::Null) => true,
                    Some(Value::Bool(b)) => *b,
                    Some(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "ok 必须是布尔"}))),
                };
                let err_text: Option<String> = match obj.get("error") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(s)) => Some(s.chars().take(1000).collect()),
                    Some(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "error 必须是字符串"}))),
                };
                let to_state = if ok_flag { "done" } else { "failed" };
                let current = db_or_503!(rt_h.get_msg(&raw_id), peer);
                let Some(current) = current else {
                    return Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})));
                };
                if current.get("to").and_then(Value::as_str) != Some(peer.as_str()) {
                    return Ok(Reply::json(403, json!({"ok": false, "error": "forbidden", "detail": "只有收件人可以报完成"})));
                }
                match rt_h.msg_finish(&raw_id, to_state, err_text.as_deref()) {
                    Err(e) => Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}))),
                    Ok(None) => Ok(Reply::json(200, json!({"ok": true, "state": current.get("state"), "repeat": true, "msg": current}))),
                    Ok(Some(msg)) => Ok(Reply::json(200, json!({"ok": true, "state": msg.get("state"), "repeat": false, "msg": msg}))),
                }
            }
        });
    }

    // ---- GET/POST /entries -----------------------------------------------
    {
        let rt_h = svc.clone();
        gw.route("/entries", &["GET"], false, move |req| {
            let _peer = peer_or!(req);
            let limit = match clamp_limit(&one(&req.query, "limit"), bb::DEFAULT_LIMIT, bb::MAX_LIMIT) {
                Ok(v) => v,
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let since = match parse_since(&one(&req.query, "since")) {
                Ok(v) => v,
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let st = one(&req.query, "status");
            let entries = db_or_503!(rt_h.select_entries(
                non_empty(&one(&req.query, "topic")).as_deref(),
                non_empty(&one(&req.query, "kind")).as_deref(),
                non_empty(&one(&req.query, "author")).as_deref(),
                limit,
                since.as_deref(),
                non_empty(&st).as_deref(),
            ), peer);
            Ok(Reply::json(200, json!({"ok": true, "entries": entries, "count": entries.len()})))
        });
    }
    {
        let rt_h = svc.clone();
        gw.route("/entries", &["POST"], false, move |req| {
            let peer = peer_or!(req);
            let body = req.body.clone();
            if body.len() > bb::MAX_BODY_BYTES {
                return Ok(Reply::json(413, json!({"ok": false, "error": "payload_too_large", "detail": format!("请求体上限 {} 字节", bb::MAX_BODY_BYTES)})));
            }
            let v: Value = match serde_json::from_slice::<Value>(&body) {
                Ok(v) if v.is_object() => v,
                Ok(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "body 必须是 JSON 对象"}))),
                Err(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "body 不是合法 JSON"}))),
            };
            let topic = match norm_text(v.get("topic"), "topic", true, "") {
                Ok(t) => t,
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let kind = match norm_text(v.get("kind"), "kind", false, "note") {
                Ok(k) => k,
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let content = match v.get("content") {
                Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
                _ => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "content 不能为空"}))),
            };
            let refs = match v.get("refs") {
                None | Some(Value::Null) => json!([]),
                Some(r @ Value::Array(_)) => r.clone(),
                Some(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "refs 必须是数组"}))),
            };
            let mut embedding_lit: Option<String> = None;
            match v.get("embedding") {
                None | Some(Value::Null) => {}
                Some(e @ Value::Array(_)) => {
                    let arr = e.as_array().unwrap();
                    if arr.len() != bb::EMBEDDING_DIM {
                        return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": format!("embedding 维度必须是 {}", bb::EMBEDDING_DIM)})));
                    }
                    let mut nums = Vec::with_capacity(arr.len());
                    for x in arr {
                        match x {
                            Value::Number(n) => nums.push(n.as_f64().unwrap_or(0.0)),
                            _ => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "embedding 只能包含数字"}))),
                        }
                    }
                    embedding_lit = bb::vector_literal(Some(&nums));
                }
                Some(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "embedding 必须是数组"}))),
            }
            if v.get("author").and_then(Value::as_str).map(|a| a != peer).unwrap_or(false) {
                println!("忽略请求体里的 author 字段 from={}", peer);
            }
            // 嵌入补算在 Rust 版暂不可用（可选增强）：留 NULL，照常 200。
            let result = db_or_503!(rt_h.insert_entry(&peer, &topic, &kind, &content, &refs, embedding_lit), peer);
            Ok(Reply::json(200, json!({"ok": true, "entry_id": result.get("entry_id"), "ts": result.get("ts")})))
        });
    }

    // ---- GET /search ------------------------------------------------------
    {
        let rt_h = svc.clone();
        gw.route("/search", &["GET"], false, move |req| {
            let _peer = peer_or!(req);
            let q = one(&req.query, "q");
            if q.is_empty() {
                return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "q 不能为空"})));
            }
            if q.chars().count() > bb::SEARCH_QUERY_MAX {
                return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": format!("q 超过 {} 字符", bb::SEARCH_QUERY_MAX)})));
            }
            let limit_raw = one(&req.query, "limit");
            let mut limit = bb::SEARCH_DEFAULT_LIMIT;
            if !limit_raw.is_empty() {
                match limit_raw.parse::<i64>() {
                    Err(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "limit 必须是整数"}))),
                    Ok(v) => {
                        limit = if v <= 0 { bb::SEARCH_DEFAULT_LIMIT } else { v };
                        limit = limit.min(bb::SEARCH_MAX_LIMIT);
                    }
                }
            }
            let topic = non_empty(&one(&req.query, "topic"));
            // 嵌入不可用 → 一律关键词模式（降级语义与 Python 一致，mode 如实报 keyword）
            let mode = "keyword";
            let entries = db_or_503!(rt_h.search_keyword(&q, topic.as_deref(), limit), peer);
            Ok(Reply::json(200, json!({"ok": true, "mode": mode, "query": q, "count": entries.len(), "entries": entries})))
        });
    }

    // ---- GET /sessions 与 /sessions/<id>（圆桌会议，只读）------------------
    {
        let rt_h = svc.clone();
        gw.route("/sessions", &["GET"], false, move |req| {
            let _peer = peer_or!(req);
            let limit = match clamp_limit(&one(&req.query, "limit"), bb::SESSIONS_DEFAULT_LIMIT, bb::SESSIONS_MAX_LIMIT) {
                Ok(v) => v,
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let since = match parse_since(&one(&req.query, "since")) {
                Ok(v) => v,
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let sessions = db_or_503!(rt_h.select_agent_sessions(
                non_empty(&one(&req.query, "state")).as_deref(),
                non_empty(&one(&req.query, "topic")).as_deref(),
                limit,
                since.as_deref(),
            ), peer);
            Ok(Reply::json(200, json!({"ok": true, "count": sessions.len(), "sessions": sessions})))
        });
    }
    {
        let rt_h = svc.clone();
        gw.route("/sessions", &["GET"], true, move |req| {
            let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
            if parts.len() != 2 {
                return Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})));
            }
            let _peer = peer_or!(req);
            let session_id = urldecode(parts[1]);
            if session_id.is_empty() || session_id.len() > bb::SESSION_ID_MAX {
                return Ok(Reply::json(404, json!({"ok": false, "error": "not_found", "detail": "没有这场会议"})));
            }
            let _limit = match clamp_limit(&one(&req.query, "limit"), bb::TURNS_DEFAULT_LIMIT, bb::SESSIONS_MAX_LIMIT) {
                Ok(v) => v,
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let degraded: Vec<String> = vec![];
            let now_iso = bb::iso_utc_now();
            let detail = db_or_503!(rt_h.select_agent_session(&session_id), peer);
            let Some(mut payload) = detail else {
                return Ok(Reply::json(404, json!({"ok": false, "error": "not_found", "detail": "没有这场会议"})));
            };
            let obj = payload.as_object_mut().unwrap();
            obj.insert("ok".into(), Value::Bool(true));
            obj.insert("degraded".into(), json!(degraded));
            obj.insert("now".into(), json!(now_iso));
            obj.insert("tz".into(), json!("UTC"));
            Ok(Reply::json(200, payload))
        });
    }

    // ---- GET /actions | /issues | /tracking --------------------------------
    {
        let rt_h = svc.clone();
        gw.route("/actions", &["GET"], false, move |req| tracking_route(&rt_h, req, "actions"))
    }
    {
        let rt_h = svc.clone();
        gw.route("/issues", &["GET"], false, move |req| tracking_route(&rt_h, req, "issues"))
    }
    {
        let rt_h = svc.clone();
        gw.route("/tracking", &["GET"], false, move |req| tracking_route(&rt_h, req, "tracking"))
    }

    // ---- tmp box -----------------------------------------------------------
    {
        let rt_h = svc.clone();
        gw.route("/tmp", &["GET"], false, move |req| {
            let peer = peer_or!(req);
            let mine = match parse_flag(&one(&req.query, "mine"), "mine") {
                Ok(v) => v,
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let inbox = match parse_flag(&one(&req.query, "inbox"), "inbox") {
                Ok(v) => v,
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let limit = match clamp_limit(&one(&req.query, "limit"), bb::TMP_LIST_DEFAULT_LIMIT, bb::TMP_LIST_MAX_LIMIT) {
                Ok(v) => v,
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let (mode, items) = db_or_503!(rt_h.tmp_list(&peer, mine, inbox, limit), peer);
            Ok(Reply::json(200, json!({"ok": true, "mode": mode, "count": items.len(), "items": items, "now": bb::iso_utc_now(), "tz": "UTC"})))
        });
    }
    {
        let rt_h = svc.clone();
        gw.route("/tmp", &["GET"], true, move |req| {
            let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
            if parts.len() != 2 {
                return Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})));
            }
            let peer = peer_or!(req);
            tmp_read(&rt_h, &peer, parts[1])
        });
    }
    {
        let rt_h = svc.clone();
        gw.route("/tmp", &["POST"], false, move |req| {
            let peer = peer_or!(req);
            let body = req.body.clone();
            if body.len() > bb::MAX_BODY_BYTES {
                return Ok(Reply::json(413, json!({"ok": false, "error": "payload_too_large", "detail": format!("请求体上限 {} 字节", bb::MAX_BODY_BYTES)})));
            }
            let v: Value = match serde_json::from_slice::<Value>(&body) {
                Ok(v) if v.is_object() => v,
                Ok(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "body 必须是 JSON 对象"}))),
                Err(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "body 不是合法 JSON"}))),
            };
            let content = match v.get("body") {
                Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
                _ => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "body 不能为空"}))),
            };
            if content.len() > bb::MAX_BODY_BYTES {
                return Ok(Reply::json(413, json!({"ok": false, "error": "payload_too_large", "detail": format!("body 有 {} 字节，超过 {}", content.len(), bb::MAX_BODY_BYTES)})));
            }
            // to → audience 去重保序
            let mut audience: Vec<String> = vec![];
            match v.get("to") {
                None | Some(Value::Null) => {}
                Some(Value::Array(a)) => {
                    for x in a {
                        match x.as_str() {
                            Some(name) => {
                                let name = name.trim().to_string();
                                if !name.is_empty() && !audience.contains(&name) {
                                    audience.push(name);
                                }
                            }
                            None => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "to 必须是成员名数组"}))),
                        }
                    }
                }
                Some(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "to 必须是成员名数组"}))),
            }
            if audience.len() > 100 {
                return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "to 最多 100 个成员"})));
            }
            let known = rt_h.known_members();
            let unknown: Vec<&String> = audience.iter().filter(|n| !known.contains(n)).collect();
            if !unknown.is_empty() {
                let names: Vec<&str> = unknown.iter().map(|s| s.as_str()).collect();
                return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": format!("to 里有未知成员名: {}", names.join(","))})));
            }
            let reads_default = (audience.len() as i64).max(1);
            let reads = match int_field(v.get("reads"), "reads") {
                Ok(None) => reads_default,
                Ok(Some(r)) if (bb::TMP_READS_MIN..=bb::TMP_READS_MAX).contains(&r) => r,
                Ok(Some(_)) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": format!("reads 必须在 {}..{}", bb::TMP_READS_MIN, bb::TMP_READS_MAX)}))),
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let ttl = match int_field(v.get("ttl_s"), "ttl_s") {
                Ok(None) => bb::TMP_DEFAULT_TTL_S,
                Ok(Some(t)) if (bb::TMP_MIN_TTL_S..=bb::TMP_MAX_TTL_S).contains(&t) => t,
                Ok(Some(_)) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": format!("ttl_s 必须在 {}..{}", bb::TMP_MIN_TTL_S, bb::TMP_MAX_TTL_S)}))),
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let title = match v.get("title") {
                None | Some(Value::Null) => None,
                Some(t) => match norm_text(Some(t), "title", false, "") {
                    Ok(s) if s.is_empty() => None,
                    Ok(s) => Some(s),
                    Err(e) => return Ok(Reply::json(e.status, e.body())),
                },
            };
            match rt_h.tmp_owner_active_count(&peer) {
                Err(e) => return Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}))),
                Ok(active) if active >= bb::TMP_OWNER_ACTIVE_MAX => {
                    return Ok(Reply::json(429, json!({"ok": false, "error": "quota_exceeded", "detail": format!("活跃临时共享已达 {} 条（先等旧的销毁或过期）", bb::TMP_OWNER_ACTIVE_MAX)})));
                }
                Ok(_) => {}
            }
            let result = db_or_503!(rt_h.insert_tmp(&peer, &audience, title.as_deref(), &content, reads, ttl), peer);
            let _ = rt_h.tmp_enforce_capacity();
            Ok(Reply::json(200, json!({
                "ok": true,
                "tmp_id": result.get("tmp_id"),
                "expires_at": result.get("expires_at"),
                "reads_max": result.get("reads_max"),
                "bytes": result.get("bytes"),
            })))
        });
    }
    {
        let rt_h = svc.clone();
        gw.route("/tmp", &["DELETE"], true, move |req| {
            let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
            if parts.len() != 2 {
                return Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})));
            }
            let peer = peer_or!(req);
            tmp_delete(&rt_h, &peer, parts[1])
        });
    }

    // ---- /sess（Agent 会话）-------------------------------------------------
    {
        let rt_h = svc.clone();
        gw.route("/sess", &["GET"], false, move |req| {
            let _peer = peer_or!(req);
            let limit = match clamp_limit(&one(&req.query, "limit"), bb::SESS_LIST_DEFAULT_LIMIT, bb::SESS_LIST_MAX_LIMIT) {
                Ok(v) => v,
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let state = match parse_status(&one(&req.query, "state"), &bb::SESS_STATES, "state") {
                Ok(v) => v,
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let topic = one(&req.query, "topic");
            if topic.chars().count() > bb::SESS_TOPIC_MAX {
                return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": format!("topic 超过 {} 字符", bb::SESS_TOPIC_MAX)})));
            }
            let since = match parse_since(&one(&req.query, "since")) {
                Ok(v) => v,
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let sessions = db_or_503!(rt_h.select_agent_sessions(state.as_deref(), non_empty(&topic).as_deref(), limit, since.as_deref()), peer);
            Ok(Reply::json(200, json!({"ok": true, "count": sessions.len(), "sessions": sessions, "now": bb::iso_z(bb::now_micros()), "tz": "UTC"})))
        });
    }
    {
        let rt_h = svc.clone();
        gw.route("/sess", &["GET"], true, move |req| {
            let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
            if parts.len() > 3 {
                return Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})));
            }
            let peer = peer_or!(req);
            if parts.len() == 2 {
                return sess_detail(&rt_h, &peer, parts[1], &req.query, bb::SESS_DETAIL_TURNS_DEFAULT, true);
            }
            if parts[2] != "turns" {
                return Ok(Reply::json(404, json!({"ok": false, "error": "not_found", "detail": "只有 /sess/<id>/turns 这一个子路径"})));
            }
            sess_detail(&rt_h, &peer, parts[1], &req.query, bb::SESS_TURNS_DEFAULT_LIMIT, false)
        });
    }
    {
        let rt_h = svc.clone();
        gw.route("/sess", &["POST"], false, move |req| {
            let peer = peer_or!(req);
            let body = req.body.clone();
            if body.len() > bb::MAX_BODY_BYTES {
                return Ok(Reply::json(413, json!({"ok": false, "error": "payload_too_large", "detail": format!("请求体上限 {} 字节", bb::MAX_BODY_BYTES)})));
            }
            let v: Value = match serde_json::from_slice::<Value>(&body) {
                Ok(v) if v.is_object() => v,
                Ok(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "body 必须是 JSON 对象"}))),
                Err(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "body 不是合法 JSON"}))),
            };
            let topic = match norm_text(v.get("topic"), "topic", false, bb::SESS_DEFAULT_TOPIC) {
                Ok(t) if t.chars().count() <= bb::SESS_TOPIC_MAX => t,
                Ok(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": format!("topic 超过 {} 字符", bb::SESS_TOPIC_MAX)}))),
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let title = match norm_text(v.get("title"), "title", false, "") {
                Ok(t) if t.is_empty() => None,
                Ok(t) => Some(t),
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            let mut members: Vec<String> = vec![];
            match v.get("members") {
                None | Some(Value::Null) => {}
                Some(Value::Array(a)) => {
                    for x in a {
                        match x.as_str() {
                            Some(name) => {
                                let name = name.trim().to_string();
                                if name.is_empty() {
                                    continue;
                                }
                                if name.len() > bb::SESS_MEMBER_NAME_MAX {
                                    return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": format!("members 里的成员名超过 {} 字符", bb::SESS_MEMBER_NAME_MAX)})));
                                }
                                if !members.contains(&name) {
                                    members.push(name);
                                }
                            }
                            None => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "members 必须是成员名数组"}))),
                        }
                    }
                }
                Some(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "members 必须是成员名数组"}))),
            }
            if members.len() > bb::SESS_MEMBERS_MAX {
                return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": format!("members 最多 {} 个成员", bb::SESS_MEMBERS_MAX)})));
            }
            let ttl = match int_field(v.get("idle_ttl_s"), "idle_ttl_s") {
                Ok(None) => bb::SESS_DEFAULT_IDLE_TTL_S,
                Ok(Some(t)) if (bb::SESS_MIN_IDLE_TTL_S..=bb::SESS_MAX_IDLE_TTL_S).contains(&t) => t,
                Ok(Some(_)) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": format!("idle_ttl_s 必须在 {}..{}", bb::SESS_MIN_IDLE_TTL_S, bb::SESS_MAX_IDLE_TTL_S)}))),
                Err(e) => return Ok(Reply::json(e.status, e.body())),
            };
            if v.get("owner").and_then(Value::as_str).map(|o| o != peer).unwrap_or(false) {
                println!("忽略请求体里的 owner 字段 from={}", peer);
            }
            let session = db_or_503!(rt_h.insert_agent_session(&peer, &topic, title.as_deref(), &members, ttl), peer);
            Ok(Reply::json(200, json!({
                "ok": true,
                "session_id": session.get("session_id"),
                "topic": session.get("topic"),
                "state": session.get("state"),
                "title": session.get("title"),
                "owner": session.get("owner"),
                "members": session.get("members"),
                "idle_ttl_s": session.get("idle_ttl_s"),
                "turn_count": session.get("turn_count"),
                "created_at": session.get("created_at"),
            })))
        });
    }
    {
        let rt_h = svc.clone();
        gw.route("/sess", &["POST"], true, move |req| {
            let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
            if parts.len() != 3 || (parts[2] != "close" && parts[2] != "turns") {
                return Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})));
            }
            let peer = peer_or!(req);
            let session_id = urldecode(parts[1]);
            if !bb::session_id_ok(&session_id) {
                return Ok(Reply::json(404, json!({"ok": false, "error": "not_found", "detail": "没有这个会话"})));
            }
            if parts[2] == "turns" {
                return sess_turn_write(&rt_h, &peer, &session_id, req);
            }
            // close
            let body = req.body.clone();
            if body.len() > bb::MAX_BODY_BYTES {
                return Ok(Reply::json(413, json!({"ok": false, "error": "payload_too_large", "detail": format!("请求体上限 {} 字节", bb::MAX_BODY_BYTES)})));
            }
            let note: Option<String> = if body.is_empty() {
                None
            } else {
                let v: Value = match serde_json::from_slice::<Value>(&body) {
                    Ok(v) if v.is_object() => v,
                    Ok(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "body 必须是 JSON 对象"}))),
                    Err(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "body 不是合法 JSON"}))),
                };
                match norm_text(v.get("note"), "note", false, "") {
                    Ok(n) if n.is_empty() => None,
                    Ok(n) => Some(n),
                    Err(e) => return Ok(Reply::json(e.status, e.body())),
                }
            };
            let session = db_or_503!(rt_h.select_agent_session(&session_id), peer);
            let Some(session) = session else {
                return Ok(Reply::json(404, json!({"ok": false, "error": "not_found", "detail": "没有这个会话"})));
            };
            if !bb::sess_visible_to(&session, &peer) {
                return Ok(Reply::json(403, json!({"ok": false, "error": "forbidden", "detail": "这个会话没有邀请你"})));
            }
            if session.get("state").and_then(Value::as_str) != Some("open") {
                return Ok(Reply::json(200, sess_close_payload(&session, true)));
            }
            match rt_h.close_agent_session(&session_id, bb::SESS_CLOSE_REASON_MANUAL, note.as_deref()) {
                Err(e) => Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}))),
                Ok((claimed, fresh, _archive, turns_cleared)) => {
                    if !claimed {
                        let fresh = db_or_503!(rt_h.select_agent_session(&session_id), peer)
                            .unwrap_or(session);
                        return Ok(Reply::json(200, sess_close_payload(&fresh, true)));
                    }
                    let mut payload = sess_close_payload(fresh.as_ref().unwrap(), false);
                    payload.as_object_mut().unwrap().insert("turns_cleared".into(), json!(turns_cleared));
                    Ok(Reply::json(200, payload))
                }
            }
        });
    }

    // ---- /api/shared、/shared（只读）----------------------------------------
    {
        let rt_h = svc.clone();
        gw.route("/api/shared", &["GET"], true, move |req| shared_route(&rt_h, req))
    }
    {
        let rt_h = svc.clone();
        gw.route("/shared", &["GET"], true, move |req| shared_route(&rt_h, req))
    }

    // ---- 未登记路径：GET/POST 裸 404 ----------------------------------------
    gw.not_found(move |req| {
        if req.method == "GET" || req.method == "POST" {
            Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})))
        } else {
            Ok(Reply::bytes(
                501,
                "text/html; charset=utf-8",
                format!(
                    "<!DOCTYPE HTML><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Error response</title></head><body><h1>Error response</h1><p>Error code: 501</p><p>Message: Unsupported method ({}).</p></body></html>",
                    req.method
                )
                .into_bytes(),
            ))
        }
    });

    // 共享路径写方法 405（pre 钩子拦：POST/PUT/PATCH/DELETE 打到 /api/shared|/shared）
    let svc_pre = svc.clone();
    gw.pre(move |req| {
        if req.method == "GET" || req.method == "HEAD" {
            return Ok(None);
        }
        let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
        if is_shared_path(&parts) {
            return Ok(Some(Reply::bytes(
                405,
                "application/json; charset=utf-8",
                serde_json::to_vec(&json!({"ok": false, "error": "method_not_allowed", "detail": "共享内容接口只读；发布与修改请用 shared_content.py"})).unwrap(),
            )));
        }
        let _ = &svc_pre;
        Ok(None)
    });

    // admin 健康口（只绑 127.0.0.1）
    if admin_port > 0 {
        let mut agw = Gateway::new("blackboard_admin", Some(bb::VERSION), true);
        agw.protocol_version = "HTTP/1.1".to_string();
        let rt_h = svc.clone();
        agw.route("/healthz", &["GET"], false, move |_req| {
            let (code, payload) = bb::health_payload(&rt_h);
            Ok(Reply::json(code, payload))
        });
        agw.not_found(|_req| {
            Ok(Reply::bytes(404, "application/json; charset=utf-8", b"{\"ok\":false,\"error\":\"not_found\"}".to_vec()))
        });
        let agw = std::sync::Mutex::new(agw);
        let port_a = admin_port;
        std::thread::spawn(move || {
            let gw = agw.into_inner().unwrap();
            let _ = gw.serve("127.0.0.1", port_a);
        });
        eprintln!("admin healthz on 127.0.0.1:{}", admin_port);
    }

    if let Err(e) = gw.serve(&host, port) {
        eprintln!("监听失败 {}:{}: {}", host, port, e);
        std::process::exit(2);
    }
}

fn non_empty(s: &str) -> Option<String> {
    if s.trim().is_empty() { None } else { Some(s.trim().to_string()) }
}

fn sess_close_payload(session: &Value, already: bool) -> Value {
    json!({
        "ok": true,
        "session_id": session.get("session_id"),
        "state": session.get("state"),
        "close_reason": session.get("close_reason"),
        "closed_at": session.get("closed_at"),
        "archive_ref": session.get("archive_ref"),
        "archived": !session.get("archive_ref").unwrap_or(&Value::Null).is_null(),
        "turn_count": session.get("turn_count"),
        "already_closed": already,
    })
}

fn sess_detail(
    rt: &BbSvc,
    peer: &str,
    raw_id: &str,
    query: &str,
    default_limit: i64,
    with_session: bool,
) -> Result<Reply, hacs_l1_transport::web_gateway::HttpError> {
    let session_id = urldecode(raw_id);
    if !bb::session_id_ok(&session_id) {
        return Ok(Reply::json(404, json!({"ok": false, "error": "not_found", "detail": "没有这个会话"})));
    }
    let limit = match clamp_limit(&one(query, "limit"), default_limit, bb::SESS_TURNS_MAX_LIMIT) {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(e.status, e.body())),
    };
    let session = match rt.select_agent_session(&session_id) {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}))),
    };
    let Some(session) = session else {
        return Ok(Reply::json(404, json!({"ok": false, "error": "not_found", "detail": "没有这个会话"})));
    };
    if !bb::sess_visible_to(&session, peer) {
        return Ok(Reply::json(403, json!({"ok": false, "error": "forbidden", "detail": "这个会话没有邀请你"})));
    }
    let turns = match rt.select_agent_turns(&session_id, limit) {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}))),
    };
    let count = turns.len();
    Ok(Reply::json(200, if with_session {
        json!({"ok": true, "session": session, "turns": turns, "count": count, "now": bb::iso_z(bb::now_micros()), "tz": "UTC"})
    } else {
        json!({"ok": true, "session_id": session_id, "state": session.get("state"), "count": count, "turn_count": session.get("turn_count"), "turns": turns, "now": bb::iso_z(bb::now_micros()), "tz": "UTC"})
    }))
}

fn sess_turn_write(rt: &BbSvc, peer: &str, session_id: &str, req: &hacs_l1_transport::web_gateway::Request) -> Result<Reply, hacs_l1_transport::web_gateway::HttpError> {
    let body = req.body.clone();
    if body.len() > bb::MAX_BODY_BYTES {
        return Ok(Reply::json(413, json!({"ok": false, "error": "payload_too_large", "detail": format!("请求体上限 {} 字节", bb::MAX_BODY_BYTES)})));
    }
    let v: Value = match serde_json::from_slice::<Value>(&body) {
        Ok(v) if v.is_object() => v,
        Ok(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "body 必须是 JSON 对象"}))),
        Err(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "body 不是合法 JSON"}))),
    };
    let role = match v.get("role") {
        Some(Value::String(s)) if bb::SESS_ROLES.contains(&s.trim().to_ascii_lowercase().as_str()) => s.trim().to_ascii_lowercase(),
        _ => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "role 必须是 user / assistant / system 之一"}))),
    };
    let content = match norm_text(v.get("content"), "content", false, "") {
        Ok(c) => c,
        Err(e) => return Ok(Reply::json(e.status, e.body())),
    };
    if content.chars().count() > bb::MAX_BODY_BYTES {
        return Ok(Reply::json(413, json!({"ok": false, "error": "payload_too_large", "detail": format!("content 超过 {} 字符", bb::MAX_BODY_BYTES)})));
    }
    let content_ref: Option<Value> = match v.get("content_ref") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() { None } else { Some(json!(t)) }
        }
        Some(x @ Value::Object(_)) if !x.as_object().unwrap().is_empty() => Some(x.clone()),
        Some(Value::Object(_)) => None,
        Some(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "content_ref 必须是字符串或对象"}))),
    };
    let to_member: Option<String> = match v.get("to_member") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Some(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "to_member 必须是非空字符串"}))),
    };
    if content.trim().is_empty() && content_ref.is_none() {
        return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "content 与 content_ref 至少要有一个"})));
    }
    if v.get("from_member").or(v.get("from")).and_then(Value::as_str).is_some() {
        println!("忽略 turns 请求体里的 from 字段（身份只认 token）actual={}", peer);
    }
    let session = match rt.select_agent_session(session_id) {
        Ok(s) => s,
        Err(e) => return Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}))),
    };
    let Some(session) = session else {
        return Ok(Reply::json(404, json!({"ok": false, "error": "not_found", "detail": "没有这个会话"})));
    };
    if !bb::sess_visible_to(&session, peer) {
        return Ok(Reply::json(403, json!({"ok": false, "error": "forbidden", "detail": "这个会话没有邀请你"})));
    }
    match rt.insert_agent_turn(session_id, peer, to_member.as_deref(), &role, &content, content_ref.as_ref()) {
        Err(e) => {
            let low = e.0.to_lowercase();
            if low.contains("closed") || low.contains("expired") || low.contains("terminal")
                || low.contains("not open") || e.0.contains("不是 open")
            {
                return Ok(Reply::json(409, json!({"ok": false, "error": "session_not_open", "detail": "这场会话已经结束了，不能再追加轮次"})));
            }
            Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)})))
        }
        Ok(turn) => Ok(Reply::json(201, json!({"ok": true, "session_id": session_id, "turn": turn, "now": bb::iso_z(bb::now_micros()), "tz": "UTC"}))),
    }
}

fn tmp_read(rt: &BbSvc, peer: &str, raw_id: &str) -> Result<Reply, hacs_l1_transport::web_gateway::HttpError> {
    let tmp_id = urldecode(raw_id);
    if !bb::tmp_id_ok(&tmp_id) {
        return Ok(Reply::json(404, json!({"ok": false, "error": "not_found", "detail": "没有这条临时共享"})));
    }
    let item = match rt.select_tmp(&tmp_id) {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}))),
    };
    let Some(item) = item else {
        return Ok(Reply::json(404, json!({"ok": false, "error": "not_found", "detail": "没有这条临时共享"})));
    };
    let audience: Vec<Value> = item
        .get("audience")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if item.get("owner").and_then(Value::as_str) != Some(peer)
        && !audience.iter().any(|a| a.as_str() == Some(peer))
    {
        return Ok(Reply::json(403, json!({"ok": false, "error": "forbidden", "detail": "这条临时共享没有共享给你"})));
    }
    if item.get("destroyed").and_then(Value::as_bool).unwrap_or(false) {
        return tmp_gone(peer, &item, item.get("destroy_reason").and_then(Value::as_str).unwrap_or("destroyed"), None);
    }
    if item.get("expired").and_then(Value::as_bool).unwrap_or(false) {
        let ts = rt.tmp_expire_now(&tmp_id).unwrap_or(None);
        return tmp_gone(peer, &item, "expired", Some(ts.unwrap_or_else(|| bb::iso_z(bb::now_micros()))));
    }
    let body = item.get("body").cloned().unwrap_or(Value::Null);
    let mut repeat = false;
    let mut destroyed = false;
    let mut reads_count = item.get("reads_count").and_then(Value::as_i64).unwrap_or(0);
    let mut reads_max = item.get("reads_max").and_then(Value::as_i64).unwrap_or(0);
    let readers: Vec<Value> = item.get("readers").and_then(Value::as_array).cloned().unwrap_or_default();
    if readers.iter().any(|r| r.as_str() == Some(peer)) {
        repeat = true;
    } else {
        match rt.tmp_counted_read(&tmp_id, peer) {
            Err(e) => return Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}))),
            Ok(None) => {
                // 并发：被抢先销毁 / 同人的并发读 → 回读真源
                let fresh = match rt.select_tmp(&tmp_id) {
                    Ok(f) => f,
                    Err(e) => return Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}))),
                };
                match fresh {
                    None => return tmp_gone(peer, &item, "gone", None),
                    Some(f) => {
                        if f.get("destroyed").and_then(Value::as_bool).unwrap_or(false)
                            || f.get("expired").and_then(Value::as_bool).unwrap_or(false)
                        {
                            return tmp_gone(peer, &f, f.get("destroy_reason").and_then(Value::as_str).unwrap_or("gone"), None);
                        }
                        let freaders: Vec<Value> = f.get("readers").and_then(Value::as_array).cloned().unwrap_or_default();
                        if freaders.iter().any(|r| r.as_str() == Some(peer)) {
                            repeat = true;
                            reads_count = f.get("reads_count").and_then(Value::as_i64).unwrap_or(0);
                            reads_max = f.get("reads_max").and_then(Value::as_i64).unwrap_or(0);
                        } else {
                            return Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": "并发读冲突，请重试"})));
                        }
                    }
                }
            }
            Ok(Some(bb::MsgFinishRow(c, m, d, _at))) => {
                reads_count = c;
                reads_max = m;
                destroyed = d;
            }
        }
    }
    Ok(Reply::json(200, json!({
        "ok": true, "tmp_id": tmp_id, "title": item.get("title"),
        "body": body, "bytes": item.get("bytes"),
        "reads_count": reads_count, "reads_max": reads_max,
        "reads_left": (reads_max - reads_count).max(0),
        "repeat": repeat, "destroyed": destroyed,
        "destroy_reason": if destroyed { json!("read_quota") } else { Value::Null },
        "expires_at": item.get("expires_at"),
    })))
}

fn tmp_gone(peer: &str, item: &Value, reason: &str, destroyed_at: Option<String>) -> Result<Reply, hacs_l1_transport::web_gateway::HttpError> {
    Ok(Reply::json(410, json!({
        "ok": false, "error": "gone", "tmp_id": item.get("tmp_id"),
        "destroy_reason": reason,
        "destroyed_at": destroyed_at.clone().unwrap_or_default(),
    })))
    .inspect(|_| { let _ = peer; })
}

fn tmp_delete(rt: &BbSvc, peer: &str, raw_id: &str) -> Result<Reply, hacs_l1_transport::web_gateway::HttpError> {
    let tmp_id = urldecode(raw_id);
    if !bb::tmp_id_ok(&tmp_id) {
        return Ok(Reply::json(404, json!({"ok": false, "error": "not_found", "detail": "没有这条临时共享"})));
    }
    let item = match rt.select_tmp(&tmp_id) {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}))),
    };
    let Some(item) = item else {
        return Ok(Reply::json(404, json!({"ok": false, "error": "not_found", "detail": "没有这条临时共享"})));
    };
    if item.get("owner").and_then(Value::as_str) != Some(peer) {
        println!("非 owner 试图销毁 tmp_id={} from={}", tmp_id, peer);
        return Ok(Reply::json(403, json!({"ok": false, "error": "forbidden", "detail": "只有写入者能销毁这条临时共享"})));
    }
    if item.get("destroyed").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(Reply::json(200, json!({
            "ok": true, "tmp_id": tmp_id,
            "destroyed_at": item.get("destroyed_at"),
            "destroy_reason": item.get("destroy_reason"),
        })));
    }
    let destroyed_at = match rt.tmp_destroy_by_owner(&tmp_id) {
        Err(e) => return Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}))),
        Ok(Some(ts)) => ts,
        Ok(None) => {
            let fresh = rt.select_tmp(&tmp_id).unwrap_or(None);
            match fresh {
                Some(f) => {
                    return Ok(Reply::json(200, json!({
                        "ok": true, "tmp_id": tmp_id,
                        "destroyed_at": f.get("destroyed_at"),
                        "destroy_reason": f.get("destroy_reason"),
                    })));
                }
                None => bb::iso_z(bb::now_micros()),
            }
        }
    };
    Ok(Reply::json(200, json!({"ok": true, "tmp_id": tmp_id, "destroyed_at": destroyed_at, "destroy_reason": "owner"})))
}

/// GET /actions | /issues | /tracking 的共用分派。
static PEER_MAP: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();

fn peer_map_global() -> &'static Vec<(String, String)> {
    PEER_MAP.get_or_init(Vec::new)
}

fn one(q: &str, key: &str) -> String {
    for pair in q.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        if k == key {
            return urldecode(v);
        }
    }
    String::new()
}

fn clamp_limit(raw: &str, default: i64, max: i64) -> Result<i64, ApiError> {
    if raw.is_empty() {
        return Ok(default);
    }
    match raw.parse::<i64>() {
        Err(_) => Err(ApiError::new(400, "bad_request").detail("limit 必须是整数")),
        Ok(v) => {
            let v = if v <= 0 { default } else { v };
            Ok(v.min(max))
        }
    }
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
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

fn parse_since(raw: &str) -> Result<Option<String>, ApiError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    // unix 秒（1..12 位数字，可带小数）
    if !raw.is_empty()
        && raw.bytes().all(|c| c.is_ascii_digit() || c == b'.')
        && raw.len() <= 13
        && raw.contains(|c: char| c.is_ascii_digit())
    {
        if let Ok(secs) = raw.parse::<f64>() {
            let us = (secs * 1_000_000.0).round() as i64;
            let (y, mo, d, h, mi, s) = bb::civil_from_epoch(us.div_euclid(1_000_000));
            return Ok(Some(format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
                y, mo, d, h, mi, s
            )));
        }
        return Err(ApiError::new(400, "bad_request").detail("since 不是合法时间"));
    }
    if raw.len() > 64 || !raw.bytes().all(|c| c.is_ascii_digit() || b"TZ:-+. ".contains(&c)) {
        return Err(ApiError::new(400, "bad_request").detail("since 不是合法时间"));
    }
    Ok(Some(raw.to_string()))
}

fn parse_flag(raw: &str, field: &str) -> Result<bool, ApiError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" => Ok(false),
        "1" | "true" | "yes" | "on" => Ok(true),
        _ => Err(ApiError::new(400, "bad_request").detail(format!("{} 只接受 0 或 1", field))),
    }
}

fn parse_status(raw: &str, allowed: &[&str], field: &str) -> Result<Option<String>, ApiError> {
    let v = raw.trim();
    if v.is_empty() {
        return Ok(None);
    }
    if !allowed.contains(&v) {
        return Err(ApiError::new(400, "bad_request").detail(format!(
            "{} 只能是 {} 之一",
            field,
            allowed.join("/")
        )));
    }
    Ok(Some(v.to_string()))
}

fn parse_owner(raw: &str, field: &str) -> Result<Option<String>, ApiError> {
    let v = raw.trim();
    if v.is_empty() {
        return Ok(None);
    }
    if v.len() > bb::OWNER_MAX {
        return Err(ApiError::new(400, "bad_request").detail(format!("{} 超过 {} 字符", field, bb::OWNER_MAX)));
    }
    Ok(Some(v.to_string()))
}

fn norm_text(value: Option<&Value>, field: &str, required: bool, default: &str) -> Result<String, ApiError> {
    match value {
        None | Some(Value::Null) => {
            if required {
                Err(ApiError::new(400, "bad_request").detail(format!("{} 必填", field)))
            } else {
                Ok(default.to_string())
            }
        }
        Some(Value::String(s)) => {
            let v = s.trim();
            if v.is_empty() {
                if required {
                    return Err(ApiError::new(400, "bad_request").detail(format!("{} 不能为空", field)));
                }
                return Ok(default.to_string());
            }
            if v.chars().count() > bb::TEXT_FIELD_MAX {
                return Err(ApiError::new(400, "bad_request").detail(format!("{} 超过 {} 字符", field, bb::TEXT_FIELD_MAX)));
            }
            Ok(v.to_string())
        }
        Some(_) => Err(ApiError::new(400, "bad_request").detail(format!("{} 必须是字符串", field))),
    }
}

fn tracking_route(rt: &BbSvc, req: &hacs_l1_transport::web_gateway::Request, kind: &str) -> Result<Reply, hacs_l1_transport::web_gateway::HttpError> {
    let _peer = match auth_peer(req.header("Authorization"), &rt.peer_map, false) {
        Some(p) => p,
        None => return Ok(Reply::json(401, json!({"ok": false, "error": "unauthorized"}))),
    };
    let status = if kind != "tracking" {
        let allowed: &[&str] = if kind == "actions" { &bb::ACTION_STATUSES } else { &bb::ISSUE_STATUSES };
        match parse_status(&one(&req.query, "status"), allowed, "status") {
            Ok(v) => v,
            Err(e) => return Ok(Reply::json(e.status, e.body())),
        }
    } else {
        None
    };
    let issue_kind = if kind == "issues" {
        match parse_status(&one(&req.query, "kind"), &bb::ISSUE_KINDS, "kind") {
            Ok(v) => v,
            Err(e) => return Ok(Reply::json(e.status, e.body())),
        }
    } else {
        None
    };
    let owner = match parse_owner(&one(&req.query, "owner"), "owner") {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(e.status, e.body())),
    };
    let overdue = match parse_flag(&one(&req.query, "overdue"), "overdue") {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(e.status, e.body())),
    };
    let limit = match clamp_limit(&one(&req.query, "limit"), bb::TRACK_DEFAULT_LIMIT, bb::TRACK_MAX_LIMIT) {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(e.status, e.body())),
    };
    let degraded: Vec<String> = vec![];
    let now_iso = bb::iso_utc_now();
    let mut degraded = degraded;
    if kind == "tracking" {
        let (actions, issues, counts) = rt.select_tracking(owner.as_deref(), overdue, limit, &mut degraded);
        Ok(Reply::json(200, json!({"ok": true, "actions": actions, "issues": issues, "counts": counts, "degraded": degraded, "now": now_iso, "tz": "UTC"})))
    } else {
        let rows = if kind == "actions" {
            rt.select_actions(status.as_deref(), owner.as_deref(), overdue, limit, &mut degraded)
        } else {
            rt.select_issues(status.as_deref(), owner.as_deref(), overdue, limit, issue_kind.as_deref(), &mut degraded)
        };
        let key = if kind == "actions" { "actions" } else { "issues" };
        Ok(Reply::json(200, json!({"ok": true, "count": rows.len(), key: rows, "degraded": degraded, "now": now_iso, "tz": "UTC"})))
    }
}

/// GET /api/shared[/tasks|/search|/<id>] 与 /shared[...] 的共用分派。
fn shared_route(rt: &BbSvc, req: &hacs_l1_transport::web_gateway::Request) -> Result<Reply, hacs_l1_transport::web_gateway::HttpError> {
    let peer = match auth_peer(req.header("Authorization"), &rt.peer_map, false) {
        Some(p) => p,
        None => return Ok(Reply::json(401, json!({"ok": false, "error": "unauthorized"}))),
    };
    let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    let rest: Vec<&str> = if parts[0] == "api" { parts[2..].to_vec() } else { parts[1..].to_vec() };
    if rest.is_empty() {
        shared_list(rt, &peer, req, false)
    } else if rest == ["tasks"] {
        shared_tasks(rt, &peer, req)
    } else if rest == ["search"] {
        shared_list(rt, &peer, req, true)
    } else if rest.len() == 1 {
        shared_detail(rt, &peer, rest[0])
    } else {
        Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})))
    }
}

fn shared_filters(
    peer: &str,
    scope: Option<&str>, kind: Option<&str>, owner: Option<&str>,
    tag: Option<&str>, task_id: Option<&str>, status: Option<&str>, expired: bool,
) -> (Vec<String>, Vec<Box<dyn postgres::types::ToSql + Sync>>) {
    let mut where_parts: Vec<String> = vec![];
    let mut params: Vec<Box<dyn postgres::types::ToSql + Sync>> = vec![];
    let push = |s: &str, cond: String, params: &mut Vec<Box<dyn postgres::types::ToSql + Sync>>| {
        params.push(Box::new(s.to_string()));
        // 占位：真正的编号在调用处拼（闭包里拿不到 len）；改为在下面统一编号
        let _ = cond;
    };
    let add = |val: Box<dyn postgres::types::ToSql + Sync>, cond: &dyn Fn(usize) -> String, params: &mut Vec<Box<dyn postgres::types::ToSql + Sync>>, parts: &mut Vec<String>| {
        params.push(val);
        parts.push(cond(params.len()));
    };
    if let Some(v) = scope {
        add(Box::new(v.to_string()), &|n| format!("scope = ${}", n), &mut params, &mut where_parts);
    }
    if let Some(v) = kind {
        add(Box::new(v.to_string()), &|n| format!("kind = ${}", n), &mut params, &mut where_parts);
    }
    if let Some(v) = owner {
        add(Box::new(v.to_string()), &|n| format!("owner = ${}", n), &mut params, &mut where_parts);
    }
    if let Some(v) = task_id {
        add(Box::new(v.to_string()), &|n| format!("task_id = ${}", n), &mut params, &mut where_parts);
    }
    if let Some(v) = tag {
        let tags = json!([v]);
        add(Box::new(tags.to_string()), &|n| format!("tags @> ${}::jsonb", n), &mut params, &mut where_parts);
    }
    if let Some(v) = status {
        add(Box::new(v.to_string()), &|n| format!("status = ${}", n), &mut params, &mut where_parts);
    } else {
        for st in bb::SHARED_HIDDEN_STATUSES {
            add(Box::new(st.to_string()), &|n| format!("status <> ${}", n), &mut params, &mut where_parts);
        }
        if !expired {
            add(Box::new("expired".to_string()), &|n| format!("status <> ${}", n), &mut params, &mut where_parts);
            let now_iso = bb::iso_utc_now();
            add(Box::new(now_iso), &|n| format!("(expires_at IS NULL OR expires_at >= ${}::text::timestamptz)", n), &mut params, &mut where_parts);
        }
    }
    // audience：all 或点名请求者（jsonb @>，吃 GIN 索引）
    let all = json!([bb::SHARED_ALL_AUDIENCE]);
    let me = json!([peer]);
    params.push(Box::new(all));
    let n1 = params.len();
    params.push(Box::new(me));
    let n2 = params.len();
    where_parts.push(format!("(audience @> ${}::jsonb OR audience @> ${}::jsonb)", n1, n2));
    let _ = push;
    (where_parts, params)
}

fn shared_list(rt: &BbSvc, peer: &str, req: &hacs_l1_transport::web_gateway::Request, search: bool) -> Result<Reply, hacs_l1_transport::web_gateway::HttpError> {
    let scope = match parse_status(&one(&req.query, "scope"), &bb::SHARED_SCOPES, "scope") {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(e.status, e.body())),
    };
    let status = match parse_status(&one(&req.query, "status"), &bb::SHARED_STATUSES, "status") {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(e.status, e.body())),
    };
    let expired = match parse_flag(&one(&req.query, "expired"), "expired") {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(e.status, e.body())),
    };
    let _semantic = match parse_flag(&one(&req.query, "semantic"), "semantic") {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(e.status, e.body())),
    };
    let kind = match parse_owner(&one(&req.query, "kind"), "kind") {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(e.status, e.body())),
    };
    let owner = match parse_owner(&one(&req.query, "owner"), "owner") {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(e.status, e.body())),
    };
    let tag = match parse_owner(&one(&req.query, "tag"), "tag") {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(e.status, e.body())),
    };
    let task_id = match parse_owner(&one(&req.query, "task_id"), "task_id") {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(e.status, e.body())),
    };
    let q = one(&req.query, "q");
    if search && q.is_empty() {
        return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": "q 不能为空"})));
    }
    if q.chars().count() > bb::SEARCH_QUERY_MAX {
        return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request", "detail": format!("q 超过 {} 字符", bb::SEARCH_QUERY_MAX)})));
    }
    let limit = match clamp_limit(&one(&req.query, "limit"), bb::SHARED_DEFAULT_LIMIT, bb::SHARED_MAX_LIMIT) {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(e.status, e.body())),
    };
    let (where_parts, params) = shared_filters(peer, scope.as_deref(), kind.as_deref(), owner.as_deref(), tag.as_deref(), task_id.as_deref(), status.as_deref(), expired);
    let mut degraded: Vec<String> = vec![];
    let now_iso = bb::iso_utc_now();
    let used;
    let items = if q.is_empty() {
        used = "list";
        match rt.select_shared(&where_parts, params, limit, &mut degraded) {
            Ok(v) => v,
            Err(e) => return Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}))),
        }
    } else {
        // 嵌入不可用 → 关键词检索（mode 如实报 keyword）
        used = "keyword";
        match rt.search_shared_keyword(&q, &where_parts, params, limit, &mut degraded) {
            Ok(v) => v,
            Err(e) => return Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}))),
        }
    };
    Ok(Reply::json(200, json!({"ok": true, "count": items.len(), "shared": items, "mode": used, "query": if q.is_empty() { Value::Null } else { json!(q) }, "degraded": degraded, "now": now_iso, "tz": "UTC"})))
}

fn shared_detail(rt: &BbSvc, peer: &str, raw_id: &str) -> Result<Reply, hacs_l1_transport::web_gateway::HttpError> {
    let raw_id = urldecode(raw_id).trim().to_string();
    if raw_id.is_empty() || !raw_id.bytes().all(|b| b.is_ascii_digit()) || raw_id.len() > 19 {
        return Ok(Reply::json(404, json!({"ok": false, "error": "not_found", "detail": "没有这条共享内容"})));
    }
    let content_id: i64 = raw_id.parse().unwrap_or(0);
    let mut degraded: Vec<String> = vec![];
    let item = match rt.select_shared_one(content_id, &mut degraded) {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}))),
    };
    let Some(item) = item else {
        return Ok(Reply::json(404, json!({"ok": false, "error": "not_found", "detail": "没有这条共享内容"})));
    };
    let audience: Vec<Value> = item.get("audience").and_then(Value::as_array).cloned().unwrap_or_default();
    let visible = audience.iter().any(|a| {
        a.as_str() == Some(bb::SHARED_ALL_AUDIENCE) || a.as_str() == Some(peer)
    });
    if !visible {
        println!("受众不含请求者，拒绝 content_id={} from={}", content_id, peer);
        return Ok(Reply::json(403, json!({"ok": false, "error": "forbidden", "detail": "这条共享内容没有共享给你"})));
    }
    let revisions = match rt.select_shared_revisions(content_id, &mut degraded) {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}))),
    };
    Ok(Reply::json(200, json!({"ok": true, "shared": item, "revisions": revisions, "degraded": degraded, "now": bb::iso_utc_now(), "tz": "UTC"})))
}

fn shared_tasks(rt: &BbSvc, _peer: &str, req: &hacs_l1_transport::web_gateway::Request) -> Result<Reply, hacs_l1_transport::web_gateway::HttpError> {
    let status = match parse_status(&one(&req.query, "status"), &bb::SHARED_TASK_STATUSES, "status") {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(e.status, e.body())),
    };
    let limit = match clamp_limit(&one(&req.query, "limit"), bb::SHARED_DEFAULT_LIMIT, bb::SHARED_MAX_LIMIT) {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(e.status, e.body())),
    };
    let mut degraded: Vec<String> = vec![];
    let tasks = match rt.select_shared_tasks(status.as_deref(), limit, &mut degraded) {
        Ok(v) => v,
        Err(e) => return Ok(Reply::json(503, json!({"ok": false, "error": "db_unavailable", "detail": bb::safe_err(&e.0)}))),
    };
    Ok(Reply::json(200, json!({"ok": true, "count": tasks.len(), "tasks": tasks, "degraded": degraded, "now": bb::iso_utc_now(), "tz": "UTC"})))
}
