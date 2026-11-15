use chrono::{Duration, Utc};
use serde_json::{Value, json};

use crate::events::Db;
use crate::run::date;

/// `log_metric`: one number, one fact, or both, under a free-form key.
pub fn log(db: &Db, chat_id: i64, args: &Value) -> Value {
    let Some(key) = key(args) else {
        return fail("key is required");
    };
    let num = args["value_num"].as_f64();
    let text = args["value_text"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if num.is_none() && text.is_none() {
        return fail("pass value_num or value_text");
    }
    if text.is_some_and(|t| t.chars().count() > 1_000) {
        return fail("value_text is too long (max 1000)");
    }
    let meta = match &args["meta"] {
        Value::Null => "{}".to_string(),
        v if v.is_object() => v.to_string(),
        _ => return fail("meta must be an object"),
    };
    if meta.chars().count() > 2_000 {
        return fail("meta is too long (max 2000)");
    }

    match db.metric_add(chat_id, key, num, text, &meta, &Utc::now().to_rfc3339()) {
        Ok(id) => json!({"ok": true, "id": id, "key": key}),
        Err(e) => fail(format!("{e:#}")),
    }
}

/// `query_metrics`: aggregates computed by SQLite, plus the latest rows.
pub fn query(db: &Db, chat_id: i64, args: &Value) -> Value {
    let since = args["since_days"]
        .as_i64()
        .map(|d| (Utc::now() - Duration::days(d.clamp(1, 3_650))).to_rfc3339());
    let since = since.as_deref();

    let Some(key) = key(args) else {
        return match db.metric_keys(chat_id, since) {
            Ok(keys) => json!({
                "keys": keys.iter().map(|(k, count, last)| json!({
                    "key": k,
                    "count": count,
                    "last_at": last.as_deref().map(date),
                })).collect::<Vec<_>>()
            }),
            Err(e) => fail(format!("{e:#}")),
        };
    };

    let limit = args["limit"].as_i64().unwrap_or(10).clamp(1, 50) as usize;
    let stats = match db.metric_stats(chat_id, key, since) {
        Ok(s) => s,
        Err(e) => return fail(format!("{e:#}")),
    };
    let rows = match db.metric_rows(chat_id, key, since, limit) {
        Ok(r) => r,
        Err(e) => return fail(format!("{e:#}")),
    };
    let last = match (stats.last_num, &stats.last_text) {
        (Some(n), _) => json!(n),
        (None, Some(t)) => json!(t),
        (None, None) => Value::Null,
    };

    json!({
        "key": key,
        "count": stats.count,
        "sum": round(stats.sum),
        "avg": round(stats.avg),
        "min": stats.min.map(round),
        "max": stats.max.map(round),
        "last": last,
        "last_at": stats.last_ts.as_deref().map(date),
        "rows": rows.iter().map(|m| json!({
            "at": date(&m.ts),
            "value": m.value_num,
            "text": m.value_text,
            "meta": meta(&m.meta),
        })).collect::<Vec<_>>()
    })
}

fn key(args: &Value) -> Option<&str> {
    args["key"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.chars().count() <= 64)
}

fn meta(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| json!(s))
}

fn round(v: f64) -> f64 {
    (v * 1_000.0).round() / 1_000.0
}

fn fail(msg: impl Into<String>) -> Value {
    json!({"ok": false, "error": msg.into()})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn db(name: &str) -> (Arc<Db>, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("hermit-metrics-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(dir.join("t.db").to_str().unwrap()).unwrap();
        (db, dir)
    }

    #[test]
    fn log_validates_input() {
        let (db, dir) = db("log");
        assert!(log(&db, 1, &json!({"key": "weight", "value_num": 82.5}))["ok"] == true);
        assert!(log(&db, 1, &json!({"key": "mood", "value_text": "ok"}))["ok"] == true);
        assert!(log(&db, 1, &json!({"key": "mood"}))["ok"] == false);
        assert!(log(&db, 1, &json!({"value_num": 1}))["ok"] == false);
        assert!(log(&db, 1, &json!({"key": "x", "value_num": 1, "meta": [1]}))["ok"] == false);
        assert!(
            log(&db, 1, &json!({"key": "k".repeat(100), "value_num": 1}))["ok"] == false,
            "key cap"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn query_aggregates_and_lists_keys() {
        let (db, dir) = db("query");
        log(&db, 7, &json!({"key": "weight", "value_num": 80}));
        log(
            &db,
            7,
            &json!({"key": "weight", "value_num": 82, "meta": {"unit": "kg"}}),
        );
        log(&db, 7, &json!({"key": "mood", "value_text": "хорошо"}));

        let out = query(&db, 7, &json!({"key": "weight"}));
        assert_eq!(out["count"], 2);
        assert_eq!(out["avg"], 81.0);
        assert_eq!(out["min"], 80.0);
        assert_eq!(out["max"], 82.0);
        assert_eq!(out["last"], 82.0, "newest row wins");
        assert_eq!(out["rows"][0]["meta"]["unit"], "kg");
        assert_eq!(out["rows"].as_array().unwrap().len(), 2);

        let keys = query(&db, 7, &json!({}));
        assert_eq!(keys["keys"].as_array().unwrap().len(), 2);

        // Another chat does not see these rows.
        let other = query(&db, 8, &json!({"key": "weight"}));
        assert_eq!(other["count"], 0);

        // Old rows fall out of the window.
        db.metric_add(
            7,
            "weight",
            Some(70.0),
            None,
            "{}",
            "2020-01-01T00:00:00+00:00",
        )
        .unwrap();
        assert_eq!(query(&db, 7, &json!({"key": "weight"}))["count"], 3);
        assert_eq!(
            query(&db, 7, &json!({"key": "weight", "since_days": 2}))["count"],
            2
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rows_are_limited_and_text_is_last_when_no_number() {
        let (db, dir) = db("text");
        log(&db, 1, &json!({"key": "note", "value_text": "first"}));
        log(&db, 1, &json!({"key": "note", "value_text": "second"}));

        let out = query(&db, 1, &json!({"key": "note", "limit": 1}));
        assert_eq!(out["last"], "second");
        assert_eq!(out["rows"].as_array().unwrap().len(), 1);
        assert_eq!(out["sum"], 0.0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
