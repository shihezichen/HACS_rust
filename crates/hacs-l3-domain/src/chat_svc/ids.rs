//! id 生成（契约 §2，Python `gen_*` 同形状）。

use super::now_epoch;

/// 小写 base32（无填充）。Python `_b32(n)`：n 的小端字节 → base32 → 去 `=` → lower。
fn b32_of(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut bits: Vec<bool> = Vec::with_capacity(bytes.len() * 8);
    for b in bytes {
        for k in (0..8).rev() {
            bits.push((b >> k) & 1 == 1);
        }
    }
    let mut out = String::new();
    for chunk in bits.chunks(5) {
        let mut v = 0u32;
        for (i, bit) in chunk.iter().enumerate() {
            if *bit {
                v |= 1 << (4 - i);
            }
        }
        out.push(ALPHABET[v as usize] as char);
    }
    out
}

/// Python `_b32`：n 的小端字节（n=0 → b"\x00"），base32 后去掉 `=` 填充。
/// 注意：不足 5 字节的输入经 base32 会补出 `=`，被 rstrip 掉 —— 与 Python 一致。
pub fn b32_small(mut n: u32) -> String {
    let mut raw: Vec<u8> = Vec::new();
    if n == 0 {
        raw.push(0);
    }
    while n > 0 {
        raw.push((n & 0xFF) as u8);
        n >>= 8;
    }
    // 仿 base64.b32encode：补到 5 字节倍数再编（填零），然后去 '='
    let mut padded = raw.clone();
    while padded.len() % 5 != 0 {
        padded.push(0);
    }
    let s = b32_of(&padded);
    // '=' 数量 = (5 - len%5) * 8 / 5 bits → 每 5 字节组产生 0/2/4/5/7 个 '='；
    // 简化：直接按真实字节数算应截掉的字符数。
    let rem = raw.len() % 5;
    let strip = match rem {
        0 => 0,
        1 => 6,
        2 => 4,
        3 => 3,
        _ => 1, // 4
    };
    let keep = s.len().saturating_sub(strip);
    s[..keep].to_lowercase()
}

fn rand_bits_30() -> u32 {
    // /dev/urandom 30 bit（与 task_svc 同款：不依赖外部 crate）
    use std::io::Read;
    let mut buf = [0u8; 4];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    u32::from_le_bytes(buf) & 0x3FFF_FFFF
}

/// BJ 日钟字符串：`yyyyMMdd` / `yyyyMMddHHmmss`。
fn bj_stamp(epoch_s: i64, with_time: bool) -> String {
    let (y, mo, d, h, mi, s) = crate::chat_svc::civil_from_epoch(epoch_s + crate::chat_svc::BJ_OFFSET_S);
    if with_time {
        format!("{:04}{:02}{:02}{:02}{:02}{:02}", y, mo, d, h, mi, s)
    } else {
        format!("{:04}{:02}{:02}", y, mo, d)
    }
}

fn now_bj() -> (String, String) {
    let now = now_epoch();
    (bj_stamp(now, false), bj_stamp(now, true))
}

/// `<kind>_<yyyyMMdd>_<6位小写base32>`。
pub fn gen_session_id(kind: &str) -> String {
    let (date, _) = now_bj();
    let tail: String = b32_small(rand_bits_30()).chars().take(6).collect();
    format!("{}_{}_{}", kind, date, tail)
}

/// `msg_<yyyyMMddHHmmss>_<6位小写base32>`。
pub fn gen_message_id() -> String {
    let (_, stamp) = now_bj();
    let tail: String = b32_small(rand_bits_30()).chars().take(6).collect();
    format!("msg_{}_{}", stamp, tail)
}

fn rand_hex6() -> String {
    use std::io::Read;
    let mut buf = [0u8; 3];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

/// `td_<yyyyMMddHHmmss>_<6位hex>`。
pub fn gen_todo_id() -> String {
    let (_, stamp) = now_bj();
    format!("td_{}_{}", stamp, rand_hex6())
}

/// `sk_<yyyyMMddHHmmss>_<6位hex>`。
pub fn gen_skip_id() -> String {
    let (_, stamp) = now_bj();
    format!("sk_{}_{}", stamp, rand_hex6())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_b32_shape() {
        let s = b32_small(0);
        assert_eq!(s, "aa"); // b"\x00" → 1 字节 → base32 "aa======"
        let s = b32_small(0xFFFFFFF);
        assert!(s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert!(!s.contains('='));
    }

    #[test]
    fn test_id_shapes() {
        let sid = gen_session_id("dm");
        assert!(sid.starts_with("dm_") && sid.matches('_').count() == 2);
        let date = sid.split('_').nth(1).unwrap();
        assert_eq!(date.len(), 8);
        let tail = sid.split('_').nth(2).unwrap();
        assert_eq!(tail.len(), 6);
        let mid = gen_message_id();
        assert!(mid.starts_with("msg_") && mid.split('_').nth(1).unwrap().len() == 14);
        let tid = gen_todo_id();
        assert!(tid.starts_with("td_"));
        assert!(tid.split('_').nth(2).unwrap().chars().all(|c| c.is_ascii_hexdigit()));
        let skid = gen_skip_id();
        assert!(skid.starts_with("sk_"));
    }
}
