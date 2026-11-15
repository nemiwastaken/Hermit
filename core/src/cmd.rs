use std::sync::atomic::{AtomicBool, Ordering};

use crate::events::{self, Db};
use crate::run::date;

/// Process-wide switches. Deliberately not stored in the DB or `behavior/`:
/// a restart always comes back in a known state.
pub struct Flags {
    pub paused: AtomicBool,
    pub safe: AtomicBool,
}

impl Flags {
    pub fn new(safe: bool) -> Self {
        Self {
            paused: AtomicBool::new(false),
            safe: AtomicBool::new(safe),
        }
    }
    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
    pub fn safe(&self) -> bool {
        self.safe.load(Ordering::Relaxed)
    }
}

/// What the worker should do with a slash command.
pub enum Cmd {
    /// Answered on the spot, no model call.
    Answer(String),
    /// Close the active session into the archive and start a new one.
    NewSession,
    /// Archive the active session, then reopen the given archived one.
    ResumeSession(i64),
}

/// Every slash command is handled here, before the model is called.
/// None means "not a slash command, hand it to the agent".
pub fn handle(text: &str, flags: &Flags, db: &Db, chat_id: i64) -> Option<Cmd> {
    let t = text.trim();
    if !t.starts_with('/') {
        return None;
    }
    let mut it = t.split_whitespace();
    let cmd = it.next().unwrap_or("").to_lowercase();
    let cmd = cmd.split('@').next().unwrap_or("").to_string();
    let args: Vec<&str> = it.collect();

    Some(match cmd.as_str() {
        "/start" | "/help" => Cmd::Answer(HELP.into()),
        "/pause" => {
            if flags.paused() {
                Cmd::Answer("Уже на паузе.".into())
            } else {
                flags.paused.store(true, Ordering::Relaxed);
                Cmd::Answer(
                    "Пауза. Новые сообщения копятся в очереди, инструменты не работают. /resume — продолжить."
                        .into(),
                )
            }
        }
        "/resume" => {
            flags.paused.store(false, Ordering::Relaxed);
            Cmd::Answer("Продолжаю, разгребаю очередь.".into())
        }
        "/safe" => {
            let on = args.first().map(|a| a.to_lowercase()).as_deref() != Some("off");
            flags.safe.store(on, Ordering::Relaxed);
            Cmd::Answer(if on {
                "Safe mode: инструменты выключены, отвечаю только текстом. /safe off — вернуть."
                    .into()
            } else {
                "Safe mode выключен, инструменты снова доступны.".into()
            })
        }
        "/clear" => Cmd::Answer(clear(db, chat_id)),
        "/new" => Cmd::NewSession,
        "/sessions" => sessions(db, chat_id, &args),
        "/status" => {
            let context = db
                .history(chat_id)
                .map(|(h, _)| h.iter().filter(|m| m["role"] != "system").count())
                .unwrap_or(0);
            Cmd::Answer(format!(
                "пауза: {}\nsafe mode: {}\nочередь: {}\nконтекст: {context} сообщений\nархив: {} сессий",
                yesno(flags.paused()),
                yesno(flags.safe()),
                db.queue_len().unwrap_or(-1),
                db.archive_count(chat_id).unwrap_or(-1)
            ))
        }
        "/rollback" | "/changes" => {
            Cmd::Answer("Появится на срезе 4: тогда у behavior/ будет git-история и откат.".into())
        }
        _ => Cmd::Answer(format!("Неизвестная команда {cmd}.\n\n{HELP}")),
    })
}

fn clear(db: &Db, chat_id: i64) -> String {
    let res = db
        .clear_history(chat_id)
        .and_then(|_| db.kv_del(&events::task_key(chat_id)))
        .and_then(|_| db.kv_del(&events::recap_key(chat_id)))
        .and_then(|_| db.kv_del(&events::recap_id_key(chat_id)));
    match res {
        Ok(()) => "Контекст этого чата удалён, в архив он не попал. Архив не тронут.".into(),
        Err(e) => format!("Не смог очистить: {e:#}"),
    }
}

fn sessions(db: &Db, chat_id: i64, args: &[&str]) -> Cmd {
    match args.first().map(|a| a.to_lowercase()).as_deref() {
        None | Some("list") => Cmd::Answer(list(db, chat_id)),
        Some("resume") => match id_arg(args.get(1)) {
            Some(id) => Cmd::ResumeSession(id),
            None => Cmd::Answer("Нужен id: /sessions resume <id>".into()),
        },
        Some("delete") => match id_arg(args.get(1)) {
            Some(id) => Cmd::Answer(match db.delete_archived(chat_id, id) {
                Ok(true) => format!("Удалил сессию #{id}."),
                Ok(false) => format!("Сессии #{id} нет."),
                Err(e) => format!("Не смог удалить: {e:#}"),
            }),
            None => Cmd::Answer("Нужен id: /sessions delete <id>".into()),
        },
        Some("rename") => {
            let Some(id) = id_arg(args.get(1)) else {
                return Cmd::Answer("Нужен id и текст: /sessions rename <id> <текст>".into());
            };
            let title = args.get(2..).unwrap_or_default().join(" ");
            if title.trim().is_empty() {
                return Cmd::Answer("Нужен текст: /sessions rename <id> <текст>".into());
            }
            Cmd::Answer(match db.rename_archived(chat_id, id, title.trim()) {
                Ok(true) => format!("Переименовал #{id} в «{}».", title.trim()),
                Ok(false) => format!("Сессии #{id} нет."),
                Err(e) => format!("Не смог переименовать: {e:#}"),
            })
        }
        Some(other) => match other.parse::<i64>() {
            Ok(id) => Cmd::Answer(show(db, chat_id, id)),
            Err(_) => Cmd::Answer(format!("Не понимаю /sessions {other}.\n\n{HELP}")),
        },
    }
}

fn list(db: &Db, chat_id: i64) -> String {
    let mut out = String::new();
    match db.history(chat_id) {
        Ok((h, at)) => {
            let n = h.iter().filter(|m| m["role"] != "system").count();
            if n == 0 {
                out.push_str("Активной сессии нет.\n");
            } else {
                let at = at.map(|s| format!(", {}", date(&s))).unwrap_or_default();
                out.push_str(&format!("Активная: {n} сообщений{at}\n"));
            }
        }
        Err(e) => out.push_str(&format!("Активная: ошибка {e:#}\n")),
    }

    match db.recent_archived(chat_id, 10) {
        Ok(rows) => {
            out.push_str(&format!(
                "\nАрхив ({}):\n",
                db.archive_count(chat_id).unwrap_or(0)
            ));
            if rows.is_empty() {
                out.push_str("пусто\n");
            }
            for a in rows {
                out.push_str(&format!(
                    "#{} {} «{}»\n",
                    a.id,
                    date(&a.updated_at),
                    a.title
                ));
            }
        }
        Err(e) => out.push_str(&format!("\nАрхив: ошибка {e:#}\n")),
    }
    out.push_str(
        "\n/sessions <id> — детали, resume <id> — открыть, rename <id> <текст>, delete <id>.",
    );
    out
}

fn show(db: &Db, chat_id: i64, id: i64) -> String {
    match db.archived(chat_id, id) {
        Ok(Some(a)) => {
            let n = a.history.iter().filter(|m| m["role"] != "system").count();
            format!(
                "#{id} «{}»\n{} · {n} сообщений\n\nЗадача: {}\n\nФинальный ответ:\n{}",
                a.info.title,
                date(&a.info.updated_at),
                a.info.task,
                if a.info.final_answer.is_empty() {
                    "(пусто)"
                } else {
                    &a.info.final_answer
                }
            )
        }
        Ok(None) => format!("Сессии #{id} нет."),
        Err(e) => format!("Не смог прочитать: {e:#}"),
    }
}

fn id_arg(s: Option<&&str>) -> Option<i64> {
    s.and_then(|s| s.parse().ok())
}

fn yesno(v: bool) -> &'static str {
    if v { "да" } else { "нет" }
}

const HELP: &str = "/pause — остановить обработку\n\
/resume — продолжить\n\
/safe — выключить инструменты\n\
/safe off — вернуть инструменты\n\
/status — очередь, режимы, контекст и архив\n\
/new — закрыть диалог в архив и начать новый\n\
/clear — удалить контекст чата, не архивируя\n\
/sessions — сессии: list | <id> | resume | rename | delete\n\
/rollback, /changes — срез 4";

#[cfg(test)]
mod tests {
    use super::*;

    fn db(name: &str) -> (std::sync::Arc<Db>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("hermit-cmd-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(dir.join("t.db").to_str().unwrap()).unwrap();
        (db, dir)
    }

    #[test]
    fn parses_commands_and_ignores_plain_text() {
        let (db, dir) = db("basic");
        let f = Flags::new(false);

        assert!(handle("привет", &f, &db, 1).is_none());
        assert!(
            matches!(
                handle("/status@hermit_bot", &f, &db, 1),
                Some(Cmd::Answer(a)) if a.contains("архив")
            ),
            "status mentions the archive"
        );
        handle("/safe", &f, &db, 1);
        assert!(f.safe());
        handle("/safe off", &f, &db, 1);
        assert!(!f.safe());
        assert!(
            matches!(handle("/nope", &f, &db, 1), Some(Cmd::Answer(a)) if a.contains("Неизвестная"))
        );
        assert!(matches!(handle("/new", &f, &db, 1), Some(Cmd::NewSession)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sessions_subcommands_and_clear() {
        let (db, dir) = db("sessions");
        let f = Flags::new(false);
        let h = vec![serde_json::json!({"role": "user", "content": "fix docker"})];
        let id = db
            .archive(
                1,
                "Docker",
                "fix docker",
                "done",
                &h,
                "2026-11-14T22:15:00+00:00",
            )
            .unwrap();
        db.save_history(1, &h).unwrap();
        db.kv_set(&events::task_key(1), "fix docker").unwrap();
        db.kv_set(&events::recap_key(1), "recap").unwrap();

        let listed = match handle("/sessions", &f, &db, 1) {
            Some(Cmd::Answer(a)) => a,
            _ => panic!("list is answered directly"),
        };
        assert!(listed.contains("Docker"), "{listed}");

        assert!(
            matches!(handle(&format!("/sessions {id}"), &f, &db, 1), Some(Cmd::Answer(a)) if a.contains("fix docker"))
        );
        assert!(
            matches!(handle(&format!("/sessions resume {id}"), &f, &db, 1), Some(Cmd::ResumeSession(n)) if n == id)
        );
        assert!(
            matches!(handle("/sessions resume", &f, &db, 1), Some(Cmd::Answer(a)) if a.contains("Нужен id"))
        );
        assert!(
            matches!(handle("/sessions wat", &f, &db, 1), Some(Cmd::Answer(a)) if a.contains("Не понимаю"))
        );

        // rename keeps the case the user typed
        handle(&format!("/sessions rename {id} Docker Fix"), &f, &db, 1);
        assert_eq!(
            db.archived(1, id).unwrap().unwrap().info.title,
            "Docker Fix"
        );

        assert!(
            matches!(handle("/clear", &f, &db, 1), Some(Cmd::Answer(a)) if a.contains("удалён"))
        );
        assert!(db.history(1).unwrap().0.is_empty());
        assert_eq!(db.kv_get(&events::task_key(1)).unwrap(), None);
        assert_eq!(db.kv_get(&events::recap_key(1)).unwrap(), None);
        assert_eq!(db.archive_count(1).unwrap(), 1, "archive is untouched");

        assert!(
            matches!(handle(&format!("/sessions delete {id}"), &f, &db, 1), Some(Cmd::Answer(a)) if a.contains("Удалил"))
        );
        assert_eq!(db.archive_count(1).unwrap(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
