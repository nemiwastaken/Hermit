use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use serde_json::{Value, json};

use crate::cmd::Flags;
use crate::events::{self, ARCHIVE_DAYS, Archived, Db, recap_id_key, recap_key, task_key};
use crate::llm::Llm;
use crate::policy;
use crate::prompt;
use crate::sandbox::Sandbox;

/// Hard stop for a runaway tool loop. The model gets one warning per run,
/// the queue gets its answer, and the chat stays responsive.
const MAX_STEPS: usize = 12;
/// Messages kept per chat, newest last. Older ones are dropped before the call.
const KEEP_MESSAGES: usize = 30;
/// One tool result must not fill the whole context window.
const MAX_TOOL_CHARS: usize = 8_000;
/// The classifier sees this much of the last question and answer.
const PAIR_CHARS: usize = 600;
/// And this much of the new message.
const MESSAGE_CHARS: usize = 1_500;
/// A recap goes into the system prompt; it must not dwarf the prompt itself.
const RECAP_CHARS: usize = 3_000;
/// A recalled transcript, newest messages win when it does not fit.
const TRANSCRIPT_CHARS: usize = 12_000;
/// At most this many archived sessions reach the search prompt.
const CANDIDATES: usize = 15;
/// List size for `/sessions` and `recall()`.
const RECALL_LIST: usize = 10;

pub struct Agent {
    pub llm: Llm,
    pub tools: Sandbox,
    pub db: Arc<Db>,
    pub flags: Arc<Flags>,
    pub behavior_dir: String,
}

struct Recap {
    info: Archived,
    text: String,
}

impl Agent {
    /// The whole agent loop: assemble → LLM → policy → tools → repeat.
    pub async fn reply(&self, event_id: i64, chat_id: i64, text: &str) -> Result<String> {
        let (mut history, updated_at) = self.db.history(chat_id)?;
        history.retain(|m| m["role"] != "system");
        let mut note = None;

        if !history.is_empty()
            && events::expired(
                updated_at.as_deref().unwrap_or(""),
                Utc::now(),
                ARCHIVE_DAYS,
            )
        {
            self.clear_session(chat_id)?;
            history.clear();
            note = Some(format!(
                "— контекст старше {ARCHIVE_DAYS} дней удалён, начал новый диалог"
            ));
        }

        if !history.is_empty() && !self.continues(&history, text, updated_at.as_deref()).await {
            match self
                .archive_current(chat_id, &history, updated_at.as_deref())
                .await
            {
                Ok(Some(title)) => {
                    note = Some(format!("— прошлый диалог закрыт в архив: «{title}»"));
                    history.clear();
                }
                Ok(None) => history.clear(),
                // Better a stale topic in context than a lost task: keep it.
                Err(e) => eprintln!("archive_current: {e:#}"),
            }
        }

        if history.is_empty() {
            self.db.kv_set(&task_key(chat_id), text)?;
            match self.retrieve(chat_id, text).await {
                Ok(Some(r)) => {
                    self.db.kv_set(&recap_key(chat_id), &r.text)?;
                    self.db
                        .kv_set(&recap_id_key(chat_id), &r.info.id.to_string())?;
                    note = Some(format!("— вспомнил сессию «{}»", r.info.title));
                }
                Ok(None) => self.forget_recap(chat_id)?,
                Err(e) => {
                    eprintln!("retrieve: {e:#}");
                    self.forget_recap(chat_id)?;
                }
            }
        }

        let mut system = prompt::system(&self.behavior_dir);
        if let Some(recap) = self.db.kv_get(&recap_key(chat_id))? {
            system.push_str("\n---\n");
            system.push_str(&recap);
        }
        history.insert(0, json!({"role": "system", "content": system}));
        history.push(json!({"role": "user", "content": text}));

        for step in 0..MAX_STEPS {
            let tools = if self.flags.safe() {
                None
            } else {
                Some(policy::schema())
            };
            let reply = self.llm.chat(&history, tools.as_ref()).await?;
            if let Some(u) = &reply.usage {
                eprintln!("usage: {u}");
            }
            let msg = reply.message;
            let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
            history.push(msg.clone());

            if calls.is_empty() {
                let content = msg["content"].as_str().unwrap_or("").trim().to_string();
                trim(&mut history);
                self.db.save_history(chat_id, &history)?;
                let answer = if content.is_empty() {
                    "(модель вернула пустой ответ)".into()
                } else {
                    content
                };
                return Ok(with_note(answer, note));
            }

            for call in calls {
                let call_id = call["id"].as_str().unwrap_or("call").to_string();
                let name = call["function"]["name"].as_str().unwrap_or("").to_string();
                let raw = call["function"]["arguments"].as_str().unwrap_or("{}");
                let args = match serde_json::from_str::<Value>(raw) {
                    Ok(v) if v.is_object() => v,
                    _ => json!({"__unparsable_arguments": raw}),
                };

                let result = self
                    .run_tool(chat_id, &format!("e{event_id}/{step}"), &name, &args)
                    .await;
                eprintln!("tool {name} -> {}", crate::llm::brief(&result));
                history.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": clamp(&result.to_string(), MAX_TOOL_CHARS),
                }));
            }
            trim(&mut history);
        }

        self.db.save_history(chat_id, &history)?;
        Ok(with_note(
            format!("Не уложился в {MAX_STEPS} шагов, похоже на цикл. Уточни задачу или /clear."),
            note,
        ))
    }

    /// `/new`: close the active session, if any, and start clean.
    pub async fn new_session(&self, chat_id: i64) -> Result<String> {
        let (mut history, updated_at) = self.db.history(chat_id)?;
        history.retain(|m| m["role"] != "system");
        if history.is_empty() {
            self.clear_session(chat_id)?;
            return Ok("Контекст пуст — начинать заново нечего.".into());
        }
        match self
            .archive_current(chat_id, &history, updated_at.as_deref())
            .await?
        {
            Some(title) => Ok(format!(
                "Закрыл прошлый диалог в архив: «{title}». Начинаем новый."
            )),
            None => Ok("Прошлый контекст удалён. Начинаем новый.".into()),
        }
    }

    /// `/sessions resume <id>`. The archive row is checked before the active
    /// session is closed, so a typo never empties the chat.
    pub async fn resume_session(&self, chat_id: i64, id: i64) -> Result<String> {
        let Some(archived) = self.db.archived(chat_id, id)? else {
            return Ok(format!("Сессии #{id} нет. /sessions — список."));
        };
        let (mut history, updated_at) = self.db.history(chat_id)?;
        history.retain(|m| m["role"] != "system");
        if !history.is_empty() {
            self.archive_current(chat_id, &history, updated_at.as_deref())
                .await?;
        }
        if !self.db.resume_archived(chat_id, id)? {
            return Ok(format!("Сессии #{id} нет. /sessions — список."));
        }
        self.db.kv_set(&task_key(chat_id), &archived.info.task)?;
        self.forget_recap(chat_id)?;
        Ok(format!(
            "Открыл сессию «{}» ({}).",
            archived.info.title,
            date(&archived.info.updated_at)
        ))
    }

    async fn run_tool(&self, chat_id: i64, id: &str, name: &str, args: &Value) -> Value {
        if let Err(e) = policy::check(name, self.flags.safe()) {
            return json!({"ok": false, "error": format!("{e:#}")});
        }
        if name == "recall" {
            return recall(&self.db, chat_id, args);
        }
        match self.tools.call(id, name, args).await {
            Ok(v) => v,
            Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
        }
    }

    /// Does the new message belong to the active topic? Any failure means
    /// "yes": losing context is worse than carrying a stale one.
    async fn continues(&self, history: &[Value], text: &str, updated_at: Option<&str>) -> bool {
        let Some((question, answer)) = last_exchange(history) else {
            return true;
        };
        let now = Local::now();
        let last = updated_at
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Local));
        let when = match last {
            Some(t) => format!(
                "последний обмен был {} ({} назад)",
                t.format("%d.%m %H:%M"),
                human_gap(now.signed_duration_since(t))
            ),
            None => "время последнего обмена неизвестно".into(),
        };
        let prompt = format!(
            "Ты решаешь, продолжает ли новое сообщение пользователя текущий диалог.\n\
             Сейчас {}.\n{when}.\n\n\
             Последний вопрос: {}\n\
             Последний ответ: {}\n\n\
             Новое сообщение: {}\n\n\
             Ответь одним словом: CONTINUE или NEW.\n\
             CONTINUE — если сообщение ссылается на предыдущее, уточняет его, продолжает задачу \
             или отвечает агенту, даже если прошло много времени.\n\
             NEW — если это самостоятельная тема.",
            now.format("%d.%m %H:%M"),
            clamp(&question, PAIR_CHARS),
            clamp(&answer, PAIR_CHARS),
            clamp(text, MESSAGE_CHARS)
        );
        let msgs = [json!({"role": "user", "content": prompt})];
        match self.llm.chat(&msgs, None).await {
            Ok(r) => !is_new(r.message["content"].as_str().unwrap_or("")),
            Err(e) => {
                eprintln!("classifier failed, staying in context: {e:#}");
                true
            }
        }
    }

    /// Close the active session into the archive. Returns the title only when a
    /// row was actually written.
    async fn archive_current(
        &self,
        chat_id: i64,
        history: &[Value],
        updated_at: Option<&str>,
    ) -> Result<Option<String>> {
        if history.is_empty() {
            return Ok(None);
        }
        let last_activity = updated_at.unwrap_or_default();
        if events::expired(last_activity, Utc::now(), ARCHIVE_DAYS) {
            self.clear_session(chat_id)?;
            return Ok(None);
        }
        let task = match self.db.kv_get(&task_key(chat_id))? {
            Some(t) => t,
            None => first_user(history).unwrap_or_default(),
        };
        let final_answer = last_answer(history).unwrap_or_default();
        let title = self.title(&task, &final_answer).await;
        let last_activity = if last_activity.is_empty() {
            Utc::now().to_rfc3339()
        } else {
            last_activity.to_string()
        };
        self.db.archive(
            chat_id,
            &title,
            &task,
            &final_answer,
            history,
            &last_activity,
        )?;
        self.clear_session(chat_id)?;
        Ok(Some(title))
    }

    async fn title(&self, task: &str, final_answer: &str) -> String {
        let prompt = format!(
            "Дай короткое название сессии до 60 символов на языке пользователя. \
             Без кавычек и точки. Опирайся на задачу и итог.\n\nЗадача: {}\nИтог: {}",
            clamp(task, 800),
            clamp(final_answer, 800)
        );
        let msgs = [json!({"role": "user", "content": prompt})];
        let raw = match self.llm.chat(&msgs, None).await {
            Ok(r) => {
                let s = r.message["content"].as_str().unwrap_or("").trim();
                s.trim_matches(|c: char| c == '"' || c == '«' || c == '»' || c == '.')
                    .trim()
                    .chars()
                    .take(60)
                    .collect::<String>()
            }
            Err(e) => {
                eprintln!("title failed: {e:#}");
                String::new()
            }
        };
        if raw.is_empty() {
            task.trim().chars().take(60).collect()
        } else {
            raw
        }
    }

    /// Find the archived session a fresh message refers to, if any.
    async fn retrieve(&self, chat_id: i64, text: &str) -> Result<Option<Recap>> {
        let catalog = self.db.catalog(chat_id)?;
        if catalog.is_empty() {
            return Ok(None);
        }
        let candidates = rank_candidates(catalog, text);
        let list = candidates
            .iter()
            .map(|a| {
                format!(
                    "[{}] {} «{}» — задача: {} — итог: {}",
                    a.id,
                    date(&a.updated_at),
                    a.title,
                    clamp(&a.task, 300),
                    clamp(&a.final_answer, 300)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "Вот прошлые сессии ассистента. Определи, к какой из них относится новое сообщение.\n\
             Верни ровно `ID <n>` или `NONE`. Не угадывай.\n\n{list}\n\n\
             Новое сообщение: {}",
            clamp(text, MESSAGE_CHARS)
        );
        let msgs = [json!({"role": "user", "content": prompt})];
        let answer = match self.llm.chat(&msgs, None).await {
            Ok(r) => r.message["content"].as_str().unwrap_or("").to_string(),
            Err(e) => {
                eprintln!("retrieve failed: {e:#}");
                return Ok(None);
            }
        };
        let Some(info) =
            parse_id(&answer).and_then(|id| candidates.into_iter().find(|a| a.id == id))
        else {
            return Ok(None);
        };
        let text = clamp(
            &format!(
                "[Прошлая сессия #{} «{}», {}]\nЗадача: {}\nФинальный ответ: {}\n\
                 Детали — recall(session_id={}).",
                info.id,
                info.title,
                date(&info.updated_at),
                info.task,
                info.final_answer,
                info.id
            ),
            RECAP_CHARS,
        );
        Ok(Some(Recap { info, text }))
    }

    fn clear_session(&self, chat_id: i64) -> Result<()> {
        self.db.clear_history(chat_id)?;
        self.forget_recap(chat_id)?;
        self.db.kv_del(&task_key(chat_id))?;
        Ok(())
    }

    fn forget_recap(&self, chat_id: i64) -> Result<()> {
        self.db.kv_del(&recap_key(chat_id))?;
        self.db.kv_del(&recap_id_key(chat_id))
    }
}

fn with_note(answer: String, note: Option<String>) -> String {
    match note {
        Some(n) => format!("{answer}\n\n{n}"),
        None => answer,
    }
}

/// The newest question → answer pair. Tool messages and empty assistant
/// messages (tool-call-only turns) are skipped.
fn last_exchange(history: &[Value]) -> Option<(String, String)> {
    let question = history
        .iter()
        .rev()
        .filter(|m| m["role"] == "user")
        .find_map(|m| {
            let s = m["content"].as_str()?.trim();
            (!s.is_empty()).then(|| s.to_string())
        })?;
    let answer = last_answer(history)?;
    Some((question, answer))
}

fn last_answer(history: &[Value]) -> Option<String> {
    history
        .iter()
        .rev()
        .filter(|m| m["role"] == "assistant")
        .find_map(|m| {
            let s = m["content"].as_str()?.trim();
            (!s.is_empty()).then(|| s.to_string())
        })
}

fn first_user(history: &[Value]) -> Option<String> {
    history
        .iter()
        .filter(|m| m["role"] == "user")
        .find_map(|m| {
            let s = m["content"].as_str()?.trim();
            (!s.is_empty()).then(|| s.to_string())
        })
}

/// `true` only for a clear NEW. Anything else keeps the context.
fn is_new(answer: &str) -> bool {
    answer
        .trim()
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_uppercase()
        .starts_with("NEW")
}

/// `ID 12` → 12. Anything else (including `NONE`) → None.
fn parse_id(answer: &str) -> Option<i64> {
    let upper = answer.to_uppercase();
    let rest = &upper[upper.find("ID")? + 2..];
    rest.split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())?
        .parse()
        .ok()
}

fn human_gap(d: chrono::Duration) -> String {
    let minutes = d.num_minutes().max(0);
    if minutes < 60 {
        format!("{minutes} мин")
    } else if minutes < 24 * 60 {
        format!("{} ч {} мин", minutes / 60, minutes % 60)
    } else {
        format!("{} дн {} ч", minutes / 1440, (minutes % 1440) / 60)
    }
}

pub(crate) fn date(ts: &str) -> String {
    DateTime::parse_from_rfc3339(ts)
        .map(|t| t.with_timezone(&Local).format("%d.%m %H:%M").to_string())
        .unwrap_or_else(|_| ts.to_string())
}

fn words(text: &str) -> Vec<String> {
    let mut w: Vec<String> = text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.chars().count() > 3)
        .map(str::to_string)
        .collect();
    w.sort();
    w.dedup();
    w
}

fn score(a: &Archived, words: &[String]) -> usize {
    let title = a.title.to_lowercase();
    let task = a.task.to_lowercase();
    let final_answer = a.final_answer.to_lowercase();
    words
        .iter()
        .map(|w| {
            usize::from(title.contains(w)) * 3
                + usize::from(task.contains(w)) * 2
                + usize::from(final_answer.contains(w))
        })
        .sum()
}

/// Word overlap first, recency second. The catalog arrives newest-first, and
/// the sort is stable, so ties keep that order.
fn rank_candidates(mut catalog: Vec<Archived>, text: &str) -> Vec<Archived> {
    let words = words(text);
    if !words.is_empty() {
        catalog.sort_by_key(|a| std::cmp::Reverse(score(a, &words)));
    }
    catalog.truncate(CANDIDATES);
    catalog
}

/// Transcript for `recall`, system prompt dropped, newest messages kept when
/// the budget runs out.
fn transcript(history: &[Value]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut used = 0;
    for m in history.iter().rev() {
        let role = m["role"].as_str().unwrap_or("");
        if role == "system" {
            continue;
        }
        let content = m["content"].as_str().unwrap_or("").trim();
        if content.is_empty() {
            continue;
        }
        let line = clamp(&format!("[{role}] {content}"), TRANSCRIPT_CHARS);
        let len = line.chars().count() + 1;
        if used + len > TRANSCRIPT_CHARS && !lines.is_empty() {
            break;
        }
        used += len;
        lines.push(line);
    }
    lines.reverse();
    lines.join("\n")
}

fn summary(a: &Archived) -> Value {
    json!({
        "id": a.id,
        "date": date(&a.updated_at),
        "title": a.title,
        "task": clamp(&a.task, 500),
        "final_answer": clamp(&a.final_answer, 1_000),
    })
}

/// Archive access for the model. No sandbox, no network: SQLite only.
fn recall(db: &Db, chat_id: i64, args: &Value) -> Value {
    let default = || {
        db.kv_get(&recap_id_key(chat_id))
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i64>().ok())
    };
    let query = args["query"].as_str().filter(|q| !q.trim().is_empty());
    let id = args["session_id"]
        .as_i64()
        .or_else(|| if query.is_none() { default() } else { None });
    if let Some(id) = id {
        return match db.archived(chat_id, id) {
            Ok(Some(a)) => json!({
                "session_id": id,
                "date": date(&a.info.updated_at),
                "title": a.info.title,
                "task": a.info.task,
                "final_answer": a.info.final_answer,
                "transcript": transcript(&a.history),
            }),
            Ok(None) => json!({"ok": false, "error": format!("no session #{id}")}),
            Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
        };
    }
    let catalog = match db.catalog(chat_id) {
        Ok(c) => c,
        Err(e) => return json!({"ok": false, "error": format!("{e:#}")}),
    };
    let mut list = match query {
        Some(q) => rank_candidates(catalog, q),
        None => catalog,
    };
    list.truncate(RECALL_LIST);
    json!({"sessions": list.iter().map(summary).collect::<Vec<_>>()})
}

/// Keep the system prompt and the newest messages. A leading orphan `tool`
/// message would be rejected by the API, so drop those too.
fn trim(history: &mut Vec<Value>) {
    let keep_from = 1 + history[1..].len().saturating_sub(KEEP_MESSAGES);
    history.drain(1..keep_from);
    while history.len() > 1 && history[1]["role"] == "tool" {
        history.remove(1);
    }
}

fn clamp(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("… (обрезано)");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Db;
    use serde_json::json;

    fn archived(id: i64, title: &str, task: &str, final_answer: &str) -> Archived {
        Archived {
            id,
            title: title.into(),
            task: task.into(),
            final_answer: final_answer.into(),
            updated_at: "2026-11-14T22:15:00+00:00".into(),
        }
    }

    #[test]
    fn verdict_is_new_only_on_new() {
        assert!(is_new("NEW"));
        assert!(is_new(" new\n"));
        assert!(is_new("**NEW**"));
        assert!(!is_new("CONTINUE"));
        assert!(!is_new("continue\n"));
        assert!(!is_new(""));
        assert!(!is_new("не уверен"));
    }

    #[test]
    fn parses_id_and_rejects_none() {
        assert_eq!(parse_id("ID 12"), Some(12));
        assert_eq!(parse_id("id 7."), Some(7));
        assert_eq!(parse_id("ID 12, потому что"), Some(12));
        assert_eq!(parse_id("NONE"), None);
        assert_eq!(parse_id("нет"), None);
    }

    #[test]
    fn last_exchange_skips_tools_and_empty_answers() {
        let h = vec![
            json!({"role": "user", "content": "старый"}),
            json!({"role": "assistant", "content": "старый ответ"}),
            json!({"role": "user", "content": "почини docker"}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": "ok"}),
            json!({"role": "assistant", "content": "готово"}),
        ];
        assert_eq!(
            last_exchange(&h),
            Some(("почини docker".into(), "готово".into()))
        );
        assert_eq!(last_answer(&h).as_deref(), Some("готово"));
        assert_eq!(first_user(&h).as_deref(), Some("старый"));
    }

    #[test]
    fn ranking_prefers_titles_and_keeps_recency_on_ties() {
        let catalog = vec![
            archived(1, "Docker в /workspace", "помоги с докером", ""),
            archived(2, "Отчёт за ноябрь", "собери цифры", "docker не трогал"),
            archived(3, "Разное", "привет", ""),
        ];
        let ranked = rank_candidates(catalog, "как там docker?");
        assert_eq!(ranked[0].id, 1);
        assert_eq!(ranked[1].id, 2);
        assert_eq!(ranked[2].id, 3);
        assert_eq!(rank_candidates(ranked, "привет").len(), 3);
    }

    #[test]
    fn transcript_drops_system_and_keeps_newest() {
        let mut h = vec![json!({"role": "system", "content": "rules"})];
        for i in 0..50 {
            h.push(json!({"role": "user", "content": format!("message {i} {}", "x".repeat(400))}));
        }
        let t = transcript(&h);
        assert!(!t.contains("rules"));
        assert!(t.contains("message 49"));
        assert!(!t.contains("message 0 "));
        assert!(t.chars().count() <= TRANSCRIPT_CHARS + 100);
    }

    #[test]
    fn recall_reads_by_id_and_by_query() {
        let dir = std::env::temp_dir().join(format!("hermit-recall-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(dir.join("t.db").to_str().unwrap()).unwrap();
        let h = vec![
            json!({"role": "system", "content": "rules"}),
            json!({"role": "user", "content": "fix docker"}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let id = db
            .archive(
                7,
                "Docker fix",
                "fix docker",
                "done",
                &h,
                "2026-11-14T22:15:00+00:00",
            )
            .unwrap();

        let by_id = recall(&db, 7, &json!({"session_id": id}));
        let text = by_id["transcript"].as_str().unwrap();
        assert!(text.contains("fix docker"));
        assert!(!text.contains("rules"));

        let by_query = recall(&db, 7, &json!({"query": "docker"}));
        assert_eq!(by_query["sessions"][0]["id"], id);

        db.kv_set("recap_id:7", &id.to_string()).unwrap();
        let by_default = recall(&db, 7, &json!({}));
        assert_eq!(by_default["session_id"], id);
        let by_query = recall(&db, 7, &json!({"query": "docker"}));
        assert_eq!(
            by_query["sessions"][0]["id"], id,
            "query wins over recap_id"
        );

        db.kv_del("recap_id:7").unwrap();
        let list = recall(&db, 7, &json!({}));
        assert_eq!(list["sessions"].as_array().unwrap().len(), 1);

        assert!(recall(&db, 8, &json!({"session_id": id}))["ok"] == false);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn trim_keeps_system_and_drops_orphan_tool() {
        let mut h = vec![json!({"role":"system","content":"s"})];
        for i in 0..40 {
            h.push(json!({"role":"user","content":format!("u{i}")}));
        }
        h.push(json!({"role":"tool","content":"orphan"}));
        h.push(json!({"role":"user","content":"last"}));
        trim(&mut h);
        assert_eq!(h[0]["role"], "system");
        assert_ne!(h[1]["role"], "tool");
        assert_eq!(h.last().unwrap()["content"], "last");
        assert!(h.len() <= 31, "{}", h.len());
    }
}
