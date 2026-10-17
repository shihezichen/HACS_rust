//! 正文里手打 @xxx 的识别（Python `parse_text_mentions`，#539 口径）。
//!
//! 三条硬口径：在标点处截断；前缀排除邮箱/普通 @ 文本（用 ASCII 排除集而非 \w）；
//! 汉字黏连由前缀回退兜住（只认截断处是汉字）。别名 所有人/全员/all → `@all`。

use std::collections::HashMap;

/// 正文里手打 @xxx 的 token 提取（对齐 `MENTION_TEXT_RE`）。
/// `(?<![A-Za-z0-9._%+@-])@([^\s@，。！？；：、,.!?;:"'"''（）()[]【】{}《》<>·…]+)`
pub fn mention_tokens(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'@' {
            i += 1;
            continue;
        }
        // 负向后顾：前一个字符在排除集里则这个 @ 不是点名
        if i > 0 {
            let prev = text[..i].chars().next_back().unwrap();
            if matches!(prev,
                'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '%' | '+' | '@' | '-')
            {
                i += 1;
                continue;
            }
        }
        // 取 token：到空白或任一标点为止
        let mut j = i + 1;
        while j < bytes.len() {
            let ch = text[j..].chars().next().unwrap();
            if ch.is_whitespace()
                || matches!(
                    ch,
                    '@' | '，'
                        | '。'
                        | '！'
                        | '？'
                        | '；'
                        | '：'
                        | '、'
                        | ','
                        | '.'
                        | '!'
                        | '?'
                        | ';'
                        | ':'
                        | '"'
                        | '\''
                        | '“'
                        | '”'
                        | '‘'
                        | '’'
                        | '（'
                        | '）'
                        | '('
                        | ')'
                        | '['
                        | ']'
                        | '【'
                        | '】'
                        | '{'
                        | '}'
                        | '《'
                        | '》'
                        | '<'
                        | '>'
                        | '·'
                        | '…'
                )
            {
                break;
            }
            j += ch.len_utf8();
        }
        if j > i + 1 {
            out.push(text[i + 1..j].to_string());
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// token 尾部标点剥离（Python `rstrip(MENTION_TRAILING)`，从右逐字剥）。
fn rstrip_trailing(token: &str) -> String {
    token
        .trim_end_matches(|ch| {
            matches!(
                ch,
                '，' | '。'
                    | '！'
                    | '？'
                    | '；'
                    | '：'
                    | '、'
                    | ','
                    | '.'
                    | '!'
                    | '?'
                    | ';'
                    | ':'
                    | '"'
                    | '\''
                    | '“'
                    | '”'
                    | '‘'
                    | '’'
                    | '）'
                    | ')'
                    | '】'
                    | ']'
                    | '}'
                    | '>'
                    | '》'
                    | '…'
            )
        })
        .to_string()
}

fn is_cjk(ch: char) -> bool {
    matches!(ch, '\u{3400}'..='\u{4dbf}') || matches!(ch, '\u{4e00}'..='\u{9fff}')
}

/// `MENTION_ALL_ALIASES`。
pub fn is_all_alias(token: &str) -> bool {
    matches!(token, "所有人" | "全员" | "all")
}

/// 正文 @xxx → 结构化 mentions（只补命中的会话成员；同一目标只补一次）。
pub fn parse_text_mentions(text: &str, member_ids: &[String]) -> Vec<String> {
    let known: std::collections::HashSet<&String> = member_ids.iter().collect();
    // casefold -> id；歧义（两个成员仅大小写不同）不认
    let mut fold: HashMap<String, Option<String>> = HashMap::new();
    for mid in member_ids {
        let key = mid.to_lowercase();
        let entry = fold.get(&key);
        let new_val = match entry {
            Some(Some(prev)) if prev != mid => None,
            _ => Some(mid.clone()),
        };
        fold.insert(key, new_val);
    }
    let resolve = |token: &str| -> Option<String> {
        if known.contains(&token.to_string()) {
            return Some(token.to_string());
        }
        if is_all_alias(token) {
            return Some("@all".to_string());
        }
        match fold.get(&token.to_lowercase()) {
            Some(Some(id)) => Some(id.clone()),
            _ => None,
        }
    };

    let mut found: Vec<String> = Vec::new();
    for raw in mention_tokens(text) {
        let token = rstrip_trailing(&raw);
        if token.is_empty() {
            continue;
        }
        let mut target = resolve(&token);
        if target.is_none() {
            // 汉字前缀回退：从右往左找汉字截断处，截到的前缀按大小写不敏感解析
            let chars: Vec<char> = token.chars().collect();
            for cut in (1..chars.len()).rev() {
                if !is_cjk(chars[cut]) {
                    continue;
                }
                let prefix: String = chars[..cut].iter().collect();
                if let Some(t) = resolve(&prefix) {
                    target = Some(t);
                    break;
                }
            }
        }
        if let Some(t) = target {
            if !found.contains(&t) {
                found.push(t);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_exact_and_alias() {
        let members = m(&["hermes", "atlas"]);
        assert_eq!(
            parse_text_mentions("@hermes 看看", &members),
            vec!["hermes".to_string()]
        );
        assert_eq!(
            parse_text_mentions("@所有人 收", &members),
            vec!["@all".to_string()]
        );
        assert_eq!(
            parse_text_mentions("@all 收", &members),
            vec!["@all".to_string()]
        );
        assert_eq!(
            parse_text_mentions("@全员", &members),
            vec!["@all".to_string()]
        );
    }

    #[test]
    fn test_punctuation_cut() {
        // 中文不写空格：@hermes，你好 必须截出 hermes（实踩口径）
        let members = m(&["hermes"]);
        assert_eq!(
            parse_text_mentions("@hermes，你好", &members),
            vec!["hermes".to_string()]
        );
    }

    #[test]
    fn test_email_not_mention() {
        let members = m(&["b"]);
        assert!(parse_text_mentions("a@b.com", &members).is_empty());
    }

    #[test]
    fn test_case_insensitive_unique() {
        let members = m(&["Hermes"]);
        assert_eq!(
            parse_text_mentions("@hermes 你好", &members),
            vec!["Hermes".to_string()]
        );
        // 精确命中优先（真有个成员叫 hermes 就不吃亏）；歧义只影响大小写回退路径
        let members = m(&["Hermes", "hermes"]);
        assert_eq!(
            parse_text_mentions("@hermes 你好", &members),
            vec!["hermes".to_string()]
        );
        // 大小写回退在有歧义时认不出「HERMES」
        assert!(parse_text_mentions("@HERMES 你好", &members).is_empty());
    }

    #[test]
    fn test_cjk_glued() {
        // @Hermes你好 → 前缀回退认 Hermes
        let members = m(&["Hermes"]);
        assert_eq!(
            parse_text_mentions("@Hermes你好", &members),
            vec!["Hermes".to_string()]
        );
        // 英文黏连不算点名
        let members = m(&["hermes"]);
        assert!(parse_text_mentions("@hermesabc", &members).is_empty());
    }

    #[test]
    fn test_dedupe_and_unknown() {
        let members = m(&["hermes"]);
        assert_eq!(
            parse_text_mentions("@hermes @hermes", &members),
            vec!["hermes".to_string()]
        );
        assert!(parse_text_mentions("@ghost 你好", &members).is_empty());
    }
}
