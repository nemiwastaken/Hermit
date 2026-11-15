use std::sync::atomic::{AtomicBool, Ordering};

use crate::events::Db;

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

/// Every slash command is answered here, before the model is called.
/// None means "not a slash command, hand it to the agent".
pub fn handle(text: &str, flags: &Flags, db: &Db, chat_id: i64) -> Option<String> {
    let t = text.trim();
    if !t.starts_with('/') {
        return None;
    }
    let mut it = t.split_whitespace();
    let cmd = it.next().unwrap_or("").to_lowercase();
    let cmd = cmd.split('@').next().unwrap_or("").to_string();
    let arg = it.next().unwrap_or("").to_lowercase();

    let reply = match cmd.as_str() {
        "/start" | "/help" => HELP.to_string(),
        "/pause" => {
            if flags.paused() {
                "Уже на паузе.".into()
            } else {
                flags.paused.store(true, Ordering::Relaxed);
                "Пауза. Новые сообщения копятся в очереди, инструменты не работают. /resume — продолжить.".into()
            }
        }
        "/resume" => {
            flags.paused.store(false, Ordering::Relaxed);
            "Продолжаю, разгребаю очередь.".into()
        }
        "/safe" => {
            let on = arg != "off";
            flags.safe.store(on, Ordering::Relaxed);
            if on {
                "Safe mode: инструменты выключены, отвечаю только текстом. /safe off — вернуть."
                    .into()
            } else {
                "Safe mode выключен, инструменты снова доступны.".into()
            }
        }
        "/clear" => match db.clear_history(chat_id) {
            Ok(()) => "Контекст этого чата очищен.".into(),
            Err(e) => format!("Не смог очистить: {e:#}"),
        },
        "/status" => format!(
            "пауза: {}\nsafe mode: {}\nочередь: {}",
            yesno(flags.paused()),
            yesno(flags.safe()),
            db.queue_len().unwrap_or(-1)
        ),
        "/rollback" | "/changes" => {
            "Появится на срезе 4: тогда у behavior/ будет git-история и откат.".into()
        }
        _ => format!("Неизвестная команда {cmd}.\n\n{HELP}"),
    };
    Some(reply)
}

fn yesno(v: bool) -> &'static str {
    if v { "да" } else { "нет" }
}

const HELP: &str = "/pause — остановить обработку\n\
/resume — продолжить\n\
/safe — выключить инструменты\n\
/safe off — вернуть инструменты\n\
/status — очередь, режимы\n\
/clear — забыть контекст чата\n\
/rollback, /changes — срез 4";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_and_ignores_plain_text() {
        let dir = std::env::temp_dir().join(format!("hermit-cmd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(dir.join("t.db").to_str().unwrap()).unwrap();
        let f = Flags::new(false);

        assert!(handle("привет", &f, &db, 1).is_none());
        assert!(
            handle("/status@hermit_bot", &f, &db, 1)
                .unwrap()
                .contains("очередь")
        );
        handle("/safe", &f, &db, 1);
        assert!(f.safe());
        handle("/safe off", &f, &db, 1);
        assert!(!f.safe());
        assert!(handle("/nope", &f, &db, 1).unwrap().contains("Неизвестная"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
