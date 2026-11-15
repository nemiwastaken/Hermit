mod behavior;
mod cmd;
mod events;
mod llm;
mod policy;
mod prompt;
mod run;
mod sandbox;
mod sched;
mod telegram;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;

use cmd::{Cmd, Flags};
use events::{ARCHIVE_DAYS, Db};
use run::Agent;
use sandbox::Sandbox;
use telegram::Tg;

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("hermit {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    dotenvy::dotenv().ok();
    let safe_mode = std::env::args().any(|a| a == "--safe-mode");
    let db_path = env_or("DATABASE_PATH", "./data/hermit.db");
    let socket = env_or("SANDBOX_SOCKET", "/opt/hermit/run/sandbox.sock");
    let behavior_dir = env_or("BEHAVIOR_DIR", "./behavior");

    let db = Db::open(&db_path)?;
    let recovered = db.recover_running()?;
    if recovered > 0 {
        eprintln!("requeued {recovered} event(s) left running by the last shutdown");
    }
    let flags = Arc::new(Flags::new(safe_mode));
    let tg = Arc::new(Tg::from_env()?);
    let agent = Arc::new(Agent {
        llm: llm::Llm::from_env()?,
        tools: Sandbox::new(&socket),
        db: db.clone(),
        flags: flags.clone(),
        behavior_dir,
    });

    eprintln!(
        "hermit core up: db={db_path} sandbox={socket} safe_mode={}",
        flags.safe()
    );

    let poller = tokio::spawn({
        let tg = tg.clone();
        let db = db.clone();
        async move { tg.poll_forever(db).await }
    });
    let worker = tokio::spawn(worker(agent, flags, db, tg));

    tokio::select! {
        _ = tokio::signal::ctrl_c() => eprintln!("ctrl-c, stopping"),
        r = poller => eprintln!("telegram task exited: {r:?}"),
        r = worker => eprintln!("worker exited: {r:?}"),
    }
    Ok(())
}

async fn worker(agent: Arc<Agent>, flags: Arc<Flags>, db: Arc<Db>, tg: Arc<Tg>) {
    let mut next_purge = Instant::now();
    let mut next_tick = Instant::now();
    loop {
        // Before the paused check on purpose: retention must work while paused.
        if Instant::now() >= next_purge {
            match db.purge_archive(ARCHIVE_DAYS) {
                Ok(0) => {}
                Ok(n) => eprintln!("purged {n} archived session(s) older than {ARCHIVE_DAYS} days"),
                Err(e) => eprintln!("purge_archive: {e:#}"),
            }
            next_purge = Instant::now() + Duration::from_secs(3600);
        }
        // Due reminders become ordinary queue events, so they also wait while paused.
        if Instant::now() >= next_tick {
            match sched::enqueue_due(&db) {
                Ok(0) => {}
                Ok(n) => eprintln!("queued {n} scheduled event(s)"),
                Err(e) => eprintln!("schedules: {e:#}"),
            }
            next_tick = Instant::now() + Duration::from_secs(60);
        }
        if flags.paused() {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        let ev = match db.claim_event() {
            Ok(Some(ev)) => ev,
            Ok(None) => {
                tokio::time::sleep(Duration::from_millis(300)).await;
                continue;
            }
            Err(e) => {
                eprintln!("claim_event: {e:#}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let payload: Value = serde_json::from_str(&ev.payload).unwrap_or(Value::Null);
        let chat_id = payload["chat_id"].as_i64().unwrap_or(0);
        let text = payload["text"].as_str().unwrap_or("").to_string();

        let outcome = match cmd::handle(&text, &flags, &db, chat_id, &agent.behavior_dir) {
            Some(Cmd::Answer(answer)) => Ok(answer),
            Some(Cmd::NewSession) => agent.new_session(chat_id).await,
            Some(Cmd::ResumeSession(id)) => agent.resume_session(chat_id, id).await,
            None => agent.reply(ev.id, chat_id, &text).await,
        };

        match outcome {
            Ok(answer) => {
                if let Err(e) = tg.send(chat_id, &answer).await {
                    eprintln!("reply to {chat_id}: {e:#}");
                }
                if let Err(e) = db.finish(ev.id) {
                    eprintln!("finish event {}: {e:#}", ev.id);
                }
            }
            Err(e) => {
                eprintln!("event {} (attempt {}): {e:#}", ev.id, ev.attempts);
                let _ = tg.send(chat_id, &format!("Ошибка: {e:#}")).await;
                if let Err(e) = db.fail(ev.id, &format!("{e:#}")) {
                    eprintln!("fail event {}: {e:#}", ev.id);
                }
            }
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
