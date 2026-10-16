//! X 横切：**错误文本脱敏**。对应 Python `hub/x/view_auth.py` 的 `safe_err()`。
//!
//! 口径（与 `blackboard_svc` 同一实现）：异常 → 可以安全打印的一行字，
//! 抹掉 DSN 里的口令类字段，再截断到 400 字符。**脱敏在出口处做，日志自身不得成为泄漏源。**

/// Python 侧正则 `(password|passfile|sslpassword)\s*=\s*\S+` 的关键词表。
/// 顺序与 Python 的交替顺序一致（`password` 在前，`passfile` 次之，`sslpassword` 最后）——
/// 交替在正则里是**首个能匹配的分支优先**，顺序是契约的一部分。
pub const SECRET_KEYWORDS: &[&str] = &["password", "passfile", "sslpassword"];

/// 截断上限（字符数，不是字节数 —— Python 切片按字符）。
pub const MAX_LEN: usize = 400;

/// 异常/错误文本 → 可以安全打印的一行字。
///
/// - 换行折成空格、去首尾空白；
/// - `(password|passfile|sslpassword)\s*=\s*\S+` → `<关键词原样>=***`（保留原大小写，同 Python `\1`）；
/// - 截断到 400 字符。
pub fn safe_err(text: &str) -> String {
    safe_err_with(text, "Error")
}

/// 同 [`safe_err`]，但文本为空时用 `fallback`（对应 Python `exc.__class__.__name__`）。
pub fn safe_err_with(text: &str, fallback: &str) -> String {
    // 口径同 Python：`str(exc).replace("\n", " ").strip()` —— 只折换行并去首尾空白，
    // **不**折叠中间的空白串。
    let one_line = text.replace('\n', " ");
    let trimmed = one_line.trim();
    let base = if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    };
    redact_secret_assignments(&base)
        .chars()
        .take(MAX_LEN)
        .collect()
}

/// 抹掉 `关键词 = 值` 形态的赋值（关键词 ASCII 大小写不敏感）。
fn redact_secret_assignments(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    'outer: while i < chars.len() {
        for kw in SECRET_KEYWORDS {
            let kw_chars: Vec<char> = kw.chars().collect();
            if !matches_keyword(&chars, i, &kw_chars) {
                continue;
            }
            // `\s*` → `=` → `\s*` → `\S+`
            let mut j = i + kw_chars.len();
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if chars.get(j) != Some(&'=') {
                continue;
            }
            j += 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            let value_start = j;
            while j < chars.len() && !chars[j].is_whitespace() {
                j += 1;
            }
            if j == value_start {
                continue; // `\S+` 要求至少一个字符，否则该分支不算命中
            }
            // 关键词原样保留（Python `\1` 是捕获到的原文），值换成 ***
            out.extend(&chars[i..i + kw_chars.len()]);
            out.push_str("=***");
            i = j;
            continue 'outer;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn matches_keyword(chars: &[char], at: usize, keyword: &[char]) -> bool {
    if at + keyword.len() > chars.len() {
        return false;
    }
    keyword
        .iter()
        .enumerate()
        .all(|(offset, k)| chars[at + offset].to_ascii_lowercase() == *k)
}
