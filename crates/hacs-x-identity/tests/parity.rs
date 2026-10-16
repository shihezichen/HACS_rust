//! X 横切等价测试：错误文本脱敏（对照 Python `view_auth.safe_err`）。

use hacs_x_identity::{safe_err, safe_err_with};

#[test]
fn redacts_secret_assignments_keeping_the_key() {
    assert_eq!(safe_err("password=hunter2 host=x"), "password=*** host=x");
    // 关键词原大小写保留（Python 的 `\1` 是捕获到的原文）
    assert_eq!(safe_err("PASSWORD=hunter2"), "PASSWORD=***");
    assert_eq!(safe_err("passfile=/etc/pg.pass"), "passfile=***");
    assert_eq!(safe_err("sslpassword=abc"), "sslpassword=***");
}

#[test]
fn consumes_whitespace_around_the_equals_sign() {
    // 正则 `(kw)\s*=\s*\S+`：等号两侧空白一并吃掉，替换形态固定为 `KEY=***`。
    // `\S+` 在第一个空格处停 —— 后面原有的空白照旧留在原文里。
    assert_eq!(
        safe_err("dsn password =  s3cret  next"),
        "dsn password=***  next"
    );
}

#[test]
fn folds_newlines_and_trims() {
    assert_eq!(safe_err("  a\nb  "), "a b");
    // 中间空白串**不**折叠（Python 只 replace("\n")，不 sub \s+）
    assert_eq!(safe_err("a  b\tc"), "a  b\tc");
}

#[test]
fn requires_a_nonempty_value_after_the_keyword() {
    // `\S+` 要求至少一个字符：没有值就不算命中，原文照旧。
    // 注意 `strip()` 在正则**之前**发生，所以首尾空白早已被去掉。
    assert_eq!(safe_err("password="), "password=");
    assert_eq!(safe_err("password = "), "password =");
    assert_eq!(safe_err("password"), "password");
}

#[test]
fn truncates_to_400_characters() {
    let long = "x".repeat(1000);
    assert_eq!(safe_err(&long).chars().count(), 400);
    // 截断按字符，不按字节
    let wide = "好".repeat(500);
    assert_eq!(safe_err(&wide).chars().count(), 400);
}

#[test]
fn empty_message_falls_back_to_the_label() {
    assert_eq!(safe_err_with("", "OSError"), "OSError");
    assert_eq!(safe_err_with("\n  \n", "ValueError"), "ValueError");
    assert_eq!(safe_err(""), "Error");
}
