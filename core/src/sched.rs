use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Utc};
use serde_json::{Value, json};

use crate::events::{self, Db, NewSchedule};
use crate::run::date;

/// `manage_schedule` entry point. The model passes local wall-clock time; the
/// row keeps UTC, like every other timestamp in the database.
pub fn handle(db: &Db, chat_id: i64, args: &Value) -> Value {
    match args["action"].as_str().unwrap_or("") {
        "create" => create(db, chat_id, args),
        "list" => list(db, chat_id),
        "delete" => {
            let Some(id) = args["id"].as_i64() else {
                return fail("id is required for delete");
            };
            match db.schedule_delete(chat_id, id) {
                Ok(true) => json!({"ok": true, "deleted": id}),
                Ok(false) => fail(format!("no schedule #{id}")),
                Err(e) => fail(format!("{e:#}")),
            }
        }
        other => fail(format!(
            "unknown action `{other}`: use create, list or delete"
        )),
    }
}

/// Turn due rows into queue events. A `question` event is a fixed message and
/// a `report` is computed from SQLite: both skip the model. `prompt` events go
/// through the agent like before.
pub fn enqueue_due(db: &Db) -> Result<usize> {
    let due = db.due_schedules(&Utc::now().to_rfc3339())?;
    let mut queued = 0;
    for s in due {
        let (kind, payload) = match s.kind.as_str() {
            "question" => (
                "question",
                json!({"chat_id": s.chat_id, "text": s.text, "key": s.metric_key}),
            ),
            "report" => (
                "report",
                json!({
                    "chat_id": s.chat_id,
                    "text": s.text,
                    "key": s.metric_key,
                    "days": s.days,
                }),
            ),
            _ => ("prompt", json!({"chat_id": s.chat_id, "text": s.text})),
        };
        db.push_event(kind, &payload.to_string())?;
        match next_after(&s.next_at, &s.repeat, Local::now()) {
            Some(next) => db.schedule_advance(s.id, &next.to_rfc3339())?,
            None => {
                db.schedule_delete(s.chat_id, s.id)?;
            }
        }
        queued += 1;
    }
    Ok(queued)
}

fn create(db: &Db, chat_id: i64, args: &Value) -> Value {
    let Some(text) = args["text"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return fail("text is required: the message to send, or a report template");
    };
    let kind = arg(&args["kind"]).unwrap_or("prompt");
    if !matches!(kind, "prompt" | "question" | "report") {
        return fail("kind must be prompt, question or report");
    }
    // The same key rule as log_metric, so a question writes where a report reads.
    let key = args["key"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.chars().count() <= 64);
    if kind != "prompt" && key.is_none() {
        return fail("key is required for a question or a report");
    }
    let days = (kind == "report").then(|| args["days"].as_i64().unwrap_or(7).clamp(1, 3_650));
    let now = Local::now();
    let (next, repeat) = if let Some(s) = arg(&args["at"]) {
        let Some(t) = parse_at(s) else {
            return fail("bad at: use YYYY-MM-DD HH:MM in local time");
        };
        if t <= now {
            return fail("at is in the past");
        }
        (t, "none")
    } else if let Some(s) = arg(&args["daily_at"]) {
        let Some(t) = parse_daily(s, now) else {
            return fail("bad daily_at: use HH:MM in local time");
        };
        (t, "daily")
    } else if let Some(s) = arg(&args["weekly_at"]) {
        let Some(t) = parse_weekly(s, now) else {
            return fail("bad weekly_at: use \"mon HH:MM\" in local time");
        };
        (t, "weekly")
    } else {
        return fail("pass at, daily_at or weekly_at");
    };

    let next_at = next.with_timezone(&Utc).to_rfc3339();
    match db.schedule_add(&NewSchedule {
        chat_id,
        text,
        next_at: &next_at,
        repeat,
        kind,
        metric_key: key,
        days,
    }) {
        Ok(id) => json!({
            "ok": true,
            "id": id,
            "when": date(&next_at),
            "repeat": repeat,
            "kind": kind,
        }),
        Err(e) => fail(format!("{e:#}")),
    }
}

fn list(db: &Db, chat_id: i64) -> Value {
    match db.schedules(chat_id) {
        Ok(rows) => json!({
            "schedules": rows.iter().map(|s| json!({
                "id": s.id,
                "text": s.text,
                "when": date(&s.next_at),
                "repeat": s.repeat,
                "kind": s.kind,
                "key": s.metric_key,
                "days": s.days,
            })).collect::<Vec<_>>()
        }),
        Err(e) => fail(format!("{e:#}")),
    }
}

fn fail(msg: impl Into<String>) -> Value {
    json!({"ok": false, "error": msg.into()})
}

fn arg(v: &Value) -> Option<&str> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty())
}

/// A fixed question was sent and this is the reply. A number is logged under
/// the question's key; anything else stays a normal message for the agent.
/// Either way the question is closed.
pub fn answer(db: &Db, chat_id: i64, text: &str) -> Option<String> {
    let key = db.kv_get(&events::ask_key(chat_id)).ok().flatten()?;
    if let Err(e) = db.kv_del(&events::ask_key(chat_id)) {
        eprintln!("clear ask: {e:#}");
    }
    let num = parse_number(text)?;
    let ts = Utc::now().to_rfc3339();
    match db.metric_add(chat_id, &key, Some(num), None, "{}", &ts) {
        Ok(_) => Some(format!("Записал: {}.", text.trim())),
        Err(e) => Some(format!("Не смог записать: {e:#}")),
    }
}

pub fn pending(db: &Db, chat_id: i64) -> Result<Option<String>> {
    db.kv_get(&events::ask_key(chat_id))
}

pub fn set_pending(db: &Db, chat_id: i64, key: &str) -> Result<()> {
    db.kv_set(&events::ask_key(chat_id), key)
}

pub fn clear_pending(db: &Db, chat_id: i64) -> Result<()> {
    db.kv_del(&events::ask_key(chat_id))
}

/// The whole message is one number, optionally followed by kg. "82.5 вчера"
/// is not a weight: it goes to the agent, which can still log it itself.
fn parse_number(text: &str) -> Option<f64> {
    let mut t = text.trim().to_lowercase();
    t = t.trim_end_matches(&['.', '!'][..]).trim().to_string();
    for unit in ["кг", "kg"] {
        if let Some(rest) = t.strip_suffix(unit) {
            t = rest.trim().to_string();
            break;
        }
    }
    let v: f64 = t.replace(',', ".").parse().ok()?;
    v.is_finite().then_some(v)
}

/// "YYYY-MM-DD HH:MM", "YYYY-MM-DDTHH:MM" or with seconds, in local time.
fn parse_at(s: &str) -> Option<DateTime<Local>> {
    for fmt in ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(n) = chrono::NaiveDateTime::parse_from_str(s.trim(), fmt) {
            return Local.from_local_datetime(&n).earliest();
        }
    }
    None
}

/// Next occurrence of a local wall-clock time today or tomorrow.
fn parse_daily(s: &str, now: DateTime<Local>) -> Option<DateTime<Local>> {
    let (h, m) = parse_hm(s)?;
    let mut date = now.date_naive();
    for _ in 0..2 {
        if let Some(t) = local_at(date, h, m)
            && t > now
        {
            return Some(t);
        }
        date += Duration::days(1);
    }
    None
}

/// Next occurrence of "mon HH:MM" (English or Russian short weekday).
fn parse_weekly(s: &str, now: DateTime<Local>) -> Option<DateTime<Local>> {
    let mut it = s.split_whitespace();
    let target = weekday(it.next()?)?;
    let (h, m) = parse_hm(it.next()?)?;
    let ahead = (target - now.weekday().num_days_from_monday() as i64).rem_euclid(7);
    match local_at(now.date_naive() + Duration::days(ahead), h, m) {
        Some(t) if t > now => Some(t),
        _ => local_at(now.date_naive() + Duration::days(ahead + 7), h, m),
    }
}

fn parse_hm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    (h < 24 && m < 60).then_some((h, m))
}

fn weekday(s: &str) -> Option<i64> {
    Some(match s.to_lowercase().as_str() {
        "mon" | "monday" | "пн" => 0,
        "tue" | "tuesday" | "вт" => 1,
        "wed" | "wednesday" | "ср" => 2,
        "thu" | "thursday" | "чт" => 3,
        "fri" | "friday" | "пт" => 4,
        "sat" | "saturday" | "сб" => 5,
        "sun" | "sunday" | "вс" => 6,
        _ => return None,
    })
}

fn local_at(date: NaiveDate, h: u32, m: u32) -> Option<DateTime<Local>> {
    Local
        .from_local_datetime(&date.and_hms_opt(h, m, 0)?)
        .earliest()
}

/// Next deadline after `scheduled` for a repeating row. Missed days are
/// skipped, so a long downtime yields one reminder instead of a burst.
fn next_after(scheduled: &str, repeat: &str, now: DateTime<Local>) -> Option<DateTime<Utc>> {
    let t = DateTime::parse_from_rfc3339(scheduled)
        .ok()?
        .with_timezone(&Local);
    let days = match repeat {
        "daily" => 1,
        "weekly" => 7,
        _ => return None,
    };
    let mut next = t;
    while next <= now {
        next = shift(next, days)?;
    }
    Some(next.with_timezone(&Utc))
}

/// Add days but keep the local wall-clock time.
fn shift(t: DateTime<Local>, days: i64) -> Option<DateTime<Local>> {
    let naive = (t.date_naive() + Duration::days(days)).and_time(t.time());
    Some(
        Local
            .from_local_datetime(&naive)
            .earliest()
            .unwrap_or_else(|| t + Duration::days(days)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn parses_absolute_and_relative_times() {
        assert_eq!(parse_at("2026-11-16 10:30"), Some(at(2026, 11, 16, 10, 30)));
        assert_eq!(parse_at("2026-11-16T10:30"), Some(at(2026, 11, 16, 10, 30)));
        assert_eq!(parse_at("завтра"), None);

        let now = at(2026, 11, 16, 9, 0);
        assert_eq!(parse_daily("10:00", now), Some(at(2026, 11, 16, 10, 0)));
        let now = at(2026, 11, 16, 12, 0);
        assert_eq!(parse_daily("10:00", now), Some(at(2026, 11, 17, 10, 0)));
        assert_eq!(parse_daily("25:00", now), None);
    }

    #[test]
    fn weekly_picks_the_next_matching_day() {
        // 2026-11-16 is a Monday.
        let now = at(2026, 11, 16, 9, 0);
        assert_eq!(
            parse_weekly("mon 10:00", now),
            Some(at(2026, 11, 16, 10, 0))
        );
        let now = at(2026, 11, 16, 12, 0);
        assert_eq!(
            parse_weekly("mon 10:00", now),
            Some(at(2026, 11, 23, 10, 0))
        );
        assert_eq!(parse_weekly("вс 08:00", now), Some(at(2026, 11, 22, 8, 0)));
        assert_eq!(parse_weekly("каждый день 10:00", now), None);
    }

    #[test]
    fn repeat_advances_and_skips_missed_windows() {
        // The row stores UTC; the wall-clock time is what must stay stable.
        let scheduled = at(2026, 11, 16, 10, 0).with_timezone(&Utc).to_rfc3339();

        let now = at(2026, 11, 16, 12, 0);
        let next = next_after(&scheduled, "daily", now).unwrap();
        assert_eq!(next.with_timezone(&Local), at(2026, 11, 17, 10, 0));

        // Two weeks of downtime: jump to the next future window, not two weeks of bursts.
        let now = at(2026, 11, 30, 12, 0);
        let next = next_after(&scheduled, "daily", now).unwrap();
        assert_eq!(next.with_timezone(&Local), at(2026, 12, 1, 10, 0));

        let next = next_after(&scheduled, "weekly", now).unwrap();
        assert_eq!(next.with_timezone(&Local), at(2026, 12, 7, 10, 0));

        assert_eq!(next_after(&scheduled, "none", now), None);
    }

    #[test]
    fn due_rows_become_events_and_advance() {
        let dir = std::env::temp_dir().join(format!("hermit-sched-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(dir.join("t.db").to_str().unwrap()).unwrap();

        let once = db
            .schedule_add(&NewSchedule {
                chat_id: 1,
                text: "разовое",
                next_at: "2020-01-01T00:00:00+00:00",
                repeat: "none",
                kind: "prompt",
                metric_key: None,
                days: None,
            })
            .unwrap();
        let daily = db
            .schedule_add(&NewSchedule {
                chat_id: 1,
                text: "каждый день",
                next_at: "2020-01-01T10:00:00+00:00",
                repeat: "daily",
                kind: "prompt",
                metric_key: None,
                days: None,
            })
            .unwrap();
        assert_eq!(enqueue_due(&db).unwrap(), 2);
        assert_eq!(db.queue_len().unwrap(), 2);

        let left = db.schedules(1).unwrap();
        assert_eq!(left.len(), 1, "one-shot is deleted");
        assert_eq!(left[0].id, daily);
        let advanced = DateTime::parse_from_rfc3339(&left[0].next_at).unwrap();
        assert!(
            advanced.with_timezone(&Utc) > Utc::now(),
            "daily moved into the future"
        );
        assert_ne!(once, daily);

        assert_eq!(enqueue_due(&db).unwrap(), 0, "nothing due right away");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn numeric_answers_parse_alone() {
        assert_eq!(parse_number("82.5"), Some(82.5));
        assert_eq!(parse_number("82,5 кг"), Some(82.5));
        assert_eq!(parse_number(" 80 KG. "), Some(80.0));
        assert_eq!(parse_number("82.5!"), Some(82.5));
        assert_eq!(
            parse_number("82.5 вчера"),
            None,
            "words mean a normal message"
        );
        assert_eq!(parse_number("напомни в 10"), None);
        assert_eq!(parse_number("кг"), None);
        assert_eq!(parse_number(""), None);
    }

    #[test]
    fn numeric_reply_is_logged_and_closes_the_question() {
        let dir = std::env::temp_dir().join(format!("hermit-ask-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(dir.join("t.db").to_str().unwrap()).unwrap();

        set_pending(&db, 1, "weight").unwrap();
        assert_eq!(
            answer(&db, 1, "82,5 кг").as_deref(),
            Some("Записал: 82,5 кг.")
        );
        let stats = db.metric_stats(1, "weight", None).unwrap();
        assert_eq!(stats.count, 1);
        assert_eq!(stats.last_num, Some(82.5));
        assert_eq!(pending(&db, 1).unwrap(), None, "question is closed");
        assert_eq!(answer(&db, 1, "81"), None, "no question waits for this");

        // A reply that is not a number goes to the agent and also closes it.
        set_pending(&db, 1, "weight").unwrap();
        assert_eq!(answer(&db, 1, "напомни в 10"), None);
        assert_eq!(pending(&db, 1).unwrap(), None);
        assert_eq!(db.metric_stats(1, "weight", None).unwrap().count, 1);

        // Another chat is not affected by this one's question.
        set_pending(&db, 1, "weight").unwrap();
        assert_eq!(answer(&db, 2, "90"), None);
        assert_eq!(pending(&db, 1).unwrap().as_deref(), Some("weight"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn questions_and_reports_need_a_key_and_keep_their_kind() {
        let dir = std::env::temp_dir().join(format!("hermit-kind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(dir.join("t.db").to_str().unwrap()).unwrap();

        let out = handle(
            &db,
            1,
            &json!({"action":"create","kind":"question","text":"Сколько весишь?","daily_at":"07:00"}),
        );
        assert_eq!(out["ok"], false, "key is required");
        assert_eq!(
            handle(
                &db,
                1,
                &json!({"action":"create","kind":"nope","text":"x","daily_at":"07:00"})
            )["ok"],
            false
        );

        assert_eq!(
            handle(
                &db,
                1,
                &json!({
                    "action":"create","kind":"question","key":"weight",
                    "text":"Доброе утро. Сколько весишь?","daily_at":"07:00"
                })
            )["ok"],
            true
        );
        assert_eq!(
            handle(
                &db,
                1,
                &json!({
                    "action":"create","kind":"report","key":"weight","days":7,
                    "text":"Средний вес: {avg} кг ({count} записей).","weekly_at":"sun 21:00"
                })
            )["ok"],
            true
        );
        let rows = db.schedules(1).unwrap();
        assert_eq!(rows[0].kind, "question");
        assert_eq!(rows[0].metric_key.as_deref(), Some("weight"));
        assert_eq!(rows[0].days, None);
        assert_eq!(rows[1].kind, "report");
        assert_eq!(rows[1].days, Some(7));

        // Due rows keep their kind in the queue.
        for (kind, key, text) in [
            ("question", Some("weight"), "Сколько весишь?"),
            ("report", Some("weight"), "Средний вес: {avg}"),
            ("prompt", None, "обычное напоминание"),
        ] {
            db.schedule_add(&NewSchedule {
                chat_id: 1,
                text,
                next_at: "2020-01-01T00:00:00+00:00",
                repeat: "none",
                kind,
                metric_key: key,
                days: (kind == "report").then_some(7),
            })
            .unwrap();
        }
        assert_eq!(enqueue_due(&db).unwrap(), 3);
        let mut kinds = Vec::new();
        while let Some(ev) = db.claim_event().unwrap() {
            kinds.push(ev.kind);
        }
        assert!(kinds.contains(&"question".to_string()));
        assert!(kinds.contains(&"report".to_string()));
        assert!(kinds.contains(&"prompt".to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }
}
