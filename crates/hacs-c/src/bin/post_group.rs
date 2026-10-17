//! `post_group` —— 把一段正文以某 Agent 身份发回人机对话群（对应 Python
//! node/l1_transport/post_group.py）。子 Agent 结果回群的那条回路。
//!
//! 默认：以 hermes 身份、发到默认群（若还开着）或最近更新的 open 临时群、
//! 自动 @ 正文里出现的合法成员。鉴权 = `X-Agent-Id` 头（与 group_watch 同套）。
//! 纪律：只发文本，不打印任何口令；发送后回读校验（--no-verify 跳过）。
//! 退出码：0 成功 / 2 参数错 / 3 拿不到 open 群 / 4 写回失败。
//!
//! todo 翻状态（#448）：--todo-id 与 --todo-status 要么都给要么都不给；
//! 翻 done 必须至少一条 --landing（否则后端 422）。
#![allow(clippy::too_many_arguments)]

use hacs_l1_transport::http_client as hc;
use serde_json::{json, Value};
use std::io::Read;

const DEFAULT_SESSION: &str = "meet_20260921_demo01";

fn chat_base_default() -> String {
    hacs_l0_host::node_profile::chat_base_default()
}

fn http(method: &str, url: &str, body: Option<&Value>, agent_id: Option<&str>) -> (u16, Value) {
    let mut headers: Vec<(&str, &str)> = Vec::new();
    let agent_hdr;
    if let Some(a) = agent_id {
        agent_hdr = ("X-Agent-Id", a);
        headers.push(agent_hdr);
    }
    let res = hc::request_json(
        url,
        method,
        body,
        &headers,
        std::time::Duration::from_secs(25),
    );
    match res {
        Ok((status, v)) => (status, v),
        Err(e) => (
            0,
            json!({"ok": false, "error": {"code": "network", "message": e.to_string()}}),
        ),
    }
}

fn pick_session(base: &str, wanted: &str) -> Option<String> {
    // 选群：显式指定 > env CHAT_SESSION > DEFAULT_SESSION（若开着）> 最近更新的 open 临时群。
    // 钉死默认群的原因：多个 open 自测群会把"最近更新"那条打错群（Python 实测踩过）。
    let (_, resp) = http("GET", &format!("{base}/api/sessions?limit=200"), None, None);
    let groups: Vec<Value> = resp["sessions"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|s| {
                    s.get("kind").and_then(Value::as_str).unwrap_or("") == "meet"
                        && s.get("state").and_then(Value::as_str) == Some("open")
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if !wanted.is_empty() {
        // --session 可能给的是群标题：先核对真 id，不是就按 title 精确匹配（要求唯一命中）
        if groups
            .iter()
            .any(|s| s["session_id"].as_str() == Some(wanted))
        {
            return Some(wanted.to_string());
        }
        let by_title: Vec<&Value> = groups
            .iter()
            .filter(|s| s.get("title").and_then(Value::as_str) == Some(wanted))
            .collect();
        if by_title.len() == 1 {
            let id = by_title[0]["session_id"].as_str().unwrap_or(wanted);
            println!("[post_group] --session 给的是群标题，已解析为 {id}");
            return Some(id.to_string());
        }
        if by_title.len() > 1 {
            println!(
                "[post_group] 群标题 {wanted:?} 命中 {} 个群，歧义，拒绝发送",
                by_title.len()
            );
            return None;
        }
        return Some(wanted.to_string());
    }
    if groups.is_empty() {
        return None;
    }
    let has_default = groups
        .iter()
        .any(|s| s["session_id"].as_str() == Some(DEFAULT_SESSION));
    if has_default {
        return Some(DEFAULT_SESSION.to_string());
    }
    let mut sorted = groups.clone();
    sorted.sort_by(|a, b| {
        b.get("updated_at")
            .and_then(Value::as_str)
            .unwrap_or("")
            .cmp(a.get("updated_at").and_then(Value::as_str).unwrap_or(""))
    });
    sorted
        .first()
        .and_then(|s| s["session_id"].as_str())
        .map(String::from)
}

fn main() {
    let mut chat_base = std::env::var("CHAT_BASE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(chat_base_default);
    let mut session = std::env::var("CHAT_SESSION").unwrap_or_default();
    let mut agent_id = std::env::var("CHAT_AGENT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "hermes".into());
    let mut text: Option<String> = None;
    let mut mention: Vec<String> = Vec::new();
    let mut todo_id = String::new();
    let mut todo_status = String::new();
    let mut landing: Vec<String> = Vec::new();
    let mut no_verify = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--chat-base" => chat_base = args.next().unwrap_or(chat_base),
            "--session" => session = args.next().unwrap_or_default(),
            "--as" => agent_id = args.next().unwrap_or(agent_id),
            "--text" => text = args.next(),
            "--mention" => {
                if let Some(m) = args.next() {
                    mention.push(m);
                }
            }
            "--todo-id" => todo_id = args.next().unwrap_or_default(),
            "--todo-status" => todo_status = args.next().unwrap_or_default(),
            "--landing" => {
                if let Some(l) = args.next() {
                    landing.push(l);
                }
            }
            "--no-verify" => no_verify = true,
            "--help" | "-h" => {
                println!("用法: post_group --text T|-(stdin) [--session S] [--as M] [--mention M] [--chat-base U] [--todo-id I --todo-status open|doing|done --landing L] [--no-verify]");
                std::process::exit(0);
            }
            other => {
                eprintln!("[post_group] 不认识的参数：{other}");
                std::process::exit(2);
            }
        }
    }
    let rc = run(
        &chat_base,
        &session,
        &agent_id,
        text.as_deref(),
        &mention,
        &todo_id,
        &todo_status,
        &landing,
        no_verify,
    );
    std::process::exit(rc);
}

fn run(
    chat_base: &str,
    session: &str,
    agent_id: &str,
    text_arg: Option<&str>,
    mention: &[String],
    todo_id: &str,
    todo_status: &str,
    landing: &[String],
    no_verify: bool,
) -> i32 {
    let text = if text_arg == Some("-") {
        let mut buf = String::new();
        let _ = std::io::stdin().read_to_string(&mut buf);
        buf
    } else {
        text_arg.unwrap_or("").to_string()
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        println!("[post_group] 正文为空，退出");
        return 2;
    }
    let Some(sid) = pick_session(chat_base, session) else {
        println!("[post_group] 拿不到任何 open 的临时群，退出");
        return 3;
    };
    // 成员名册（过滤 @ 提及：后端对不存在的成员会 422 拒掉整条消息）
    let mut members: Vec<String> = Vec::new();
    let (_, d) = http(
        "GET",
        &format!("{chat_base}/api/sessions/{sid}/messages?limit=1"),
        None,
        Some(agent_id),
    );
    if let Some(msgs) = d["messages"].as_array() {
        for m in msgs {
            if let Some(id) = m["author"]["id"].as_str() {
                members.push(id.to_string());
            }
        }
    }
    let (_, sess) = http(
        "GET",
        &format!("{chat_base}/api/sessions?limit=100"),
        None,
        None,
    );
    if let Some(sessions) = sess["sessions"].as_array() {
        for s in sessions {
            if s["session_id"].as_str() == Some(&sid) {
                if let Some(ms) = s["members"].as_array() {
                    for m in ms {
                        let mid = m
                            .as_str()
                            .map(String::from)
                            .or_else(|| m["id"].as_str().map(String::from));
                        if let Some(mid) = mid {
                            members.push(mid);
                        }
                    }
                }
            }
        }
    }
    // mentions：--mention 优先 + 正文里的 @token（@所有人 → @all）；非法成员过滤
    let mut mentions: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut tokens: Vec<String> = mention.to_vec();
    tokens.extend(extract_mentions(&text));
    for tok in tokens {
        let tid = if tok == "所有人" {
            "@all".to_string()
        } else {
            tok
        };
        if tid != "@all" && !members.is_empty() && !members.contains(&tid) {
            continue;
        }
        if seen.contains(&tid) {
            continue;
        }
        seen.push(tid.clone());
        mentions.push(tid);
    }
    let mut payload = json!({"text": text, "mentions": mentions});
    if !todo_id.is_empty() && !todo_status.is_empty() {
        let mut decl = json!({"id": todo_id, "status": todo_status});
        if !landing.is_empty() {
            decl["landing"] = json!(landing);
        }
        payload["todo"] = decl;
    } else if !todo_id.is_empty() || !todo_status.is_empty() {
        println!("[post_group] --todo-id 与 --todo-status 要么都给、要么都不给，退出");
        return 2;
    }
    let (status, posted) = http(
        "POST",
        &format!("{chat_base}/api/sessions/{sid}/messages"),
        Some(&payload),
        Some(agent_id),
    );
    let ok = (200..300).contains(&status)
        && posted.get("ok").and_then(Value::as_bool).unwrap_or(true)
        && posted.get("error").is_none();
    if !ok {
        println!("[post_group] 写回群失败: {posted}");
        return 4;
    }
    println!(
        "[post_group] 已发回群 {sid} as {agent_id} ({} 字) mentions={}",
        text.chars().count(),
        if mentions.is_empty() {
            "-".to_string()
        } else {
            format!("{mentions:?}")
        }
    );
    if posted.get("queued").map(|v| !v.is_null()).unwrap_or(false) {
        println!("[post_group] 队列自动入队: {}", posted["queued"]);
    }
    if posted.get("todo").map(|v| !v.is_null()).unwrap_or(false) {
        println!(
            "[post_group] 队列状态已翻: ({}, {})",
            posted["todo"]["id"], posted["todo"]["status"]
        );
    }
    if no_verify {
        return 0;
    }
    // 回读校验：确认自己这条真的落库了（发送成功 != 送达）
    let (_, d) = http(
        "GET",
        &format!("{chat_base}/api/sessions/{sid}/messages?limit=10"),
        None,
        Some(agent_id),
    );
    let head40 = text.chars().take(40).collect::<String>();
    let hit = d["messages"]
        .as_array()
        .map(|tail| {
            tail.iter()
                .filter(|m| {
                    m["author"]["id"].as_str() == Some(agent_id)
                        && m["text"]
                            .as_str()
                            .unwrap_or("")
                            .trim()
                            .chars()
                            .take(40)
                            .collect::<String>()
                            == head40
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(last) = hit.last() {
        println!(
            "[post_group] 回读校验通过：已在群里看到自己的消息 seq={}",
            last["seq"]
        );
    } else {
        println!("[post_group] 回读校验未命中（可能只是列表还没刷到），已发但请人工确认");
    }
    0
}

/// 正文里的 @token（Python `re.findall(r"@([A-Za-z0-9_.\-]+|所有人)", text)` 手写对应物）。
fn extract_mentions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' {
            let start = i + 1;
            let mut j = start;
            while j < chars.len()
                && (chars[j].is_ascii_alphanumeric()
                    || chars[j] == '_'
                    || chars[j] == '.'
                    || chars[j] == '-'
                    || chars[j] == '所'
                    || chars[j] == '有'
                    || chars[j] == '人')
            {
                j += 1;
            }
            if j > start {
                out.push(chars[start..j].iter().collect());
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}
