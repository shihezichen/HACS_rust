//! L2 适配层：对外部存储（PostgreSQL / pgvector）的**类型口径适配**·单一出处。
//! 对应 Python `hub/l2_adapter/pg_literals.py`。
//!
//! 为什么在这一层：这里没有任何协作域语义（黑板 / 共享内容 / 会议一个字都不沾），
//! 只有「值怎么写给 PG、PG 的值怎么读回来」这一件事 —— 外部依赖的差异落在适配层。
//!
//! 纪律：纯函数，无状态、无 I/O、不碰连接；**认不出来的输入一律返回 `None`，不抛、不猜、不编造**。

/// 时区标记（对应 Python 的 `datetime.timezone.utc`）。
pub const UTC: &str = "+00:00";

/// pgvector 的文本字面量形式，避免为了一列向量引入 pgvector 的适配器。
///
/// 逐字节对齐 Python：`"[" + ",".join(repr(float(x)) for x in embedding) + "]"`。
pub fn vector_literal(embedding: Option<&[f64]>) -> Option<String> {
    let values = embedding?;
    let mut out = String::from("[");
    for (i, x) in values.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&py_repr_float(*x));
    }
    out.push(']');
    Some(out)
}

/// Python `repr(float)` 的对应物。
///
/// Rust 的 `{:?}` 也是「最短且能往返」的十进制表示，与 Python 的算法同族；差异只在两处：
/// 指数写法（Python 是 `e+16` / `e-05`，Rust 是 `e16` / `e-5`）与非数的拼写（`nan`/`inf`）。
/// 本函数把这两处对齐；`{:?}` 已保证整数值带 `.0`、且阈值与 Python 一致
/// （指数 `< -4` 或 `>= 16` 时切指数记法）。
pub fn py_repr_float(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        };
    }
    let s = format!("{x:?}");
    match s.find(['e', 'E']) {
        None => s,
        Some(pos) => {
            let (mantissa, exp) = s.split_at(pos);
            let digits = &exp[1..];
            let (sign, digits) = match digits.strip_prefix('-') {
                Some(d) => ('-', d),
                None => ('+', digits.strip_prefix('+').unwrap_or(digits)),
            };
            let digits = if digits.len() < 2 {
                format!("0{digits}")
            } else {
                digits.to_string()
            };
            format!("{mantissa}e{sign}{digits}")
        }
    }
}

/// 归一化到 UTC 的时间点（自 Unix 纪元起的秒 + 纳秒）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct UtcDateTime {
    pub unix_seconds: i64,
    pub nanos: u32,
}

impl UtcDateTime {
    pub fn from_unix(seconds: i64, nanos: u32) -> Self {
        Self {
            unix_seconds: seconds,
            nanos,
        }
    }

    /// `datetime.isoformat()` 的对应物（UTC）：`2026-09-27T12:34:56.789000+00:00`。
    pub fn to_isoformat(self) -> String {
        let days = self.unix_seconds.div_euclid(86_400);
        let rem = self.unix_seconds.rem_euclid(86_400);
        let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
        let (y, mo, d) = civil_from_days(days);
        let micros = self.nanos / 1000;
        let frac = if micros != 0 {
            format!(".{micros:06}")
        } else {
            String::new()
        };
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}{frac}+00:00")
    }
}

/// 把 timestamptz / ISO 字符串统一成 UTC 时间点；认不出来返回 `None`。
///
/// 裸时间（没带时区）一律按 UTC 解释 —— 宁可口径单一，也不要「看你在哪台机器上跑」。
///
/// 接受的文法（对应 Python `datetime.fromisoformat` 的常用面）：
/// `YYYY-MM-DD` / `YYYYMMDD`，时间分隔符 `T` / 空格 / 无，
/// 时间 `HH:MM[:SS[.frac]]` 或 `HHMM[SS[.frac]]`，
/// 时区 `Z` / `z` / `±HH:MM` / `±HHMM` / `±HH`，或省略（按 UTC）。
pub fn to_utc(value: &str) -> Option<UtcDateTime> {
    let text = value.trim();
    if text.is_empty() {
        return None;
    }
    // 对应 Python：先把结尾的 Z/z 换成 +00:00
    let normalized = if text.ends_with('Z') || text.ends_with('z') {
        format!("{}+00:00", &text[..text.len() - 1])
    } else {
        text.to_string()
    };

    let (year, month, day, rest) = split_date(&normalized)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // 时区从时间部分里第一个 '+' / '-' 起（时间本身不含这两个字符）
    let (time_part, tz_part) = match rest.find(['+', '-']) {
        Some(idx) => (&rest[..idx], Some(&rest[idx..])),
        None => (rest, None),
    };
    let time_part = time_part
        .strip_prefix('T')
        .or_else(|| time_part.strip_prefix(' '))
        .unwrap_or(time_part);

    let (hour, minute, second, nanos) = split_time(time_part)?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let offset_seconds = match tz_part {
        None => 0,
        Some(tz) => split_offset(tz)?,
    };

    let days = days_from_civil(year, month, day);
    let unix_seconds = days * 86_400 + (hour as i64) * 3600 + (minute as i64) * 60 + second as i64
        - offset_seconds;
    Some(UtcDateTime {
        unix_seconds,
        nanos,
    })
}

/// `-> (年, 月, 日, 余下)`；余下是时间与时区部分。
fn split_date(text: &str) -> Option<(i64, u32, u32, &str)> {
    if text.len() >= 10 && text.as_bytes()[4] == b'-' && text.as_bytes()[7] == b'-' {
        let year = text[0..4].parse::<i64>().ok()?;
        let month = text[5..7].parse::<u32>().ok()?;
        let day = text[8..10].parse::<u32>().ok()?;
        return Some((year, month, day, &text[10..]));
    }
    if text.len() >= 8 && text[0..8].bytes().all(|b| b.is_ascii_digit()) {
        let year = text[0..4].parse::<i64>().ok()?;
        let month = text[4..6].parse::<u32>().ok()?;
        let day = text[6..8].parse::<u32>().ok()?;
        return Some((year, month, day, &text[8..]));
    }
    None
}

/// `-> (时, 分, 秒, 纳秒)`；空时间（只有日期）按 00:00:00。
fn split_time(text: &str) -> Option<(u32, u32, u32, u32)> {
    if text.is_empty() {
        return Some((0, 0, 0, 0));
    }
    let (h, m, s, frac) = if text.contains(':') {
        let mut parts = text.split(':');
        let h = parts.next()?;
        let m = parts.next()?;
        let s = parts.next().unwrap_or("0");
        (h, m, s, None)
    } else {
        // 基本格式：HHMM 或 HHMMSS[.frac]
        let (main, frac) = match text.split_once('.') {
            Some((a, b)) => (a, Some(b)),
            None => (text, None),
        };
        if main.len() < 4 || !main.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        (
            &main[0..2],
            &main[2..4],
            main.get(4..6).unwrap_or("0"),
            frac,
        )
    };
    let (seconds_str, frac_str) = match frac {
        Some(f) => (s.to_string(), f.to_string()),
        None => match s.split_once('.') {
            Some((a, b)) => (a.to_string(), b.to_string()),
            None => (s.to_string(), String::new()),
        },
    };
    let hour = h.parse::<u32>().ok()?;
    let minute = m.parse::<u32>().ok()?;
    let second = if seconds_str.is_empty() {
        0
    } else {
        seconds_str.parse::<u32>().ok()?
    };
    let nanos = parse_fraction(&frac_str);
    Some((hour, minute, second, nanos))
}

/// 小数秒 → 纳秒（超过 9 位截断，与 datetime 的微秒精度同向）。
fn parse_fraction(frac: &str) -> u32 {
    if frac.is_empty() {
        return 0;
    }
    let digits: String = frac
        .chars()
        .take(9)
        .filter(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return 0;
    }
    let padded = format!("{digits:0<9}");
    padded.parse::<u32>().unwrap_or(0)
}

/// 时区偏移 → 秒（东为正）。
fn split_offset(tz: &str) -> Option<i64> {
    let (sign, body) = match tz.as_bytes().first()? {
        b'+' => (1i64, &tz[1..]),
        b'-' => (-1i64, &tz[1..]),
        _ => return None,
    };
    let (hours, minutes) = if let Some((h, m)) = body.split_once(':') {
        (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?)
    } else if body.len() >= 4 {
        (
            body[0..2].parse::<i64>().ok()?,
            body[2..4].parse::<i64>().ok()?,
        )
    } else if body.len() == 2 {
        (body.parse::<i64>().ok()?, 0)
    } else {
        return None;
    };
    Some(sign * (hours * 3600 + minutes * 60))
}

/// 天数（自 1970-01-01）→ `(年, 月, 日)`（Howard Hinnant civil_from_days）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `(年, 月, 日)` → 天数（自 1970-01-01）（Howard Hinnant days_from_civil）。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
