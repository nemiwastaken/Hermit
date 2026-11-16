mod behavior;
mod cmd;
mod events;
mod llm;
mod mcp;
mod md;
mod metrics;
mod policy;
mod prompt;
mod run;
mod sandbox;
mod sched;
mod telegram;

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;

use cmd::{Cmd, Flags};
use events::{ARCHIVE_DAYS, Db};
use run::{APPROVAL_MINUTES, Agent};
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
    let socket = env_or("SANDBOX_SOCKET", "/opt/hermit/run/sandbox/sandbox.sock");
    let net_socket = env_or("NET_SOCKET", "/opt/hermit/run/net/net.sock");
    let behavior_dir = env_or("BEHAVIOR_DIR", "./behavior");

    let db = Db::open(&db_path)?;
    let recovered = db.recover_running()?;
    if recovered > 0 {
        eprintln!("requeued {recovered} event(s) left running by the last shutdown");
    }
    let flags = Arc::new(Flags::new(safe_mode));
    let tg = Arc::new(Tg::from_env()?);
    let net = Sandbox::with_timeout(&net_socket, 60);
    // MCP discovery runs before Telegram polling with a short timeout so a
    // slow server cannot hold the bot hostage at boot.
    let mcp_net = Sandbox::with_timeout(&net_socket, 15);
    let initial = mcp::load(&mcp_net).await.unwrap_or_default();
    eprintln!("mcp: {} tool(s)", initial.len());
    let mcp = Arc::new(RwLock::new(initial));
    let agent = Arc::new(Agent {
        llm: llm::Llm::from_env()?,
        tools: Sandbox::new(&socket),
        net,
        mcp: mcp.clone(),
        db: db.clone(),
        flags: flags.clone(),
        behavior_dir,
        tg: tg.clone(),
    });

    eprintln!(
        "hermit core up: db={db_path} sandbox={socket} net={net_socket} safe_mode={}",
        flags.safe()
    );

    // At boot net may not be listening yet, and `compose restart net` replaces
    // its list; without this the tools would stay missing or stale forever.
    tokio::spawn(refresh_mcp(
        mcp,
        Sandbox::with_timeout(net_socket.clone(), 60),
    ));
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
            match db.approval_purge(APPROVAL_MINUTES) {
                Ok(0) => {}
                Ok(n) => eprintln!("purged {n} stale approval(s)"),
                Err(e) => eprintln!("approval_purge: {e:#}"),
            }
            for (label, result) in [("event", db.purge_done(30)), ("usage", db.purge_usage(365))] {
                match result {
                    Ok(0) => {}
                    Ok(n) => eprintln!("purged {n} old {label} row(s)"),
                    Err(e) => eprintln!("purge {label}: {e:#}"),
                }
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

        let mut ask_after: Option<String> = None;
        let typing = tg.typing_loop(chat_id);
        let outcome: Result<(String, Option<i64>)> = match ev.kind.as_str() {
            "approval" => {
                let id = payload["id"].as_i64().unwrap_or(0);
                let approve = payload["approve"].as_bool().unwrap_or(false);
                agent.resume(chat_id, id, approve).await
            }
            // Fixed message, no model. Waiting for an answer starts only after
            // it is delivered, so a failed send cannot swallow the next message.
            "question" => {
                ask_after = payload["key"].as_str().map(str::to_string);
                Ok((text.clone(), None))
            }
            "report" => Ok((
                metrics::report(
                    &db,
                    chat_id,
                    payload["key"].as_str().unwrap_or(""),
                    payload["days"].as_i64().unwrap_or(7),
                    &text,
                ),
                None,
            )),
            // The reply to a fixed question is parsed before the model sees it.
            // telegram.rs pushes this kind; "telegram" was an old name.
            "message" => match sched::answer(&db, chat_id, &text) {
                Some(answer) => Ok((answer, None)),
                None => usual(&agent, &flags, &db, &tg, chat_id, &text, ev.id).await,
            },
            _ => usual(&agent, &flags, &db, &tg, chat_id, &text, ev.id).await,
        };

        typing.abort();
        match outcome {
            Ok((answer, approval)) => {
                // An empty answer means the streamer already delivered it.
                if !answer.is_empty()
                    && let Err(e) = send_with_retry(&tg, chat_id, &answer, approval).await
                {
                    // The run succeeded but nothing reached the chat. Park it
                    // so /dlq shows it instead of finishing as if delivered.
                    eprintln!("reply to {chat_id}: {e:#}");
                    if let Err(e) = db.fail(ev.id, &format!("send: {e:#}")) {
                        eprintln!("fail event {}: {e:#}", ev.id);
                    }
                    continue;
                }
                if let Some(key) = ask_after
                    && let Err(e) = sched::set_pending(&db, chat_id, &key)
                {
                    eprintln!("set ask: {e:#}");
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

/// One send with two retries: a short 429 must not lose a ready answer.
async fn send_with_retry(tg: &Tg, chat_id: i64, text: &str, approval: Option<i64>) -> Result<()> {
    let mut attempt = 0u64;
    loop {
        let result = match approval {
            Some(id) => tg.send_approval(chat_id, text, id).await,
            None => tg.send(chat_id, text).await,
        };
        match result {
            Err(e) if attempt < 2 => {
                attempt += 1;
                eprintln!("send retry {attempt}: {e:#}");
                tokio::time::sleep(Duration::from_secs(2 * attempt)).await;
            }
            other => return other,
        }
    }
}

/// Keep the MCP tool list current: net may come up after core, and a config
/// change only takes effect after `compose restart net`.
async fn refresh_mcp(mcp: Arc<RwLock<Vec<mcp::McpTool>>>, net: Sandbox) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        if let Some(tools) = mcp::load(&net).await {
            *mcp.write().unwrap_or_else(|e| e.into_inner()) = tools;
        }
    }
}

/// Commands and the agent. A scheduled `prompt` lands here too.
async fn usual(
    agent: &Agent,
    flags: &Flags,
    db: &Db,
    tg: &Tg,
    chat_id: i64,
    text: &str,
    event_id: i64,
) -> Result<(String, Option<i64>)> {
    match cmd::handle(text, flags, db, chat_id, &agent.behavior_dir) {
        // Data from SQLite and git is not markdown: no bold headings or
        // interpreted asterisks in /memory, /sessions or /usage.
        Some(Cmd::Answer(answer)) => {
            tg.send_plain(chat_id, &answer).await?;
            Ok((String::new(), None))
        }
        Some(Cmd::NewSession) => agent.new_session(chat_id).await.map(|s| (s, None)),
        Some(Cmd::ResumeSession(id)) => agent.resume_session(chat_id, id).await.map(|s| (s, None)),
        Some(Cmd::Compress) => agent.compress(chat_id).await.map(|s| (s, None)),
        None => agent.reply(event_id, chat_id, text).await,
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
