//! `creds_mirror` —— 中枢侧凭据镜像通道纯逻辑（对应 Python tools/roster_creds_mirror.py）。
//!
//! 铁律：入参与出参里**没有明文**，只有 sha256[:12] —— 结构上拿不到口令。
//! 常量、探针解析（parse_probe）、报告（analyze）、退出码（exit_code）逐条对齐。

pub const SCHEMA: &str = "l5-creds-mirror/1";
pub const DIR_MODE: &str = "700";
pub const FILE_MODE: &str = "600";
pub const OWNER: &str = "root";
pub const DIR_MARK: &str = "DIR ";
pub const FILE_MARK: &str = "M ";
pub const NONE_MARK: &str = "-";
pub const ST_OK: &str = "ok";
pub const ST_MISSING: &str = "missing";
pub const ST_DRIFT: &str = "drift";
pub const ST_MODE: &str = "mode";
pub const ST_OWNER: &str = "owner";
pub const EXIT_OK: i32 = 0;
pub const EXIT_DRIFT: i32 = 1;
pub const EXIT_COLLECT: i32 = 2;

/// 远端只读探针命令（只发 stat / sha256sum / cut，零写操作）。
pub fn probe_cmd(dirpath: &str, members: &[&str]) -> String {
    let names = members.join(" ");
    format!(
        "d={dp}; if [ -d $d ]; then echo {DM}$(stat -c %a $d) $(stat -c %U $d); \
else echo {DM}{N} {N}; fi; \
for m in {names}; do f=$d/$m.pass; if [ -f $f ]; then \
echo {FM}$m $(stat -c %a $f) $(stat -c %U $f) $(sha256sum $f | cut -c1-12); \
else echo {FM}$m {N} {N} {N}; fi; done",
        dp = dirpath,
        names = names,
        DM = DIR_MARK,
        FM = FILE_MARK,
        N = NONE_MARK
    )
}

/// 探针 stdout → 观测结构；读不懂返回 Err（**不许**退化成"没有"）。
pub fn parse_probe(out: &str, members: &[&str]) -> Result<serde_json::Value, String> {
    use serde_json::json;
    let mut dir_row: Option<serde_json::Value> = None;
    let mut files = serde_json::Map::new();
    for raw in out.split('\n') {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix(DIR_MARK) {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() < 2 {
                return Err("探针的 DIR 行读不懂".into());
            }
            dir_row = Some(json!({"present": parts[0] != NONE_MARK,
                                  "mode": parts[0], "owner": parts[1]}));
        } else if let Some(rest) = line.strip_prefix(FILE_MARK) {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() < 4 {
                return Err("探针的 M 行读不懂".into());
            }
            let (name, mode, owner, sha12) = (parts[0], parts[1], parts[2], parts[3]);
            files.insert(
                name.to_string(),
                json!({"present": sha12 != NONE_MARK, "mode": mode, "owner": owner,
                       "sha12": if sha12 == NONE_MARK { "" } else { sha12 }}),
            );
        }
    }
    let dir_row = match dir_row {
        Some(d) => d,
        None => return Err("探针没有回传 DIR 行".into()),
    };
    for m in members {
        if !files.contains_key(*m) {
            return Err(format!(
                "探针漏了 {} 个成员的行",
                members.iter().filter(|x| !files.contains_key(**x)).count()
            ));
        }
    }
    Ok(json!({"dir": dir_row, "files": files}))
}

/// 纯函数报告：expected = {成员: sha12}，observed = {中枢: 观测|null}。
pub fn analyze(
    expected: &[(String, String)],
    observed: &serde_json::Value,
    hubs: &[&str],
    errors: Vec<serde_json::Value>,
) -> serde_json::Value {
    use serde_json::json;
    let mut members: Vec<String> = expected.iter().map(|(m, _)| m.clone()).collect();
    members.sort();
    let mut per_hub: Vec<serde_json::Value> = Vec::new();
    let mut hub_sorted: Vec<&str> = hubs.to_vec();
    hub_sorted.sort();
    for hub in &hub_sorted {
        let obs = observed.get(*hub);
        let mut row = json!({"hub": hub, "dir_present": false, "dir_mode": "",
                             "dir_owner": "", "dir_ok": false, "members": [],
                             "ok": false, "why": ""});
        let obs = match obs {
            None | Some(serde_json::Value::Null) => {
                row["why"] = json!("没有量到这台中枢的镜像目录");
                per_hub.push(row);
                continue;
            }
            Some(o) => o,
        };
        let d = obs.get("dir").cloned().unwrap_or(json!({}));
        let dir_present = d.get("present").and_then(|v| v.as_bool()).unwrap_or(false);
        let dir_mode = d.get("mode").and_then(|v| v.as_str()).unwrap_or("");
        let dir_owner = d.get("owner").and_then(|v| v.as_str()).unwrap_or("");
        let dir_ok = dir_present && dir_mode == DIR_MODE && dir_owner == OWNER;
        row["dir_present"] = json!(dir_present);
        row["dir_mode"] = json!(dir_mode);
        row["dir_owner"] = json!(dir_owner);
        row["dir_ok"] = json!(dir_ok);
        let files = obs.get("files").cloned().unwrap_or(json!({}));
        let mut bad = 0usize;
        for m in &members {
            let want = expected
                .iter()
                .find(|(k, _)| k == m)
                .map(|(_, v)| v.as_str())
                .unwrap_or("");
            let f = files.get(m).cloned().unwrap_or(json!({}));
            let present = f.get("present").and_then(|v| v.as_bool()).unwrap_or(false);
            let got = f.get("sha12").and_then(|v| v.as_str()).unwrap_or("");
            let mode = f.get("mode").and_then(|v| v.as_str()).unwrap_or("");
            let owner = f.get("owner").and_then(|v| v.as_str()).unwrap_or("");
            let status = if !present {
                ST_MISSING
            } else if got != want {
                ST_DRIFT
            } else if mode != FILE_MODE {
                ST_MODE
            } else if owner != OWNER {
                ST_OWNER
            } else {
                ST_OK
            };
            if status != ST_OK {
                bad += 1;
            }
            row["members"].as_array_mut().unwrap().push(json!(
                {"member": m, "present": present, "expected_sha12": want,
                 "actual_sha12": got, "mode": mode, "owner": owner, "status": status}));
        }
        let ok = dir_ok && bad == 0;
        row["ok"] = json!(ok);
        if !ok {
            let mut bits: Vec<String> = Vec::new();
            if !dir_ok {
                bits.push(format!(
                    "目录不合规（present={} mode={} owner={}）",
                    dir_present,
                    if dir_mode.is_empty() { "—" } else { dir_mode },
                    if dir_owner.is_empty() {
                        "—"
                    } else {
                        dir_owner
                    }
                ));
            }
            if bad > 0 {
                bits.push(format!("{} 个成员不合格", bad));
            }
            row["why"] = json!(bits.join("；"));
        }
        per_hub.push(row);
    }
    let mut collect_errors = errors;
    collect_errors.sort_by(|a, b| {
        serde_json::to_string(a)
            .unwrap_or_default()
            .cmp(&serde_json::to_string(b).unwrap_or_default())
    });
    let ok = !hub_sorted.is_empty()
        && !members.is_empty()
        && collect_errors.is_empty()
        && per_hub.iter().all(|r| r["ok"].as_bool().unwrap_or(false));
    json!({"schema": SCHEMA, "hub_list": hub_sorted, "roster": members,
           "per_hub": per_hub, "collect_errors": collect_errors, "ok": ok})
}

/// 报告 → 退出码。采集失败优先（绝不退化为 0/1）。
pub fn exit_code(report: &serde_json::Value) -> i32 {
    let errs = report.get("collect_errors").and_then(|v| v.as_array());
    if errs.map(|a| !a.is_empty()).unwrap_or(false) {
        return EXIT_COLLECT;
    }
    if report.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        EXIT_OK
    } else {
        EXIT_DRIFT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_probe_cmd_shape() {
        let c = probe_cmd("/root/.hacs/nats/creds", &["hermes", "lobster"]);
        assert!(c.contains("d=/root/.hacs/nats/creds;"));
        assert!(c.contains("for m in hermes lobster;"));
        assert!(c.contains("cut -c1-12"));
        assert!(!c.contains("cat "), "探针不允许任何写操作");
    }

    #[test]
    fn test_parse_probe_ok() {
        let out = "DIR 700 root\nM hermes 600 root 0123456789ab\nM top001 - - -\n";
        let obs = parse_probe(out, &["hermes", "top001"]).unwrap();
        assert_eq!(obs["dir"]["mode"], "700");
        assert_eq!(obs["dir"]["present"], json!(true));
        assert_eq!(obs["files"]["hermes"]["sha12"], "0123456789ab");
        assert_eq!(obs["files"]["hermes"]["present"], json!(true));
        assert_eq!(obs["files"]["top001"]["sha12"], "");
        assert_eq!(obs["files"]["top001"]["present"], json!(false));
    }

    #[test]
    fn test_parse_probe_undreadable() {
        assert!(parse_probe("M hermes 600 root abc", &["hermes"]).is_err());
        assert!(parse_probe("DIR 700 root\nM hermes 600 root\n", &["hermes"]).is_err());
        assert!(parse_probe("", &["hermes"]).is_err());
        assert!(parse_probe("DIR 700 root\nM hermes 600 root abc\n", &["hermes", "x"]).is_err());
    }

    #[test]
    fn test_analyze_all_ok() {
        let expected = vec![
            ("hermes".into(), "0123456789ab".into()),
            ("top001".into(), "ffffffffffff".into()),
        ];
        let observed = json!({"hub_a": {"dir": {"present": true, "mode": "700", "owner": "root"},
            "files": {"hermes": {"present": true, "mode": "600", "owner": "root", "sha12": "0123456789ab"},
                      "top001": {"present": true, "mode": "600", "owner": "root", "sha12": "ffffffffffff"}}}});
        let rep = analyze(&expected, &observed, &["hub_a", "hub_b"], vec![]);
        assert!(
            !rep["ok"].as_bool().unwrap(),
            "hub_b 没量到，整体必须不合格"
        );
        assert_eq!(rep["per_hub"][0]["ok"], json!(true));
        assert_eq!(rep["per_hub"][1]["why"], "没有量到这台中枢的镜像目录");
        assert_eq!(exit_code(&rep), EXIT_DRIFT);
    }

    #[test]
    fn test_analyze_drift_and_modes() {
        let expected = vec![("m1".into(), "aaaaaaaaaaaa".into())];
        let mk = |mode: &str, owner: &str, sha: &str, present: bool| {
            json!(
            {"present": present, "mode": mode, "owner": owner, "sha12": sha})
        };
        let observed = json!({"h": {"dir": {"present": true, "mode": "700", "owner": "root"},
            "files": {"m1": mk("600", "root", "bbbbbbbbbbbb", true)}}});
        let rep = analyze(&expected, &observed, &["h"], vec![]);
        assert_eq!(rep["per_hub"][0]["members"][0]["status"], ST_DRIFT);
        // mode 不符
        let observed = json!({"h": {"dir": {"present": true, "mode": "700", "owner": "root"},
            "files": {"m1": mk("644", "root", "aaaaaaaaaaaa", true)}}});
        let rep = analyze(&expected, &observed, &["h"], vec![]);
        assert_eq!(rep["per_hub"][0]["members"][0]["status"], ST_MODE);
        // owner 不符
        let observed = json!({"h": {"dir": {"present": true, "mode": "700", "owner": "root"},
            "files": {"m1": mk("600", "nobody", "aaaaaaaaaaaa", true)}}});
        let rep = analyze(&expected, &observed, &["h"], vec![]);
        assert_eq!(rep["per_hub"][0]["members"][0]["status"], ST_OWNER);
        // 目录 mode 不符
        let observed = json!({"h": {"dir": {"present": true, "mode": "755", "owner": "root"},
            "files": {"m1": mk("600", "root", "aaaaaaaaaaaa", true)}}});
        let rep = analyze(&expected, &observed, &["h"], vec![]);
        assert!(rep["per_hub"][0]["why"]
            .as_str()
            .unwrap()
            .contains("目录不合规"));
        // 全绿
        let observed = json!({"h": {"dir": {"present": true, "mode": "700", "owner": "root"},
            "files": {"m1": mk("600", "root", "aaaaaaaaaaaa", true)}}});
        let rep = analyze(&expected, &observed, &["h"], vec![]);
        assert!(rep["ok"].as_bool().unwrap());
        assert_eq!(exit_code(&rep), EXIT_OK);
    }

    #[test]
    fn test_collect_errors_never_degrade() {
        let rep = analyze(
            &[],
            &json!({}),
            &[],
            vec![json!({"kind": "collect_error", "node": "h", "what": "ssh", "why": "x"})],
        );
        assert_eq!(exit_code(&rep), EXIT_COLLECT);
        assert!(!rep["ok"].as_bool().unwrap());
    }
}
