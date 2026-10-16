fn main() {
    let mut client = postgres::Client::connect("host=127.0.0.1 port=15432 dbname=chat user=postgres", postgres::NoTls).unwrap();
    let hits = serde_json::json!(vec!["进度".to_string()]);
    let r = client.execute(
        "INSERT INTO chat_todo_intent_skips (skip_id, session_id, seq, message_id, author_id, owner, intent, reason, hits, text, dedupe_key) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) ON CONFLICT (session_id, dedupe_key) DO NOTHING RETURNING skip_id",
        &[&"sk_x", &"s1", &Some(1i64), &"m1", &"boss", &"hermes", &"followup", &"followup_phrase", &hits, &"进度如何", &"进度如何"]);
    println!("{:?}", r);
}
