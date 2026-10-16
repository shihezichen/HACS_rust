//! `task_svc` —— 群任务清单服务 HTTP 装配（对应 Python `hub/l3_domain/task_svc.py` 的
//! `Handler` + `main`，端口 9981）。
//!
//! 路由面（与 Python 逐条对齐）：
//! * GET  `/`、`/health` → 服务健康（含 groups 列表）
//! * GET  `/tasks?group=&status=&limit=` → 列表（special 优先 + created_at asc）
//! * GET  `/tasks/next?group=&agent=` → 取队首并置 running（空队列 reason=empty_queue）
//! * GET  `/tasks/lease` → 租约快照；GET `/chains` → 已冻结链
//! * GET  `/tasks/<id>`；POST `/tasks`（201 / 幂等 200 already_exists）
//! * POST `/tasks/ack`（403/404/409/400 错误码映射）、`/chains/unfreeze`、`/tasks/lease/reap`
//! * POST `/tasks/<id>/report`（首次 → 立刻投递回帖；重复 → 绝不重发）
//! * PATCH `/tasks/<id>`；DELETE `/tasks/<id>`；其余 → 404 no_route
//!
//! 后台线程：回帖重试（30s，`--no-reply` 关）+ lease 巡检（10s，永远启）。
#![allow(clippy::too_many_arguments)]

use hacs_l1_transport::web_gateway::{Gateway, Reply};
use hacs_l3_domain::task_svc::{self, Store};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const CHAT_RETRY_INTERVAL: u64 = 30; // TASK_CHAT_RETRY_INTERVAL
const LEASE_REAP_INTERVAL: u64 = 10; // TASK_LEASE_REAP_INTERVAL

struct Rt {
    store: Store,
    chat_base: String,
    agent_fallback: String,
    retry_max: i64,
    lease_s_default: i64,
    max_dispatch: i64,
    whitelist: Vec<String>,
    no_reply: bool,
}

fn one(q: &str, key: &str, d: Option<&str>) -> Option<String> {
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            return Some(v.to_string());
        }
    }
    d.map(str::to_string)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut host, mut port, mut db) = ("0.0.0.0".to_string(), 9981u16, task_svc::default_db_path());
    let mut chat_base = std::env::var("CHAT_BASE").unwrap_or_else(|_| hacs_l0_host::node_profile::chat_base_default());
    let mut whitelist_env = std::env::var("TASK_AGENT_WHITELIST").unwrap_or_default();
    let mut lease_s_default: i64 = std::env::var("TASK_LEASE_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let mut no_reply = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--host" => host = args.next().unwrap_or(host),
            "--port" => port = args.next().and_then(|v| v.parse().ok()).unwrap_or(port),
            "--db" => db = args.next().unwrap_or(db),
            "--chat-base" => chat_base = args.next().unwrap_or(chat_base),
            "--agent-whitelist" => whitelist_env = args.next().unwrap_or(whitelist_env),
            "--lease-s" => {
                lease_s_default = args.next().and_then(|v| v.parse().ok()).unwrap_or(lease_s_default)
            }
            "--no-reply" => no_reply = true,
            _ => {}
        }
    }
    let whitelist: Vec<String> = if whitelist_env.trim().is_empty() {
        // Python 默认：hermes,openclaw-nb（TASK_AGENT_WHITELIST 未设时）
        if std::env::var("TASK_AGENT_WHITELIST").is_err() {
            vec!["hermes".into(), "openclaw-nb".into()]
        } else {
            vec![]
        }
    } else {
        whitelist_env
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    };
    let retry_max: i64 = std::env::var("TASK_CHAT_RETRY_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let max_dispatch: i64 = std::env::var("TASK_LEASE_MAX_DISPATCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let agent_fallback = std::env::var("TASK_CHAT_AGENT").unwrap_or_else(|_| "hermes".into());

    let rt = Arc::new(Rt {
        store: Store::open(&db).expect("打不开 SQLite 库"),
        chat_base,
        agent_fallback,
        retry_max,
        lease_s_default,
        max_dispatch,
        whitelist,
        no_reply,
    });

    // ---- 后台线程：回帖重试 + lease 巡检 --------------------------------
    {
        let rt2 = rt.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(CHAT_RETRY_INTERVAL));
            if rt2.no_reply {
                continue;
            }
            for row in rt2.store.reply_pending(rt2.retry_max, 20) {
                if let Some(tid) = row.get("task_id").and_then(Value::as_str) {
                    rt2.store
                        .reply_deliver(tid, &rt2.chat_base, &rt2.agent_fallback, rt2.retry_max);
                }
            }
        });
    }
    {
        let rt2 = rt.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(LEASE_REAP_INTERVAL));
            rt2.store.reap_leases(rt2.max_dispatch);
        });
    }

    let mut gw = Gateway::new("task_svc", Some("0.1"), true);
    gw.protocol_version = "HTTP/1.1".to_string();

    // ---- GET ------------------------------------------------------------
    let rt_h = rt.clone();
    gw.route("/", &["GET"], false, move |req| {
        if req.path == "/" || req.path == "/health" {
            return Ok(Reply::json(
                200,
                json!({
                    "ok": true, "service": "task_svc", "version": "0.1",
                    "time": task_svc::now_iso(),
                    "groups": rt_h.store.groups(),
                }),
            ));
        }
        Err(err_not_found(&req.path))
    });
    let rt_h = rt.clone();
    gw.route("/tasks", &["GET"], false, move |req| {
        let Some(group) = one(&req.query, "group", None) else {
            return Ok(Reply::json(400, json!({"ok": false, "error": "group_required"})));
        };
        let status = one(&req.query, "status", None).filter(|s| !s.is_empty());
        if let Some(st) = &status {
            if !task_svc::VALID_STATUS.contains(&st.as_str()) {
                return Ok(Reply::json(400, json!({"ok": false, "error": "bad_status"})));
            }
        }
        let limit: i64 = one(&req.query, "limit", Some("500"))
            .and_then(|v| v.parse::<i64>().ok())
            .map(|v| v.min(2000))
            .unwrap_or(500);
        let items = rt_h.store.list(&group, status.as_deref(), limit);
        Ok(Reply::json(
            200,
            json!({
                "ok": true, "group": group, "status": status,
                "count": items.len(), "tasks": items,
                "order": "priority(special first) + created_at asc",
            }),
        ))
    });
    let rt_h = rt.clone();
    gw.route("/tasks/next", &["GET"], false, move |req| {
        let group = one(&req.query, "group", None).unwrap_or_default();
        let agent = one(&req.query, "agent", None).unwrap_or_default();
        if group.is_empty() || agent.is_empty() {
            return Ok(Reply::json(400, json!({"ok": false, "error": "group_and_agent_required"})));
        }
        match rt_h.store.next_for(&group, &agent) {
            None => Ok(Reply::json(
                200,
                json!({"ok": true, "task": null, "reason": "empty_queue", "group": group, "agent": agent}),
            )),
            Some(t) => Ok(Reply::json(
                200,
                json!({"ok": true, "task": t, "dispatched": true, "group": group, "agent": agent}),
            )),
        }
    });
    let rt_h = rt.clone();
    gw.route("/tasks/lease", &["GET"], false, move |_req| {
        Ok(Reply::json(
            200,
            json!({
                "ok": true, "leases": rt_h.store.lease_snapshot(),
                "lease_s_default": rt_h.lease_s_default,
                "lease_max_dispatch": rt_h.max_dispatch,
                "whitelist": rt_h.whitelist,
            }),
        ))
    });
    let rt_h = rt.clone();
    gw.route("/chains", &["GET"], false, move |_req| {
        Ok(Reply::json(200, json!({"ok": true, "frozen": rt_h.store.freeze_list()})))
    });
    let rt_h = rt.clone();
    gw.route("/tasks", &["GET"], true, move |req| {
        let tid = req.path.trim_start_matches("/tasks/");
        match rt_h.store.get(tid) {
            None => Ok(Reply::json(404, json!({"ok": false, "error": "not_found"}))),
            Some(t) => Ok(Reply::json(200, json!({"ok": true, "task": t}))),
        }
    });

    // ---- POST -----------------------------------------------------------
    let rt_h = rt.clone();
    gw.route("/tasks", &["POST"], false, move |req| {
        let body = match req.json_body(1 << 20) {
            Ok(v) => v,
            Err(e) => return Ok(Reply::json(e.status, json!({"ok": false, "error": "bad_json"}))),
        };
        let group = body
            .get("group_id")
            .and_then(Value::as_str)
            .or_else(|| body.get("group").and_then(Value::as_str))
            .unwrap_or("");
        let title = body.get("title").and_then(Value::as_str).unwrap_or("").trim();
        if group.is_empty() || title.is_empty() {
            return Ok(Reply::json(400, json!({"ok": false, "error": "group_and_title_required"})));
        }
        let as_i64 = |k: &str| body.get(k).and_then(Value::as_i64);
        let as_str = |k: &str| body.get(k).and_then(Value::as_str);
        match rt_h.store.create(
            group,
            title,
            as_str("content"),
            as_str("owner"),
            as_str("priority").unwrap_or("normal"),
            as_str("msg_seq"),
            as_str("proposer"),
            as_str("status").unwrap_or("pending"),
            as_str("id"),
            as_str("chain_id"),
            as_str("kind"),
            as_str("idem_key"),
            as_str("origin_task_id"),
            as_str("context_digest"),
            as_str("groups"),
            as_str("fallback"),
            as_i64("lease_s"),
            body.get("hold").and_then(Value::as_i64).unwrap_or(0),
        ) {
            Err(e) => Ok(Reply::json(400, json!({"ok": false, "error": e}))),
            Ok(t) => {
                if t.get("already_exists").and_then(Value::as_bool).unwrap_or(false) {
                    Ok(Reply::json(200, json!({"ok": true, "already_exists": true, "task": t})))
                } else {
                    Ok(Reply::json(201, json!({"ok": true, "task": t})))
                }
            }
        }
    });
    let rt_h = rt.clone();
    gw.route("/tasks/ack", &["POST"], false, move |req| {
        let body = match req.json_body(1 << 20) {
            Ok(v) => v,
            Err(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_json"}))),
        };
        let tid = body
            .get("task_id")
            .and_then(Value::as_str)
            .or_else(|| body.get("id").and_then(Value::as_str))
            .unwrap_or("")
            .trim()
            .to_string();
        let agent = body.get("agent").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if tid.is_empty() || agent.is_empty() {
            return Ok(Reply::json(400, json!({"ok": false, "error": "task_id_and_agent_required"})));
        }
        // Python `int(lease_s)`：解析失败 → bad_lease_s
        let lease_s = match body.get("lease_s") {
            None => Some(rt_h.lease_s_default),
            Some(Value::Null) => None,
            Some(v) => match v.as_i64() {
                Some(n) => Some(n),
                None => match v.as_str().and_then(|s| s.parse::<i64>().ok()) {
                    Some(n) => Some(n),
                    None => {
                        return Ok(Reply::json(400, json!({"ok": false, "error": "bad_lease_s"})))
                    }
                },
            },
        };
        let r = rt_h.store.ack(&tid, &agent, lease_s, rt_h.lease_s_default, &rt_h.whitelist);
        if r.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(Reply::json(200, r));
        }
        let code = match r.get("error").and_then(Value::as_str).unwrap_or("") {
            "not_found" => 404,
            "agent_not_allowed" => 403,
            "leased_by_other" | "not_dispatchable" => 409,
            "bad_lease_s" => 400,
            _ => 400,
        };
        Ok(Reply::json(code, r))
    });
    let rt_h = rt.clone();
    gw.route("/chains/unfreeze", &["POST"], false, move |req| {
        let body = match req.json_body(1 << 20) {
            Ok(v) => v,
            Err(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_json"}))),
        };
        let cid = body.get("chain_id").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if cid.is_empty() {
            return Ok(Reply::json(400, json!({"ok": false, "error": "chain_id_required"})));
        }
        Ok(Reply::json(
            200,
            json!({"ok": true, "chain_id": cid, "unfrozen": rt_h.store.unfreeze_chain(&cid)}),
        ))
    });
    let rt_h = rt.clone();
    gw.route("/tasks/lease/reap", &["POST"], false, move |_req| {
        Ok(Reply::json(200, json!({"ok": true, "reaped": rt_h.store.reap_leases(rt_h.max_dispatch)})))
    });
    let rt_h = rt.clone();
    gw.route("/tasks", &["POST"], true, move |req| {
        let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
        if parts.len() == 3 && parts[0] == "tasks" && parts[2] == "report" {
            let tid = parts[1];
            let body = match req.json_body(1 << 20) {
                Ok(v) => v,
                Err(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_json"}))),
            };
            let r = rt_h.store.report(
                tid,
                body.get("agent").and_then(Value::as_str),
                body.get("result").and_then(Value::as_str).unwrap_or("done"),
                body.get("note").and_then(Value::as_str),
                body.get("chain_id").and_then(Value::as_str),
            );
            if !r.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                return Ok(Reply::json(404, r));
            }
            // 首次汇报（非重复）→ 立刻投递回帖；重复汇报**绝不重发**。
            let mut r = r;
            if !r.get("already_reported").and_then(Value::as_bool).unwrap_or(false) {
                let delivered = rt_h.store.reply_deliver(
                    tid,
                    &rt_h.chat_base,
                    &rt_h.agent_fallback,
                    rt_h.retry_max,
                );
                if let Some(m) = r.as_object_mut() {
                    m.insert("reply".into(), delivered);
                }
            }
            return Ok(Reply::json(200, r));
        }
        if parts.len() == 2 && parts[0] == "tasks" && parts[1] == "next" {
            return Ok(Reply::json(405, json!({"ok": false, "error": "use_GET"})));
        }
        Ok(Reply::json(404, json!({"ok": false, "error": "no_route", "path": req.path})))
    });

    // ---- PATCH / DELETE -------------------------------------------------
    let rt_h = rt.clone();
    gw.route("/tasks", &["PATCH"], true, move |req| {
        let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
        if parts.len() != 2 || parts[0] != "tasks" {
            return Ok(Reply::json(404, json!({"ok": false, "error": "no_route", "path": req.path})));
        }
        let body = match req.json_body(1 << 20) {
            Ok(v) => v,
            Err(_) => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_json"}))),
        };
        match rt_h.store.patch(
                parts[1],
                &body,
                rt_h.no_reply,
                &rt_h.whitelist,
                &rt_h.chat_base,
                &rt_h.agent_fallback,
                rt_h.retry_max,
            ) {
            Err(e) => Ok(Reply::json(400, json!({"ok": false, "error": e}))),
            Ok(None) => Ok(Reply::json(404, json!({"ok": false, "error": "not_found"}))),
            Ok(Some(t)) => {
                // Python：PATCH 响应 = {"ok": true, "task": out}，out 里可能带 reply。
                let reply = t.get("reply").cloned();
                let mut resp = json!({"ok": true, "task": t});
                if let Some(r) = reply {
                    resp["reply"] = r;
                }
                Ok(Reply::json(200, resp))
            }
        }
    });
    let rt_h = rt.clone();
    gw.route("/tasks", &["DELETE"], true, move |req| {
        let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
        if parts.len() != 2 || parts[0] != "tasks" {
            return Ok(Reply::json(404, json!({"ok": false, "error": "no_route", "path": req.path})));
        }
        if !rt_h.store.delete(parts[1]) {
            return Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})));
        }
        Ok(Reply::json(200, json!({"ok": true, "deleted": parts[1]})))
    });

    gw.not_found(move |req| Ok(Reply::json(404, json!({"ok": false, "error": "no_route", "path": req.path}))));

    eprintln!("task_svc starting host={host} port={port} db={db}");
    if let Err(e) = gw.serve(&host, port) {
        eprintln!("task_svc 启动失败: {e}");
        std::process::exit(1);
    }
}

fn err_not_found(path: &str) -> hacs_l1_transport::web_gateway::HttpError {
    hacs_l1_transport::web_gateway::HttpError::json(
        404,
        json!({"ok": false, "error": "no_route", "path": path}),
    )
}
