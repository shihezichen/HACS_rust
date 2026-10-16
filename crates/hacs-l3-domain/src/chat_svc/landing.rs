//! 落点（landing）校验与归一（Python `validate_todo_evidence` / `landing_kind_of` /
//! `normalize_landing`）。四类机器可验落点：commit / curl / restart / file。

use super::RequestError;
use crate::chat_svc::{TODO_EVIDENCE_TYPES, TODO_LANDING_MAX, TODO_LANDING_MAXLEN};

/// `\d{4}-\d{2}-\d{2}([ T]\d{2}:\d{2}(:\d{2})?)?` 时间戳形状。
pub fn has_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut i = 0;
    while i + 10 <= bytes.len() {
        // \d{4}-\d{2}-\d{2}
        if bytes[i].is_ascii_digit()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
            && bytes[i + 4] == b'-'
            && bytes[i + 5].is_ascii_digit()
            && bytes[i + 6].is_ascii_digit()
            && bytes[i + 7] == b'-'
            && bytes[i + 8].is_ascii_digit()
            && bytes[i + 9].is_ascii_digit()
        {
            // 之后的可选时间部分必须形状正确（否则这是日期号中间子串，Python \d{4} 从
            // 任意位置起配但后续字符限定，本实现按「完整起点」扫 —— 与 search 语义一致）
            let mut j = i + 10;
            if j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'T') {
                j += 1;
                // \d{2}:\d{2}
                if j + 5 <= bytes.len()
                    && bytes[j].is_ascii_digit()
                    && bytes[j + 1].is_ascii_digit()
                    && bytes[j + 2] == b':'
                    && bytes[j + 3].is_ascii_digit()
                    && bytes[j + 4].is_ascii_digit()
                {
                    j += 5;
                    if j + 3 <= bytes.len() && bytes[j] == b':' {
                        let k = j + 1;
                        if k + 2 <= bytes.len()
                            && bytes[k].is_ascii_digit()
                            && bytes[k + 1].is_ascii_digit()
                        {
                            j = k + 2;
                        }
                    }
                    let _ = j;
                }
            }
            return true;
        }
        i += 1;
    }
    false
}

/// `\b[1-5]\d{2}\b` HTTP 状态码形状。
pub fn has_http_code(value: &str) -> bool {
    let bytes = value.as_bytes();
    for i in 0..bytes.len().saturating_sub(2) {
        let c0 = bytes[i];
        if !(b'1'..=b'5').contains(&c0) {
            continue;
        }
        if !bytes[i + 1].is_ascii_digit() || !bytes[i + 2].is_ascii_digit() {
            continue;
        }
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        let after = i + 3;
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

/// `(?i)\bpid\b\s*[:=]?\s*\d+`。
pub fn has_pid(value: &str) -> bool {
    let lower = value.to_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if !lower.is_char_boundary(i) {
            i += 1;
            continue;
        }
        if &lower[i..i + 3] == "pid" {
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            let after = i + 3;
            let mut j = after;
            while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b':' || bytes[j] == b'=') {
                j += 1;
                while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                    j += 1;
                }
            }
            let digits = j < bytes.len() && bytes[j].is_ascii_digit();
            if before_ok && digits {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// `^[0-9a-f]{7,40}$`。
pub fn is_commit(value: &str) -> bool {
    let b = value.as_bytes();
    (7..=40).contains(&b.len()) && b.iter().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// `^commit\s*[:：=#]?\s*([0-9a-f]{7,40})$`（宽松：允许前缀写法）。
fn commit_prefixed(value: &str) -> Option<String> {
    let rest = value.strip_prefix("commit")?;
    let rest = rest.trim_start();
    let rest = rest.trim_start_matches([':', '：', '=', '#']).trim_start();
    if is_commit(rest) {
        Some(rest.to_string())
    } else {
        None
    }
}

/// `curl|\b(GET|POST|PUT|PATCH|DELETE)\s+/`。
fn looks_like_curl(value: &str) -> bool {
    let lower = value.to_lowercase();
    if lower.contains("curl") {
        return true;
    }
    for m in ["get ", "post ", "put ", "patch ", "delete "] {
        // \b 前界：非字母数字
        let mut from = 0usize;
        while let Some(pos) = lower[from..].find(m) {
            let abs = from + pos;
            let before_ok = abs == 0
                || !(lower.as_bytes()[abs - 1].is_ascii_alphanumeric() || lower.as_bytes()[abs - 1] == b'_');
            if before_ok {
                let after = abs + m.len();
                if after < lower.len() && lower.as_bytes()[after] == b'/' {
                    return true;
                }
            }
            from = abs + 1;
        }
    }
    false
}

/// 文件路径形状（POSIX `/[\w./-]{2,}` 或 Windows 盘符）。
fn find_path(value: &str) -> Option<String> {
    // Windows 盘符
    let b = value.as_bytes();
    for i in 0..b.len().saturating_sub(2) {
        if b[i].is_ascii_alphabetic() && b[i + 1] == b':' && (b[i + 2] == b'\\' || b[i + 2] == b'/')
        {
            let mut j = i;
            while j < b.len() {
                let ch = value[j..].chars().next().unwrap();
                if matches!(ch, ' ' | '，' | ',' | '。' | '；' | ';' | '）' | ')' | '】' | ']') {
                    break;
                }
                j += ch.len_utf8();
            }
            return Some(value[i..j].to_string());
        }
    }
    // POSIX：/[word./-]{2,}
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'/' {
            let mut j = i;
            let mut n = 0usize;
            while j < b.len() {
                let c = b[j];
                if c.is_ascii_alphanumeric() || c == b'.' || c == b'/' || c == b'-' || c == b'_' {
                    j += 1;
                    n += 1;
                } else {
                    break;
                }
            }
            if n >= 2 {
                return Some(value[i..j].to_string());
            }
        }
        i += 1;
    }
    None
}

/// `landing_kind_of`：一条落点字符串 → (type, value)；不达标 None。
pub fn landing_kind_of(value: &str) -> Option<(String, String)> {
    let text = value.trim();
    if text.is_empty() {
        return None;
    }
    if is_commit(text) {
        return Some(("commit".into(), text.to_string()));
    }
    if let Some(c) = commit_prefixed(text) {
        return Some(("commit".into(), c));
    }
    if has_pid(text) && has_timestamp(text) {
        return Some(("restart".into(), text.to_string()));
    }
    if looks_like_curl(text) && has_http_code(text) {
        return Some(("curl".into(), text.to_string()));
    }
    if find_path(text).is_some() && has_timestamp(text) {
        return Some(("file".into(), text.to_string()));
    }
    None
}

/// `validate_todo_evidence`：四类逐类校验，不过 422（错误码/文案逐字对齐）。
pub fn validate_todo_evidence(raw: &serde_json::Value) -> Result<serde_json::Value, RequestError> {
    if !raw.is_object() {
        return Err(RequestError::new(422, "bad_evidence", "落点必须是对象 {type, value}"));
    }
    let kind = raw.get("type").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let value = raw.get("value").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if !TODO_EVIDENCE_TYPES.contains(&kind.as_str()) {
        return Err(RequestError::new(
            422,
            "bad_evidence",
            &format!("落点类型只能是 {}", TODO_EVIDENCE_TYPES.join("、")),
        ));
    }
    if value.is_empty() {
        return Err(RequestError::new(422, "bad_evidence", "落点 value 不能为空"));
    }
    match kind.as_str() {
        "commit" => {
            if !is_commit(&value) {
                return Err(RequestError::new(
                    422,
                    "bad_evidence",
                    "commit 落点需 7~40 位小写十六进制 commit 号",
                ));
            }
        }
        "curl" => {
            if !has_http_code(&value) {
                return Err(RequestError::new(422, "bad_evidence", "curl 落点需带 HTTP 状态码（如 200）"));
            }
        }
        "restart" => {
            if !(has_pid(&value) && has_timestamp(&value)) {
                return Err(RequestError::new(422, "bad_evidence", "重启落点需带 PID 与时间戳"));
            }
        }
        _ => {
            // file：按第一个 @（或 |）切路径
            let path = match value.find('@') {
                Some(i) => value[..i].to_string(),
                None => match value.find('|') {
                    Some(i) => value[..i].to_string(),
                    None => value.clone(),
                },
            };
            let path = path.trim().to_string();
            if path.is_empty() {
                return Err(RequestError::new(422, "bad_evidence", "file 落点需给路径"));
            }
            if path.starts_with('/') && !std::path::Path::new(&path).exists() {
                return Err(RequestError::new(
                    422,
                    "evidence_not_found",
                    &format!("落点文件不存在：{}", path),
                ));
            }
            if !has_timestamp(&value) {
                return Err(RequestError::new(
                    422,
                    "bad_evidence",
                    "file 落点需带时间戳（形如 /path/file @ 2026-09-22 12:00）",
                ));
            }
        }
    }
    let cut: String = value.chars().take(500).collect();
    Ok(serde_json::json!({"type": kind, "value": cut}))
}

/// `normalize_landing`：归一化落点入参 → (landing 字符串数组, evidences 结构化数组)。
pub fn normalize_landing(
    raw: Option<&serde_json::Value>,
) -> Result<(Vec<String>, Vec<serde_json::Value>), RequestError> {
    let raw = match raw {
        None | Some(serde_json::Value::Null) => return Ok((vec![], vec![])),
        Some(v) => v,
    };
    let list: Vec<serde_json::Value> = match raw {
        serde_json::Value::String(_) | serde_json::Value::Object(_) => vec![raw.clone()],
        serde_json::Value::Array(a) => a.clone(),
        _ => {
            return Err(RequestError::new(
                422,
                "bad_landing",
                "landing 需是字符串数组（或单个字符串）：commit 哈希 / 字面 curl+HTTP 码 / 文件路径+时间戳 / 重启 PID+时间戳",
            ))
        }
    };
    let list: Vec<serde_json::Value> = list
        .into_iter()
        .filter(|v| !matches!(v, serde_json::Value::Null))
        .filter(|v| !(v.is_string() && v.as_str().unwrap_or("").trim().is_empty()))
        .filter(|v| !(v.is_array() && v.as_array().map(|a| a.is_empty()).unwrap_or(false)))
        .filter(|v| !(v.is_object() && v.as_object().map(|o| o.is_empty()).unwrap_or(false)))
        .collect();
    if list.is_empty() {
        return Ok((vec![], vec![]));
    }
    if list.len() > TODO_LANDING_MAX {
        return Err(RequestError::new(
            422,
            "bad_landing",
            &format!("landing 一次最多 {} 条", TODO_LANDING_MAX),
        ));
    }
    let mut strings: Vec<String> = Vec::new();
    let mut evidences: Vec<serde_json::Value> = Vec::new();
    for entry in list {
        if let Some(obj) = entry.as_object() {
            let checked = validate_todo_evidence(&entry)?;
            let t = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let v = obj.get("value").and_then(|v| v.as_str()).unwrap_or("");
            strings.push(take_chars(&format!("{}: {}", t, v), TODO_LANDING_MAXLEN));
            evidences.push(checked);
            continue;
        }
        let text = entry.as_str().unwrap_or("").trim().to_string();
        if text.is_empty() {
            return Err(RequestError::new(422, "bad_landing", "landing 元素不能为空"));
        }
        let (kind, norm_value) = match landing_kind_of(&text) {
            Some(kv) => kv,
            None => {
                let preview: String = text.chars().take(80).collect();
                return Err(RequestError::new(
                    422,
                    "bad_landing",
                    &format!(
                        "landing 不是机器可验写法：{}（收 commit 哈希 / 字面 curl+HTTP 码 / 文件路径+时间戳 / 重启 PID+时间戳；「已上线」这类自述不算落点）",
                        preview
                    ),
                ));
            }
        };
        let checked = validate_todo_evidence(&serde_json::json!({"type": kind, "value": norm_value}))?;
        strings.push(take_chars(&text, TODO_LANDING_MAXLEN));
        evidences.push(checked);
    }
    Ok((strings, evidences))
}

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_landing_kind() {
        assert_eq!(landing_kind_of("abc1234"), Some(("commit".into(), "abc1234".into())));
        assert_eq!(
            landing_kind_of("commit: abc1234"),
            Some(("commit".into(), "abc1234".into()))
        );
        assert_eq!(landing_kind_of("ABC1234"), None); // 大写不算
        assert!(landing_kind_of("curl http://x/abc 200").is_some());
        assert!(landing_kind_of("POST /api/x 200").is_some());
        assert!(landing_kind_of("重启成功 pid=123 2026-09-22 12:00").is_some());
        assert!(landing_kind_of("/var/log/x.log @ 2026-09-22 12:00").is_some());
        // 自述不算落点
        assert_eq!(landing_kind_of("已上线"), None);
        assert_eq!(landing_kind_of("搞定了"), None);
        assert_eq!(landing_kind_of("/path/only no stamp"), None);
    }

    #[test]
    fn test_normalize() {
        let (s, e) = normalize_landing(Some(&serde_json::json!(["abc1234"]))).unwrap();
        assert_eq!(s, vec!["abc1234".to_string()]);
        assert_eq!(e[0]["type"], serde_json::json!("commit"));
        let (s, _) = normalize_landing(Some(&serde_json::json!("abc1234"))).unwrap();
        assert_eq!(s.len(), 1);
        let (s, e) = normalize_landing(None).unwrap();
        assert!(s.is_empty() && e.is_empty());
        // 自述拒收
        let err = normalize_landing(Some(&serde_json::json!(["已经上线了"]))).unwrap_err();
        assert_eq!(err.code, "bad_landing");
        assert_eq!(err.status, 422);
        // 超量拒收
        let nine = serde_json::json!(["abc1234", "abc1234", "abc1234", "abc1234", "abc1234", "abc1234", "abc1234", "abc1234", "abc1234"]);
        assert_eq!(normalize_landing(Some(&nine)).unwrap_err().code, "bad_landing");
    }

    #[test]
    fn test_validate_evidence_file_missing() {
        let err = validate_todo_evidence(&serde_json::json!(
            {"type": "file", "value": "/nonexistent/zzz.txt @ 2026-09-22 12:00"}
        ))
        .unwrap_err();
        assert_eq!(err.code, "evidence_not_found");
        let err = validate_todo_evidence(&serde_json::json!({"type": "file", "value": "/tmp"}))
            .unwrap_err();
        assert_eq!(err.code, "bad_evidence");
    }

    #[test]
    fn test_http_pid() {
        assert!(has_http_code("curl /x 200"));
        assert!(has_http_code("404 error"));
        assert!(!has_http_code("12345")); // 5 位连写不命中（\b 边界）
        assert!(has_pid("pid: 123"));
        assert!(has_pid("PID=456"));
        assert!(!has_pid("rapid 123"));
    }
}
