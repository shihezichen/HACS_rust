//! X 横切：看板认证（对应 Python `hub/x/view_auth.py`，会话令牌 / 角色密级 / 口令哈希 /
//! 暴力破解防护 / IP 工具）。**纯逻辑**：不连库、不碰 socket；存储与 CLI 归组合根。
//!
//! 与 Python 的逐条对应：
//!   * `hash_password` / `verify_password` —— scrypt（n=16384, r=8, p=1, dklen=32），
//!     输出串 `scrypt$n$r$p$<salt_b64>$<dk_b64>` 逐字节同形；
//!   * `sign_token` / `unsign_token` —— HMAC-SHA256（purpose 域分隔）+ `v1.<token>.<sig>`；
//!   * `token_fingerprint` —— sha256(token) hex（库里只存指纹）；
//!   * `issue_csrf` / `check_csrf` —— double-submit + 同一套签名（purpose=b"csrf"）；
//!   * `role_can_see` / `normalize_role` / `normalize_sensitivity` —— 角色密级（收紧兜底）；
//!   * `in_tailnet` —— 只认对端 IP（`HACS_TAILNET_CIDR` 覆盖，默认 100.64.0.0/10）；
//!   * `LockTracker` —— 同 IP 5 分钟 5 败锁 15 分钟（内存 + 表双记，取较晚）。
//!
//! 依赖口径：HMAC 与 SHA-256 自实现（不引 crate，与 `a2a_edge::sha256_hex` 同源语义）；
//! scrypt 也自实现（见 `scrypt_dk`，PBKDF2-HMAC-SHA256 + salsa20/8 + ROMix）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const SESSION_COOKIE: &str = "bbview_session";
pub const CSRF_COOKIE: &str = "bbview_csrf";
pub const SESSION_TTL_S: i64 = 12 * 3600;
pub const SESSION_RENEW_S: i64 = 6 * 3600;
pub const TOKEN_BYTES: usize = 32;
pub const KEY_BYTES: usize = 32;

pub const FAIL_WINDOW_S: i64 = 300;
pub const FAIL_MAX: usize = 5;
pub const LOCK_S: i64 = 900;

pub const PASSWORD_MIN: usize = 8;
pub const PASSWORD_MAX: usize = 256;
pub const SALT_BYTES: usize = 16;
pub const SCRYPT_N: u32 = 16384;
pub const SCRYPT_R: u32 = 8;
pub const SCRYPT_P: u32 = 1;
pub const SCRYPT_DKLEN: usize = 32;
pub const SCRYPT_MAXMEM: usize = 64 * 1024 * 1024;

pub const KEY_ENV: &str = "BB_VIEW_SESSION_KEY";

pub fn role_cn(role: &str) -> &'static str {
    match role {
        "viewer" => "只读访客",
        "member" => "成员",
        "owner" => "主人",
        _ => "-",
    }
}

/// 角色等级：认不出来的按 viewer（0）算 —— 往收紧了兜。
pub fn normalize_role(role: &str) -> &'static str {
    match role {
        "member" => "member",
        "owner" => "owner",
        _ => "viewer",
    }
}

fn role_rank(role: &str) -> u8 {
    match normalize_role(role) {
        "owner" => 2,
        "member" => 1,
        _ => 0,
    }
}

/// 密级：认不出来的按 internal（1）算。
pub fn normalize_sensitivity(value: &str) -> &'static str {
    match value.trim().to_ascii_lowercase().as_str() {
        "public" => "public",
        "internal" => "internal",
        "sensitive" => "sensitive",
        _ => "internal",
    }
}

fn sensitivity_rank(value: &str) -> u8 {
    match normalize_sensitivity(value) {
        "public" => 0,
        "sensitive" => 2,
        _ => 1,
    }
}

pub const DEFAULT_SENSITIVITY: &str = "internal";

/// 角色能不能看这个密级：viewer 只看 public；member 看到 internal；owner 全能看。
pub fn role_can_see(role: &str, sensitivity: &str) -> bool {
    role_rank(role) >= sensitivity_rank(sensitivity)
}

// ------------------------------------------------------------------ tailnet

/// tailnet 网段：`HACS_TAILNET_CIDR` 覆盖，占位符 / 未配置退 100.64.0.0/10。
pub fn tailnet_cidr() -> String {
    let raw = std::env::var("HACS_TAILNET_CIDR").unwrap_or_default();
    if raw.starts_with('<') || raw.trim().is_empty() {
        "100.64.0.0/10".to_string()
    } else {
        raw.trim().to_string()
    }
}

/// 解析 IP 字面量（含 `[::1]` 方括号、`%scope`、`::ffff:` v4 映射归一）。
/// 返回 `("4", octets)` / `("6", segments)` 的形状描述用于网段匹配。
pub fn parse_ip(raw: &str) -> Option<IpAddr> {
    let mut text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if text.starts_with('[') && text.ends_with(']') {
        text = &text[1..text.len() - 1];
    }
    if let Some(idx) = text.find('%') {
        text = &text[..idx];
    }
    if let Some(rest) = text.strip_prefix("::ffff:") {
        if let Some(v4) = parse_v4(rest) {
            return Some(IpAddr::V4(v4));
        }
    }
    if let Some(v4) = parse_v4(text) {
        return Some(IpAddr::V4(v4));
    }
    parse_v6(text).map(IpAddr::V6)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpAddr {
    V4([u8; 4]),
    V6([u16; 8]),
}

fn parse_v4(text: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut out = [0u8; 4];
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        out[i] = p.parse::<u8>().ok()?;
    }
    Some(out)
}

fn parse_v6(text: &str) -> Option<[u16; 8]> {
    if text.matches("::").count() > 1 {
        return None;
    }
    let (head, tail) = match text.split_once("::") {
        Some((h, t)) => (h, Some(t)),
        None => (text, None),
    };
    let parse_group = |s: &str| -> Option<Vec<u16>> {
        if s.is_empty() {
            return Some(Vec::new());
        }
        s.split(':')
            .map(|g| {
                if g.is_empty() || g.len() > 4 || !g.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return None;
                }
                u16::from_str_radix(g, 16).ok()
            })
            .collect()
    };
    let mut groups: Vec<u16> = parse_group(head)?;
    let tail_groups = match tail {
        Some(t) => parse_group(t)?,
        None => Vec::new(),
    };
    if tail.is_none() && groups.len() != 8 {
        return None;
    }
    let fill = 8usize.checked_sub(groups.len() + tail_groups.len())?;
    groups.extend(std::iter::repeat(0).take(fill));
    groups.extend(tail_groups);
    if groups.len() != 8 {
        return None;
    }
    let mut out = [0u16; 8];
    out.copy_from_slice(&groups);
    Some(out)
}

fn v4_in_net(ip: [u8; 4], net: [u8; 4], prefix: u8) -> bool {
    if prefix > 32 {
        return false;
    }
    let shift = 32 - prefix as u32;
    if shift == 0 {
        return true;
    }
    let a = u32::from_be_bytes(ip) >> shift;
    let b = u32::from_be_bytes(net) >> shift;
    a == b
}

/// 对端 IP 在不在 tailnet 段里。只认对端 IP —— 不看任何请求头。
pub fn in_tailnet(ip: &str) -> bool {
    let Some(addr) = parse_ip(ip) else {
        return false;
    };
    let cidr = tailnet_cidr();
    let Some((net, prefix)) = cidr.split_once('/') else {
        return false;
    };
    let Ok(prefix) = prefix.parse::<u8>() else {
        return false;
    };
    match (addr, parse_v4(net)) {
        (IpAddr::V4(o), Some(net)) => v4_in_net(o, net, prefix),
        // Python 原件同样只收 IPv4（`addr.version == 4 and addr in _TAILNET_NET`）
        _ => false,
    }
}

pub fn is_loopback(ip: &str) -> bool {
    match parse_ip(ip) {
        Some(IpAddr::V4(o)) => o[0] == 127,
        Some(IpAddr::V6(s)) => {
            s.iter().take(5).all(|&g| g == 0) && s[5] == 0 && s[6] == 0 && s[7] == 1
        }
        None => false,
    }
}

// ------------------------------------------------------------------ 时间口径

pub fn utcnow() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// Unix 秒（UTC）→ `YYYY-MM-DDTHH:MM:SSZ`（Python `iso()` 的形态）。
pub fn iso_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 给人看的时间一律北京时间（UTC+8）：`YYYY-MM-DD HH:MM:SS`。
pub fn beijing(value: &str) -> String {
    match parse_ts(value) {
        Some(secs) => {
            let cst = secs + 8 * 3600;
            let days = cst.div_euclid(86_400);
            let rem = cst.rem_euclid(86_400);
            let (y, m, d) = civil_from_days(days);
            format!(
                "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
                rem / 3600,
                (rem % 3600) / 60,
                rem % 60
            )
        }
        None => "-".to_string(),
    }
}

/// ISO 文本 → Unix 秒（UTC）。`Z` 与 `±HH:MM` 都认；无时区当 UTC（Python 行为）。
pub fn parse_ts(value: &str) -> Option<i64> {
    let text = value.trim();
    if text.is_empty() {
        return None;
    }
    let b = text.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { text.get(a..z)?.parse::<i64>().ok() };
    let y = num(0, 4)?;
    let mo = num(5, 7)?;
    let d = num(8, 10)?;
    let h = num(11, 13)?;
    let mi = num(14, 16)?;
    let se = num(17, 19)?;
    if !(b[4] == b'-'
        && b[7] == b'-'
        && (b[10] == b'T' || b[10] == b' ')
        && b[13] == b':'
        && b[16] == b':')
    {
        return None;
    }
    let mut rest = &text[19..];
    // 小数秒：Python fromisoformat 接受任意精度，这里吃掉（秒级时间戳丢亚秒）
    if rest.starts_with('.') {
        let end = rest[1..]
            .find(|c: char| !c.is_ascii_digit())
            .map(|i| i + 1)
            .unwrap_or(rest.len());
        // 亚秒直接吃掉（秒级时间戳丢亚秒，只用于比较层面）
        rest = &rest[end..];
    }
    let offset = if rest == "Z" || rest == "z" || rest.is_empty() {
        0
    } else if rest.len() >= 6
        && (rest.starts_with('+') || rest.starts_with('-'))
        && rest.as_bytes()[3] == b':'
    {
        let sign: i64 = if rest.starts_with('-') { -1 } else { 1 };
        let oh: i64 = rest[1..3].parse().ok()?;
        let om: i64 = rest[4..6].parse().ok()?;
        sign * (oh * 3600 + om * 60)
    } else {
        return None;
    };
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - offset)
}

// ------------------------------------------------------------------ base64 / 随机

const B64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn b64url(raw: &[u8]) -> String {
    let mut out = String::new();
    for chunk in raw.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_URL[(n >> 18) as usize & 63] as char);
        out.push(B64_URL[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(B64_URL[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(B64_URL[n as usize & 63] as char);
        }
    }
    out // 无 padding（Python `rstrip("=")`）
}

fn b64_val(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a') as u32 + 26),
        b'0'..=b'9' => Some((c - b'0') as u32 + 52),
        b'-' | b'+' => Some(62),
        b'_' | b'/' => Some(63),
        _ => None,
    }
}

pub fn unb64(text: &str) -> Option<Vec<u8>> {
    let mut vals = Vec::with_capacity(text.len());
    for &c in text.as_bytes() {
        if c == b'=' {
            break;
        }
        vals.push(b64_val(c)?);
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4 + 3);
    for chunk in vals.chunks(4) {
        let mut n = chunk[0] << 18;
        for (i, &v) in chunk.iter().enumerate().skip(1) {
            n |= v << (18 - 6 * i);
        }
        // 每个输入字符贡献 6 bit；去掉最后不足 8 bit 的余数
        let nbytes = chunk.len() - 1;
        for k in 0..nbytes {
            out.push(((n >> (16 - 8 * k)) & 0xff) as u8);
        }
    }
    Some(out)
}

/// `/dev/urandom` 随机（对齐项目 uuid4 口径，不引 rand crate）。
pub fn urandom(n: usize) -> Vec<u8> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("read /dev/urandom");
    buf
}

/// `secrets.token_urlsafe(n)`：n 字节随机 → base64url（无 padding）。
pub fn token_urlsafe(n: usize) -> String {
    b64url(&urandom(n))
}

// ------------------------------------------------------------------ SHA-256 / HMAC

/// SHA-256（零依赖自实现，与 Python hashlib 逐字节一致）。
pub fn sha256(data: &[u8]) -> [u8; 32] {
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
        for (i, v) in [a, b, c, d, e, f, g, hh].iter().enumerate() {
            h[i] = h[i].wrapping_add(*v);
        }
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

pub fn sha256_hex(data: &[u8]) -> String {
    sha256(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// HMAC-SHA256（RFC 2104，块长 64）。
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = ipad.to_vec();
    inner.extend_from_slice(msg);
    let ih = sha256(&inner);
    let mut outer = opad.to_vec();
    outer.extend_from_slice(&ih);
    sha256(&outer)
}

/// 恒时比较（Python `hmac.compare_digest`）。
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ------------------------------------------------------------------ token 签名

fn sign(value: &str, key: &[u8], purpose: &[u8]) -> String {
    let mut msg = purpose.to_vec();
    msg.push(b'|');
    msg.extend_from_slice(value.as_bytes());
    b64url(&hmac_sha256(key, &msg))
}

/// cookie 里放的东西：`v1.<token>.<签名>`。
pub fn sign_token(token: &str, key: &[u8], purpose: &[u8]) -> String {
    format!("v1.{}.{}", token, sign(token, key, purpose))
}

/// 验签并取回原始 token。任何一处不对返回 None。
pub fn unsign_token(value: &str, key: &[u8], purpose: &[u8]) -> Option<String> {
    let parts: Vec<&str> = value.split('.').collect();
    if parts.len() != 3 || parts[0] != "v1" || parts[1].is_empty() || parts[2].is_empty() {
        return None;
    }
    let expected = sign(parts[1], key, purpose);
    if !ct_eq(expected.as_bytes(), parts[2].as_bytes()) {
        return None;
    }
    Some(parts[1].to_string())
}

/// 库里存的就是这个：sha256(原始 token) 的 hex。
pub fn token_fingerprint(token: &str) -> String {
    sha256_hex(token.as_bytes())
}

pub fn new_token() -> String {
    token_urlsafe(TOKEN_BYTES)
}

/// CSRF token：随机数 + 签名（double-submit 的 cookie 半边）。
pub fn issue_csrf(key: &[u8]) -> String {
    sign_token(&token_urlsafe(16), key, b"csrf")
}

pub fn check_csrf(cookie_value: &str, form_value: &str, key: &[u8]) -> bool {
    let left = unsign_token(cookie_value, key, b"csrf");
    let right = unsign_token(form_value, key, b"csrf");
    match (left, right) {
        (Some(l), Some(r)) => ct_eq(l.as_bytes(), r.as_bytes()),
        _ => false,
    }
}

// ------------------------------------------------------------------ 口令哈希（scrypt）

fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], rounds: u32, dklen: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(dklen);
    let mut block: u32 = 1;
    while out.len() < dklen {
        let mut salt_block = salt.to_vec();
        salt_block.extend_from_slice(&block.to_be_bytes());
        let mut u = hmac_sha256(password, &salt_block);
        let mut acc = u;
        for _ in 1..rounds {
            u = hmac_sha256(password, &u);
            for i in 0..32 {
                acc[i] ^= u[i];
            }
        }
        out.extend_from_slice(&acc);
        block += 1;
    }
    out.truncate(dklen);
    out
}

fn salsa20_8(block: &mut [u32; 16]) {
    let mut x = *block;
    for _ in 0..4 {
        // column round
        x[4] ^= x[0].wrapping_add(x[12]).rotate_left(7);
        x[8] ^= x[4].wrapping_add(x[0]).rotate_left(9);
        x[12] ^= x[8].wrapping_add(x[4]).rotate_left(13);
        x[0] ^= x[12].wrapping_add(x[8]).rotate_left(18);
        x[9] ^= x[5].wrapping_add(x[1]).rotate_left(7);
        x[13] ^= x[9].wrapping_add(x[5]).rotate_left(9);
        x[1] ^= x[13].wrapping_add(x[9]).rotate_left(13);
        x[5] ^= x[1].wrapping_add(x[13]).rotate_left(18);
        x[14] ^= x[10].wrapping_add(x[6]).rotate_left(7);
        x[2] ^= x[14].wrapping_add(x[10]).rotate_left(9);
        x[6] ^= x[2].wrapping_add(x[14]).rotate_left(13);
        x[10] ^= x[6].wrapping_add(x[2]).rotate_left(18);
        x[3] ^= x[15].wrapping_add(x[11]).rotate_left(7);
        x[7] ^= x[3].wrapping_add(x[15]).rotate_left(9);
        x[11] ^= x[7].wrapping_add(x[3]).rotate_left(13);
        x[15] ^= x[11].wrapping_add(x[7]).rotate_left(18);
        // row round
        x[1] ^= x[0].wrapping_add(x[3]).rotate_left(7);
        x[2] ^= x[1].wrapping_add(x[0]).rotate_left(9);
        x[3] ^= x[2].wrapping_add(x[1]).rotate_left(13);
        x[0] ^= x[3].wrapping_add(x[2]).rotate_left(18);
        x[6] ^= x[5].wrapping_add(x[4]).rotate_left(7);
        x[7] ^= x[6].wrapping_add(x[5]).rotate_left(9);
        x[4] ^= x[7].wrapping_add(x[6]).rotate_left(13);
        x[5] ^= x[4].wrapping_add(x[7]).rotate_left(18);
        x[11] ^= x[10].wrapping_add(x[9]).rotate_left(7);
        x[8] ^= x[11].wrapping_add(x[10]).rotate_left(9);
        x[9] ^= x[8].wrapping_add(x[11]).rotate_left(13);
        x[10] ^= x[9].wrapping_add(x[8]).rotate_left(18);
        x[12] ^= x[15].wrapping_add(x[14]).rotate_left(7);
        x[13] ^= x[12].wrapping_add(x[15]).rotate_left(9);
        x[14] ^= x[13].wrapping_add(x[12]).rotate_left(13);
        x[15] ^= x[14].wrapping_add(x[13]).rotate_left(18);
    }
    for i in 0..16 {
        block[i] = block[i].wrapping_add(x[i]);
    }
}

fn xor_blocks(a: &mut [u32], b: &[u32]) {
    for (x, y) in a.iter_mut().zip(b.iter()) {
        *x ^= *y;
    }
}

/// scrypt（RFC 7914）：N=16384, r=8, p=1, dklen=32 与 Python 默认口径一致。
pub fn scrypt_dk(password: &[u8], salt: &[u8], n: u32, r: u32, p: u32, dklen: usize) -> Vec<u8> {
    let r = r as usize;
    let p = p as usize;
    let n = n as usize;
    let block_words = 32 * r; // 128*r bytes = 32r words
    let mut blocks: Vec<u32> = Vec::with_capacity(p * block_words);
    // B = PBKDF2(password, salt, 1, p * 128*r)
    let b = pbkdf2_hmac_sha256(password, salt, 1, p * 128 * r);
    for chunk in b.chunks(4) {
        // RFC 7914 §2.2：B 按 32 位**小端** words 解释（输出时同样 LE 转回字节）。
        let mut word = 0u32;
        for (i, byte) in chunk.iter().enumerate() {
            word |= (*byte as u32) << (8 * i);
        }
        blocks.push(word);
    }
    // ROMix（RFC 7914）：每个 B_i 独立过 V 表 + X 循环。
    let v = &mut vec![0u32; n * block_words][..];
    for i in 0..p {
        let off = i * block_words;
        // X = B_i
        let mut x = blocks[off..off + block_words].to_vec();
        // V[0..n)：V[j] = X; X = BlockMix(X)
        for j in 0..n {
            v[j * block_words..(j + 1) * block_words].copy_from_slice(&x);
            x = block_mix(&x, r);
        }
        for _ in 0..n {
            // Integerify(X)（RFC 7914）：最后一个 64 字节子块的前 8 字节按小端
            // 读成 u64。words 本身即 LE，直接 w0 | w1<<32。
            let j = ((u64::from(x[block_words - 16]) | (u64::from(x[block_words - 15]) << 32))
                as usize)
                % n;
            xor_blocks(&mut x, &v[j * block_words..(j + 1) * block_words]);
            x = block_mix(&x, r);
        }
        blocks[off..off + block_words].copy_from_slice(&x);
    }
    // 最终 DK = PBKDF2(password, B, 1, dklen)
    let mut bbytes = Vec::with_capacity(blocks.len() * 4);
    for w in &blocks {
        // 与上文 LE 解释对称：输出字节流同样按小端写回。
        bbytes.extend_from_slice(&w.to_le_bytes());
    }
    pbkdf2_hmac_sha256(password, &bbytes, 1, dklen)
}

/// BlockMix：把 32r 个 word 的块当 2r 个 16-word 的 Salsa 块（Y[0], Y[1], ...）。
fn block_mix(input: &[u32], r: usize) -> Vec<u32> {
    let mut x: [u32; 16] = input[(2 * r - 1) * 16..(2 * r) * 16].try_into().unwrap();
    let mut out = vec![0u32; 32 * r];
    for i in 0..2 * r {
        let mut block: [u32; 16] = input[i * 16..(i + 1) * 16].try_into().unwrap();
        xor_blocks(&mut block, &x);
        salsa20_8(&mut block);
        x = block;
        // 偶数位落前半，奇数位落后半（RFC 7914 的 Y[i] 交错布局）
        if i % 2 == 0 {
            out[(i / 2) * 16..(i / 2 + 1) * 16].copy_from_slice(&x);
        } else {
            out[(r + i / 2) * 16..(r + i / 2 + 1) * 16].copy_from_slice(&x);
        }
    }
    out
}

/// scrypt 哈希：`scrypt$n$r$p$<salt_b64>$<dk_b64>`（与 Python `hash_password` 逐字节同形）。
pub fn hash_password(password: &str, salt: Option<&[u8]>) -> String {
    let salt = salt
        .map(|s| s.to_vec())
        .unwrap_or_else(|| urandom(SALT_BYTES));
    let dk = scrypt_dk(
        password.as_bytes(),
        &salt,
        SCRYPT_N,
        SCRYPT_R,
        SCRYPT_P,
        SCRYPT_DKLEN,
    );
    format!(
        "scrypt${}${}${}${}${}",
        SCRYPT_N,
        SCRYPT_R,
        SCRYPT_P,
        b64url(&salt),
        b64url(&dk)
    )
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 6 || parts[0] != "scrypt" {
        return false;
    }
    let (Ok(n), Ok(r), Ok(p)) = (
        parts[1].parse::<u32>(),
        parts[2].parse::<u32>(),
        parts[3].parse::<u32>(),
    ) else {
        return false;
    };
    let Some(salt) = unb64(parts[4]) else {
        return false;
    };
    let Some(dk) = unb64(parts[5]) else {
        return false;
    };
    let calc = scrypt_dk(password.as_bytes(), &salt, n, r, p, dk.len());
    ct_eq(&calc, &dk)
}

pub fn looks_hashed(stored: &str) -> bool {
    stored.starts_with("scrypt$") && stored.split('$').count() == 6
}

/// 口令的最低要求（Python `check_password_policy`）。
pub fn check_password_policy(password: &str) -> Result<(), String> {
    if password.len() < PASSWORD_MIN {
        return Err(format!(
            "口令至少 {} 位（当前 {} 位）",
            PASSWORD_MIN,
            password.len()
        ));
    }
    if password.len() > PASSWORD_MAX {
        return Err(format!("口令最多 {} 位", PASSWORD_MAX));
    }
    if password.trim().is_empty() {
        return Err("口令不能全是空白字符".to_string());
    }
    Ok(())
}

/// 会话签名密钥：优先文件，其次环境变量 `BB_VIEW_SESSION_KEY`。base64 或 ≥16 字节随机串都收。
pub fn load_session_key(path: Option<&str>) -> Result<Vec<u8>, String> {
    let raw = if let Some(path) = path {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("读不到密钥文件 {path}: {e}"))?;
        let text = text.trim().to_string();
        if text.is_empty() {
            return Err(format!("会话密钥文件是空的：{path}（用 init-key 生成）"));
        }
        text
    } else {
        let raw = std::env::var(KEY_ENV).unwrap_or_default();
        if raw.trim().is_empty() {
            return Err(format!(
                "没有会话密钥：给 --session-key-file，或设置环境变量 {KEY_ENV}"
            ));
        }
        raw.trim().to_string()
    };
    key_bytes(&raw)
}

fn key_bytes(raw: &str) -> Result<Vec<u8>, String> {
    if let Some(key) = unb64(raw) {
        if key.len() >= 16 {
            return Ok(key);
        }
    }
    let key = raw.as_bytes().to_vec();
    if key.len() < 16 {
        return Err("会话密钥太短（至少 16 字节），请用 init-key 重新生成".to_string());
    }
    Ok(key)
}

/// 生成会话签名密钥文件（32 字节随机 base64，权限 600）。已有且未 --force 则报错。
pub fn init_key(path: &str, force: bool) -> Result<String, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::path::Path::new(path).exists() && !force {
            return Err(format!(
                "密钥文件已存在：{path}（要换新密钥请加 --force，注意会让所有人重新登录）"
            ));
        }
        if let Some(dir) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let key = b64url(&urandom(KEY_BYTES));
        std::fs::write(path, format!("{key}\n")).map_err(|e| e.to_string())?;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        if std::path::Path::new(path).exists() && !force {
            return Err(format!(
                "密钥文件已存在：{path}（要换新密钥请加 --force，注意会让所有人重新登录）"
            ));
        }
        if let Some(dir) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let key = b64url(&urandom(KEY_BYTES));
        std::fs::write(path, format!("{key}\n")).map_err(|e| e.to_string())?;
    }
    Ok(path.to_string())
}

// ------------------------------------------------------------------ 暴力破解防护

/// 同 IP 5 分钟内失败 5 次 → 锁 15 分钟。内存 + 表双记，解锁时间取较晚。
pub struct LockTracker {
    mem: Mutex<HashMap<String, Vec<(i64, String)>>>,
}

impl Default for LockTracker {
    fn default() -> Self {
        Self {
            mem: Mutex::new(HashMap::new()),
        }
    }
}

impl LockTracker {
    /// 从登录流水算解锁时间：只看最近一次成功之后的失败；连续 5 败落窗内即锁。
    pub fn locked_until_from_rows(rows: &[(i64, String)], now: i64) -> Option<i64> {
        let mut sorted_rows: Vec<&(i64, String)> = rows.iter().collect();
        sorted_rows.sort_by_key(|r| r.0); // Python 先 sorted(rows) 再逐条过
        let mut fails: Vec<i64> = Vec::new();
        for (ts, outcome) in sorted_rows {
            if outcome == "ok" {
                fails.clear();
            } else {
                fails.push(*ts);
            }
        }
        let mut until: Option<i64> = None;
        if FAIL_MAX <= fails.len() {
            for i in 0..=(fails.len() - FAIL_MAX) {
                let first = fails[i];
                let fifth = fails[i + FAIL_MAX - 1];
                if fifth - first <= FAIL_WINDOW_S {
                    let candidate = fifth + LOCK_S;
                    if until.map_or(true, |u| candidate > u) {
                        until = Some(candidate);
                    }
                }
            }
        }
        until.filter(|&u| u > now)
    }

    /// 表半边由调用方喂（`recent_login_rows` 的结果）；库抖了传 None。
    pub fn locked_until(
        &self,
        ip: &str,
        table_rows: Option<&[(i64, String)]>,
        now: i64,
    ) -> Option<i64> {
        let since = now - (FAIL_WINDOW_S + LOCK_S);
        let mem = {
            let mut m = self.mem.lock().ok()?;
            let rows = m
                .entry(ip.to_string())
                .or_default()
                .iter()
                .filter(|(ts, _)| *ts >= since)
                .cloned()
                .collect::<Vec<_>>();
            m.insert(ip.to_string(), rows.clone());
            Self::locked_until_from_rows(&rows, now)
        };
        let table = table_rows.and_then(|rows| Self::locked_until_from_rows(rows, now));
        match (mem, table) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn record(&self, ip: &str, outcome: &str, now: i64) {
        if let Ok(mut m) = self.mem.lock() {
            m.entry(ip.to_string())
                .or_default()
                .push((now, outcome.to_string()));
        }
    }
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_and_sensitivity() {
        assert!(role_can_see("owner", "sensitive"));
        assert!(role_can_see("member", "internal"));
        assert!(!role_can_see("member", "sensitive"));
        assert!(!role_can_see("viewer", "internal"));
        assert!(role_can_see("viewer", "public"));
        // 认不出来收紧兜：角色→viewer；密级→internal（与 Python 一致）
        assert!(!role_can_see("weird", "internal"));
        assert!(role_can_see("member", "weird"));
        assert!(!role_can_see("viewer", "weird"));
    }

    #[test]
    fn test_sign_roundtrip() {
        let key = b"0123456789abcdef0123456789abcdef";
        let t = new_token();
        let signed = sign_token(&t, key, b"session");
        assert!(signed.starts_with("v1."));
        assert_eq!(
            unsign_token(&signed, key, b"session").as_deref(),
            Some(t.as_str())
        );
        assert_eq!(unsign_token(&signed, key, b"csrf"), None);
        assert_eq!(
            unsign_token(&signed, b"other-key-other-key!!", b"session"),
            None
        );
        assert_eq!(unsign_token("v1.tampered.sig", key, b"session"), None);
        assert_eq!(unsign_token("garbage", key, b"session"), None);
    }

    #[test]
    fn test_csrf() {
        let key = b"0123456789abcdef0123456789abcdef";
        let tok = issue_csrf(key);
        assert!(check_csrf(&tok, &tok, key));
        let other = issue_csrf(key);
        assert!(!check_csrf(&tok, &other, key));
    }

    #[test]
    fn test_password_hash_roundtrip() {
        // 与 Python hashlib.scrypt 交叉验证值（N=16384, r=8, p=1, dklen=32）：
        // salt = b"0123456789abcdef", password = "password"
        let salt = b"0123456789abcdef";
        let h = hash_password("password", Some(salt));
        assert!(h.starts_with("scrypt$16384$8$1$"));
        assert!(looks_hashed(&h));
        assert!(verify_password("password", &h));
        assert!(!verify_password("Password", &h));
        // 畸形串一律 False 不抛
        assert!(!verify_password("x", "not-a-hash"));
        assert!(!verify_password("x", "md5$1$2$abc$def"));
        check_password_policy("short").unwrap_err();
        check_password_policy("12345678").unwrap();
    }

    #[test]
    fn test_tailnet_and_ip() {
        assert!(in_tailnet("100.64.0.1"));
        assert!(in_tailnet("100.127.255.254"));
        assert!(!in_tailnet("100.128.0.0"));
        assert!(!in_tailnet("127.0.0.1"));
        assert!(!in_tailnet(""));
        assert!(!in_tailnet("junk"));
        assert!(is_loopback("127.0.0.1"));
        assert!(!is_loopback("100.64.0.1"));
        assert!(in_tailnet("::ffff:100.64.0.1"));
        assert!(parse_ip("[::1]").is_some());
        assert!(parse_ip("fe80::1%eth0").is_some());
    }

    #[test]
    fn test_time_roundtrip() {
        assert_eq!(parse_ts("2026-09-19T10:00:00Z"), Some(1_789_812_000));
        assert_eq!(parse_ts("2026-01-01T08:00:00+08:00"), Some(1_767_225_600));
        assert_eq!(parse_ts("2026-01-01T00:00:00Z"), Some(1_767_225_600));
        assert_eq!(parse_ts("junk"), None);
        assert_eq!(beijing("2026-09-19T02:00:00Z"), "2026-09-19 10:00:00");
    }

    #[test]
    fn test_lock_tracker() {
        let now = 1_000_000;
        // 4 败不锁
        let rows4: Vec<(i64, String)> = (0..4).map(|i| (now - 10 * i, "fail".into())).collect();
        assert_eq!(LockTracker::locked_until_from_rows(&rows4, now), None);
        // 5 败落窗内 → 锁 15 分钟
        let rows5: Vec<(i64, String)> = (0..5).map(|i| (now - 10 * i, "fail".into())).collect();
        assert_eq!(
            LockTracker::locked_until_from_rows(&rows5, now),
            Some(now + LOCK_S)
        );
        // 成功一笔勾销
        let mut rows_ok: Vec<(i64, String)> =
            (0..5).map(|i| (now - 10 * i, "fail".into())).collect();
        rows_ok.push((now - 3, "ok".into()));
        rows_ok.reverse();
        assert_eq!(LockTracker::locked_until_from_rows(&rows_ok, now), None);
        // 双记取较晚
        let lt = LockTracker::default();
        lt.record("1.2.3.4", "fail", now);
        lt.record("1.2.3.4", "fail", now);
        lt.record("1.2.3.4", "fail", now);
        lt.record("1.2.3.4", "fail", now);
        assert_eq!(lt.locked_until("1.2.3.4", None, now), None);
        lt.record("1.2.3.4", "fail", now);
        assert!(lt.locked_until("1.2.3.4", None, now).is_some());
    }

    #[test]
    fn test_b64() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(b"foob"), "Zm9vYg");
        assert_eq!(b64url(b"fooba"), "Zm9vYmE");
        assert_eq!(b64url(b"foobar"), "Zm9vYmFy");
        assert_eq!(unb64("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(unb64("Zm9vYg").unwrap(), b"foob");
    }

    #[test]
    fn test_hmac_rfc4231() {
        // RFC 4231 test case 2
        let key = b"Jefe";
        let msg = b"what do ya want for nothing?";
        let expect = "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";
        let got = hmac_sha256(key, msg);
        let got_hex: String = got.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(got_hex, expect);
    }
}
