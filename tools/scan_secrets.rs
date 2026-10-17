//! `scan_secrets` —— 提交前密钥扫描硬门禁（对应 Python tools/scan_secrets.py）。
//!
//! 两路一起扫：①已知凭证指纹（token 前缀/私钥头/连接串）②「键名像口令 + 值是字面量」。
//! 只报文件名+行号+种类掩码，**不回显命中原文**。退出码 0 干净 / 1 命中 / 2 路径不存在。
//!
//! regex-free 铁律：全部手写扫描器（与 Python re 口径逐条对齐，见各 fn 注释）。
#![allow(clippy::too_many_arguments)]

const SKIP_DIRS: &[&str] = &[
    ".git", "node_modules", "__pycache__", ".venv", "venv", "dist", "build", ".pytest_cache",
    "target",
];
const SKIP_EXT: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".ico", ".pdf", ".zip", ".gz", ".tgz", ".7z",
    ".whl", ".bin", ".gguf", ".safetensors", ".onnx", ".mp4", ".mov", ".woff", ".woff2", ".ttf",
    ".eot", ".so", ".dll", ".exe", ".pyc", ".sqlite", ".db", ".rs.bk",
];
const MAX_BYTES: u64 = 2 * 1024 * 1024;
const ALLOW_PATHS: &[&str] = &["tools/scan_secrets.rs", "tools/scan_secrets.py", ".env.example", "docs/"];
const FIXTURE_MARK: &str = "hacs-test-fixtures";

// ======================================================== 路 1：凭证指纹（手写匹配）

/// 各指纹逐一手写；返回 Some(label)。与 Python FINGERPRINTS 一一对应。
fn fingerprint(line: &str) -> Option<&'static str> {
    for tok in tokens(line) {
        // github-token: gh[pousr]_[A-Za-z0-9]{16,} | github_pat_[A-Za-z0-9_]{20,}
        if tok.starts_with("ghp_")
            || tok.starts_with("gho_")
            || tok.starts_with("ghu_")
            || tok.starts_with("ghs_")
            || tok.starts_with("ghr_")
        {
            let rest = &tok[4..];
            if rest.len() >= 16 && rest.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Some("github-token");
            }
        }
        if let Some(rest) = tok.strip_prefix("github_pat_") {
            if rest.len() >= 20 && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Some("github-token");
            }
        }
        // aws: (AKIA|LTAI)[A-Za-z0-9]{12,}
        if (tok.starts_with("AKIA") || tok.starts_with("LTAI"))
            && tok.len() >= 16
            && tok[4..].chars().all(|c| c.is_ascii_alphanumeric())
        {
            return Some("aws-access-key");
        }
        // private-key 头
        if let Some(b) = line.find("-----BEGIN ") {
            let after = &line[b + 11..];
            if let Some(e) = after.find("PRIVATE KEY-----") {
                if after[..e].chars().all(|c| c.is_ascii_uppercase() || c == ' ') {
                    return Some("private-key");
                }
            }
        }
        // feishu: cli_[a-z0-9]{16,}
        if let Some(rest) = tok.strip_prefix("cli_") {
            if rest.len() >= 16 && rest.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            {
                return Some("feishu-app-secret");
            }
        }
        // slack: xox[baprs]-[A-Za-z0-9-]{10,}
        if (tok.starts_with("xoxb-")
            || tok.starts_with("xoxa-")
            || tok.starts_with("xoxp-")
            || tok.starts_with("xoxr-")
            || tok.starts_with("xoxs-"))
            && tok.len() >= 15
        {
            return Some("slack-token");
        }
        // openai: sk-[A-Za-z0-9]{20,}（anthropic: sk-ant- 前缀优先）
        if tok.starts_with("sk-ant-") && tok.len() >= 27 {
            return Some("anthropic-key");
        }
        if tok.starts_with("sk-") && tok.len() >= 23 && tok[3..].chars().all(|c| c.is_ascii_alphanumeric())
        {
            return Some("openai-key");
        }
        // jwt: eyJ + 两段 . 分隔
        if tok.starts_with("eyJ") && tok.matches('.').count() >= 2 && tok.len() >= 32 {
            return Some("jwt");
        }
    }
    // pg-dsn-with-password / url-basic-auth：scheme://user:pass@
    if let Some(idx) = line.find("://") {
        let after = &line[idx + 3..];
        if let Some(at) = after.find('@') {
            let cred = &after[..at];
            if cred.contains(':')
                && !cred.contains('<')
                && !cred.chars().all(|c| c.is_ascii_alphanumeric())
            {
                let scheme = &line[..idx];
                if scheme.starts_with("postgres") {
                    return Some("pg-dsn-with-password");
                }
                if scheme == "http" || scheme == "https" {
                    return Some("url-basic-auth");
                }
            }
        }
    }
    // bearer-literal: authorization: bearer <16+ 字面量>
    let low = line.to_lowercase();
    if let Some(idx) = low.find("authorization") {
        let rest = &low[idx..];
        if let Some(bidx) = rest.find("bearer") {
            let val = rest[bidx + 6..].trim();
            if val.len() >= 16
                && val
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
            {
                return Some("bearer-literal");
            }
        }
    }
    None
}

fn tokens(line: &str) -> Vec<String> {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'))
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

// ======================================================== 路 2：赋值即口令

/// SECRETISH：`键名 [:=] 值`（键名含口令词根）。
fn secretish(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    let idx = match trimmed.find(':').or_else(|| trimmed.find('=')) {
        Some(i) => i,
        None => return None,
    };
    let (key, rest) = trimmed.split_at(idx);
    let value = rest[1..].trim();
    if key.is_empty() || value.is_empty() {
        return None;
    }
    // 键名只允许 [a-z0-9_]
    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some((key.to_lowercase(), value.trim_end_matches(',').to_string()))
}

/// KEY_HINT：独立词根（词边界）。`CTX_MIN_TOKENS_FLOOR` 的 TOKENS 不算，`api_key` 要拦。
fn key_hint(key: &str) -> bool {
    const WORDS: [&str; 9] = [
        "pass", "passwd", "password", "secret", "token", "api_key", "access_key", "credential",
        "private_key",
    ];
    let parts: Vec<&str> = key.split('_').collect();
    // 形态一：整键 == 词根（passwd/password/secret/token）
    const EXACT: [&str; 5] = ["pass", "passwd", "password", "secret", "token"];
    if EXACT.contains(&key) {
        return true;
    }
    // 形态二：api_key / access_key / private_key / credential / credential_key（下划线分段重组）
    for w in ["api", "access", "private"] {
        if key == format!("{w}_key") {
            return true;
        }
    }
    if key == "credential" || key == "credentials" {
        return true;
    }
    // 形态三：词根作为独立段出现在键中且不是纯后缀词（tokens 不算）
    if parts.iter().any(|p| *p == "secret" || *p == "passwd" || *p == "password") {
        return true;
    }
    if parts.iter().any(|p| *p == "token") && !parts.iter().any(|p| *p == "tokens") {
        return true;
    }
    let _ = WORDS;
    false
}

/// PLACEHOLDER：值是占位（空串/${ENV}/$VAR/尖括号/xxx/todo/…/os.environ 读法）。
fn placeholder(value: &str) -> bool {
    let v = value.trim();
    let stripped = v.trim_matches(['"', '\'']).trim();
    if stripped.is_empty() {
        return true;
    }
    if stripped.starts_with("${") && stripped.ends_with('}') {
        return true;
    }
    if stripped.starts_with('$') {
        return true;
    }
    if stripped.starts_with('<') && stripped.ends_with('>') {
        return true;
    }
    let low = stripped.trim_matches(['"', '\'']).to_lowercase();
    for w in ["xxx", "todo", "fixme", "changeme", "placeholder", "example", "redacted", "none", "null", "true", "false"] {
        if low == w {
            return true;
        }
    }
    if low == "***" || low.chars().all(|c| c == '*') {
        return true;
    }
    // os.environ / getenv 读法（整行）
    let line_low = v.to_lowercase();
    if line_low.contains("os.environ") || line_low.contains("getenv(") || line_low.contains("environ[") {
        return true;
    }
    false
}

/// NUMERICISH：数字（含引号/逗号尾）。
fn numericish(value: &str) -> bool {
    let v = value.trim().trim_end_matches(',').trim().trim_matches(['"', '\'']);
    !v.is_empty() && v.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
        && v.replace(['.', ','], "").chars().all(|c| c.is_ascii_digit())
}

/// CONSTNAME：全大写常量名（引用另一个变量，非硬编码）。
fn constname(value: &str) -> bool {
    let v = value.trim();
    v.len() >= 3
        && v.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false)
        && v.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// LITERALISH：值是字面量形态（引号串/裸标识符/数字开头）。
fn literalish(value: &str) -> bool {
    let v = value.trim();
    if (v.starts_with('"') && v.len() >= 2 && v.ends_with('"'))
        || (v.starts_with('\'') && v.len() >= 2 && v.ends_with('\''))
    {
        return true;
    }
    if v.chars().next().map(|c| c.is_ascii_alphabetic() || c == '_').unwrap_or(false)
        && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return true;
    }
    v.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
}

// ======================================================== 扫描主循环

fn is_allowed(rel: &str) -> bool {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    ALLOW_PATHS
        .iter()
        .any(|p| rel == *p || rel.starts_with(p) || base.ends_with(p))
}

fn mask(hit: &str) -> String {
    let hit = hit.trim();
    let n = hit.chars().count();
    if n <= 6 {
        return "*".repeat(n);
    }
    let head: String = hit.chars().take(3).collect();
    let tail: String = hit.chars().skip(n - 2).collect();
    format!("{head}…{tail}（len={n}）")
}

struct Finding {
    rel: String,
    line: usize,
    label: String,
    masked: String,
}

fn walk(root: &str) -> Vec<(std::path::PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(root)];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(p);
                }
            } else {
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((p, rel));
            }
        }
    }
    out
}

fn scan(root: &str) -> (Vec<Finding>, usize, usize) {
    let mut findings = Vec::new();
    let mut scanned = 0usize;
    let mut skipped = 0usize;
    for (full, rel) in walk(root) {
        let ext = full
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        if SKIP_EXT.contains(&ext.as_str()) {
            skipped += 1;
            continue;
        }
        let Ok(meta) = std::fs::metadata(&full) else {
            skipped += 1;
            continue;
        };
        if meta.len() > MAX_BYTES {
            skipped += 1;
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&full) else {
            skipped += 1;
            continue;
        };
        scanned += 1;
        let fixture = text.contains(FIXTURE_MARK);
        let allowed = is_allowed(&rel);
        for (idx, line) in text.lines().enumerate() {
            let lineno = idx + 1;
            match fingerprint(line) {
                Some(label) => {
                    // 允许清单（样板）：尖括号占位不算
                    if allowed && (line.contains('<') || placeholder(line)) {
                        continue;
                    }
                    findings.push(Finding {
                        rel: rel.clone(),
                        line: lineno,
                        label: label.to_string(),
                        masked: mask(line),
                    });
                }
                None => {
                    if allowed || fixture {
                        continue;
                    }
                    if let Some((key, value)) = secretish(line) {
                        if key_hint(&key) {
                            let value = strip_comment(&value);
                            if !placeholder(&value)
                                && !numericish(&value)
                                && !constname(&value)
                                && literalish(&value)
                                // 值是无引号裸标识符且整行无引号 → 变量/类型引用
                                // （Rust 字段初始化 `token: String,`），非硬编码凭据。
                                // Python 树无此形态（字段带类型注解），口径差异登记 PARITY。
                                && !(value.trim_end_matches(',').trim().chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                                    && !line.contains('"')
                                    && !line.contains('\''))
                            {
                                findings.push(Finding {
                                    rel: rel.clone(),
                                    line: lineno,
                                    label: format!("literal-assignment:{key}"),
                                    masked: mask(&value),
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    (findings, scanned, skipped)
}

fn strip_comment(value: &str) -> String {
    // 剥行尾注释（` # ...`）—— 引号外的第一个 #；
    // 简化：若值以引号开头，找闭合引号后的 #；否则找第一个 #
    let v = value.trim();
    if let Some(rest) = v.strip_prefix('"') {
        if let Some(end) = rest.find('"') {
            let after = &rest[end + 1..];
            let cut = after.find('#').map(|i| i + 1).unwrap_or(after.len());
            return format!("\"{}{}", &rest[..end], &after[..cut]).trim_end().to_string();
        }
    }
    match v.find(" #") {
        Some(i) => v[..i].trim().to_string(),
        None => v.to_string(),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = args.get(1).map(String::as_str).unwrap_or(".");
    if !std::path::Path::new(root).exists() {
        println!("路径不存在：{root}");
        std::process::exit(2);
    }
    let (findings, scanned, skipped) = scan(root);
    println!("扫描根：{}", std::fs::canonicalize(root).unwrap_or_else(|_| root.into()).display());
    println!("已扫文本文件：{scanned} 个；按扩展名/体积跳过：{skipped} 个");
    if findings.is_empty() {
        println!("结果：干净，可提交。");
        std::process::exit(0);
    }
    println!("结果：命中 {} 处，禁止提交。", findings.len());
    for f in &findings {
        println!("  {}:{}  [{}]  {}", f.rel, f.line, f.label, f.masked);
    }
    println!("处置：改 ${{ENV}} 占位 / 移出仓 / 加 .gitignore 后重跑。");
    std::process::exit(1);
}
