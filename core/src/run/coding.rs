use std::path::{Component, Path};
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

use super::*;

const TOOLS: &[&str] = &[
    "bash", "read", "write", "edit", "patch", "glob", "grep", "list",
];
const HELP: &str = "/project <имя> — выбрать репозиторий\n/code <задача>\n/code diff <номер>\n/code revise <номер> <уточнение>\n/code accept <номер>";

#[derive(Clone, Deserialize, Serialize)]
struct Project {
    repo: String,
    check: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct Job {
    project: Project,
    base: String,
    history: Vec<Value>,
    baseline: Option<String>,
    tree: Option<String>,
    report: Option<String>,
    accepted: Option<String>,
    event: i64,
    steps: usize,
    interrupted: bool,
}

fn projects() -> Result<std::collections::BTreeMap<String, Project>> {
    let path = std::env::var("CODING_PROJECTS").unwrap_or_else(|_| "projects.json".into());
    match std::fs::read(&path) {
        Ok(data) => serde_json::from_slice(&data).with_context(|| format!("reading {path}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(e) => Err(e.into()),
    }
}

fn location(id: i64) -> String {
    format!("/workspace/.hermit-code/{id}")
}

fn job_key(chat: i64, id: i64) -> String {
    format!("coding:{chat}:{id}")
}

impl Agent {
    pub fn select_project(&self, chat: i64, name: &str) -> Result<String> {
        let projects = projects()?;
        let key = format!("coding-project:{chat}");
        if name.is_empty() {
            let current = self.db.kv_get(&key)?.unwrap_or_else(|| "не выбран".into());
            let available = projects.keys().cloned().collect::<Vec<_>>().join(", ");
            return Ok(format!(
                "Проект: {current}\nДоступны: {available}\n\n{HELP}"
            ));
        }
        ensure!(
            projects.contains_key(name),
            "Неизвестный проект. Список — /project."
        );
        self.db.kv_set(&key, name)?;
        Ok(format!("Выбран проект {name}. Используй /code <задача>."))
    }

    pub async fn coding(
        &self,
        chat: i64,
        event: i64,
        request: &str,
    ) -> Result<(String, Option<i64>)> {
        if request.is_empty() {
            return Ok((HELP.into(), None));
        }
        ensure!(
            !self.flags.safe(),
            "Инструменты выключены. Используй /safe off."
        );
        let mut parts = request.splitn(3, char::is_whitespace);
        let action = parts.next().unwrap_or("");
        let existing = match action {
            "diff" | "revise" | "accept" => Some(
                parts
                    .next()
                    .and_then(|s| s.parse::<i64>().ok())
                    .context(HELP)?,
            ),
            _ => None,
        };
        let id = existing.unwrap_or(event);
        let key = job_key(chat, id);
        let cwd = location(id);
        let mut job: Job = match self.db.kv_get(&key)? {
            Some(data) => serde_json::from_str(&data)?,
            None => {
                ensure!(existing.is_none(), "Задача #{id} не найдена.");
                let name = self.db.kv_get(&format!("coding-project:{chat}"))?;
                let mut projects = projects()?;
                let name = name.or_else(|| {
                    (projects.len() == 1).then(|| projects.keys().next().unwrap().clone())
                });
                let project = name
                    .and_then(|n| projects.remove(&n))
                    .context("Выбери настроенный репозиторий через /project.")?;
                ensure!(
                    !project.check.is_empty() && !project.check[0].is_empty(),
                    "Проекту нужна команда check."
                );
                ensure!(
                    project.repo.starts_with("/workspace/") || project.repo.starts_with("/host/"),
                    "repo должен быть абсолютным путём внутри /workspace или /host"
                );
                let base = self
                    .code_git(&project.repo, &["rev-parse", "--verify", "HEAD"])
                    .await?;
                let system = "You are implementing a coding task in its dedicated Git worktree. Paths and commands default to its root. Read applicable AGENTS.md instructions, including in subdirectories you edit. Follow repository conventions. For a bug, reproduce it before fixing when possible, then rerun the same check. Run focused tests, inspect your diff, and explain what you verified. A configured project check runs separately after your work. Keep edits in this worktree. Leave changes for the user to review, without committing or pushing. Reply in the user's language.";
                let job = Job {
                    project,
                    base,
                    event,
                    steps: 0,
                    interrupted: false,
                    baseline: None,
                    tree: None,
                    report: None,
                    accepted: None,
                    history: vec![
                        json!({"role":"system", "content":system}),
                        json!({"role":"user","content":request}),
                    ],
                };
                self.save_job(&key, &job)?;
                job
            }
        };

        match action {
            "diff" => {
                let target = job.accepted.as_deref();
                let output = if let Some(commit) = target {
                    self.code_command(
                        &cwd,
                        &[
                            "git",
                            "diff",
                            "--no-ext-diff",
                            "--no-textconv",
                            "--binary",
                            &job.base,
                            commit,
                        ],
                        30_000,
                    )
                    .await?
                } else {
                    self.snapshot(&cwd).await?;
                    self.code_command(
                        &cwd,
                        &[
                            "git",
                            "diff",
                            "--cached",
                            "--no-ext-diff",
                            "--no-textconv",
                            "--binary",
                            &job.base,
                        ],
                        30_000,
                    )
                    .await?
                };
                ensure!(
                    output["ok"] == true,
                    "git diff: {}",
                    command_output(&output)
                );
                let diff = output["stdout"].as_str().unwrap_or("").trim_end();
                let diff = if diff.is_empty() {
                    "Изменений нет."
                } else {
                    diff
                };
                return Ok((format!("Задача #{id}\n{diff}"), None));
            }
            "accept" => {
                if let Some(commit) = &job.accepted {
                    return Ok((
                        format!("Задача #{id} принята. Коммит {commit}, ветка hermit/code-{id}."),
                        None,
                    ));
                }
                let head = self.code_git(&cwd, &["rev-parse", "HEAD"]).await?;
                let tree = job
                    .tree
                    .as_deref()
                    .context("Сначала заверши проверку задачи.")?;
                let verified = self.snapshot(&cwd).await?;
                ensure!(
                    tree == verified,
                    "Изменения появились после проверки. Запусти /code revise {id} и проверь заново."
                );
                let branch = format!("refs/heads/hermit/code-{id}");
                ensure!(
                    self.code_git(&cwd, &["symbolic-ref", "HEAD"]).await? == branch,
                    "Рабочая ветка изменилась."
                );
                if head == job.base {
                    let commit = self
                        .code_git(
                            &cwd,
                            &[
                                "-c",
                                "user.name=Hermit",
                                "-c",
                                "user.email=hermit@localhost",
                                "commit-tree",
                                tree,
                                "-p",
                                &job.base,
                                "-m",
                                &format!("Hermit task #{id}"),
                            ],
                        )
                        .await?;
                    self.code_git(&cwd, &["update-ref", &branch, &commit, &job.base])
                        .await?;
                } else {
                    let parent = self.code_git(&cwd, &["rev-parse", "HEAD^"]).await?;
                    let committed_tree = self.code_git(&cwd, &["rev-parse", "HEAD^{tree}"]).await?;
                    ensure!(
                        parent == job.base && committed_tree == tree,
                        "HEAD изменился после проверки."
                    );
                }
                ensure!(
                    self.code_git(&cwd, &["symbolic-ref", "HEAD"]).await? == branch,
                    "Рабочая ветка изменилась."
                );
                let commit = self.code_git(&cwd, &["rev-parse", "HEAD"]).await?;
                job.accepted = Some(commit.clone());
                self.save_job(&key, &job)?;
                return Ok((
                    format!("Задача #{id} принята. Коммит {commit}, ветка hermit/code-{id}."),
                    None,
                ));
            }
            "revise" if job.event != event => {
                ensure!(
                    job.accepted.is_none(),
                    "Задача уже принята. Начни новую через /code."
                );
                let note = parts
                    .next()
                    .filter(|s| !s.trim().is_empty())
                    .context(HELP)?;
                job.history.push(json!({"role":"user","content":note}));
                job.event = event;
                job.steps = 0;
                job.report = None;
                job.tree = None;
                self.save_job(&key, &job)?;
            }
            _ => {}
        }
        if let Some(report) = &job.report {
            return Ok((report.clone(), Some(id)));
        }

        let branch = format!("hermit/code-{id}");
        let current = self
            .tools
            .call_once(
                "code-worktree",
                "read",
                &json!({"path":format!("{cwd}/.git")}),
            )
            .await?;
        if current["ok"] != true {
            self.code_git(
                &job.project.repo,
                &[
                    "-c",
                    "core.hooksPath=/dev/null",
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    &cwd,
                    &job.base,
                ],
            )
            .await?;
        }
        ensure!(
            self.code_git(&cwd, &["rev-parse", "HEAD"]).await? == job.base
                && self.code_git(&cwd, &["branch", "--show-current"]).await? == branch,
            "Рабочая ветка или исходный коммит изменились."
        );
        if job.baseline.is_none() {
            if let Ok(result) = self
                .tools
                .call_once(
                    "code-instructions",
                    "read",
                    &json!({"path":format!("{cwd}/AGENTS.md"),"limit":500}),
                )
                .await
                && let Some(text) = result["content"].as_str()
            {
                job.history[0]["content"] = json!(format!(
                    "{}\n\nProject AGENTS.md:\n{}",
                    job.history[0]["content"].as_str().unwrap_or(""),
                    clamp(text, 12_000)
                ));
            }
            let (status, output) = self.code_check(&cwd, &job.project).await?;
            job.history.push(json!({"role":"user","content":format!("Configured baseline check: {status}\n{output}")}));
            job.baseline = Some(status);
            self.save_job(&key, &job)?;
        }
        if job.interrupted {
            job.history.push(json!({"role":"user","content":"The preceding tool batch was interrupted. Inspect git status/diff and the relevant files before deciding which steps still need to run."}));
            job.interrupted = false;
            self.save_job(&key, &job)?;
        }

        let schema = policy::schema(&[]);
        let tools = json!(
            schema
                .as_array()
                .unwrap()
                .iter()
                .filter(|t| {
                    t["function"]["name"]
                        .as_str()
                        .is_some_and(|n| TOOLS.contains(&n))
                })
                .collect::<Vec<_>>()
        );
        let deadline = Instant::now() + Duration::from_secs(900);
        let mut answer = "Достигнут лимит работы. Продолжить можно через «Доработать».".to_owned();
        while job.steps < 40 && Instant::now() < deadline {
            let reply = self
                .chat(chat, &job.history, Some(&tools), None)
                .await?
                .message;
            let calls = reply["tool_calls"].as_array().cloned().unwrap_or_default();
            if calls.is_empty() {
                if let Some(text) = reply["content"].as_str().filter(|s| !s.trim().is_empty()) {
                    answer = text.into();
                }
                job.history.push(reply);
                break;
            }

            job.interrupted = true;
            job.steps += 1;
            self.save_job(&key, &job)?;
            job.history.push(reply);
            for (index, value) in calls.iter().enumerate() {
                let call = parse_call(value);
                let result = if Instant::now() < deadline {
                    self.code_tool(
                        &cwd,
                        &format!("code-{id}-{event}-{}-{index}", job.steps),
                        call.name.as_str(),
                        call.args,
                    )
                    .await
                } else {
                    json!({"ok":false,"error":"Time budget reached"})
                };
                job.history.push(json!({"role":"tool","tool_call_id":call.id,"content":clamp(&result.to_string(), MAX_TOOL_CHARS)}));
            }
            job.interrupted = false;
            // Trim whole tool batches while retaining the system prompt and original task.
            while job.history.len() > 38 && job.history.len() > 3 {
                let end = job.history[3..]
                    .iter()
                    .position(|m| m["role"] != "tool")
                    .map_or(job.history.len(), |n| n + 3);
                if end == job.history.len() {
                    break;
                }
                job.history.drain(2..end);
            }
            self.save_job(&key, &job)?;
        }
        let before = self.snapshot(&cwd).await?;
        let (status, output) = self.code_check(&cwd, &job.project).await?;
        let after = self.snapshot(&cwd).await?;
        let base_tree = self
            .code_git(&cwd, &["rev-parse", &format!("{}^{{tree}}", job.base)])
            .await?;
        let summary = self
            .code_git(&cwd, &["diff", "--cached", "--stat", &job.base])
            .await?;
        let changed_during_check = before != after;
        job.tree = (status == "прошла" && before == after && after != base_tree).then_some(after);
        let changed = if changed_during_check {
            "\nПроверка изменила файлы. Повтори проверку перед принятием."
        } else {
            ""
        };
        let output = if status == "прошла" {
            String::new()
        } else {
            format!("\n{}", clamp(&output, 1500))
        };
        let report = format!(
            "Задача #{id} · {branch}\n\n{}\n\nПроверка: {}\nДо правки: {}\nПосле правки: {status}{changed}{output}\n\n{}",
            clamp(&answer, 3000),
            job.project.check.join(" "),
            job.baseline.as_deref().unwrap_or("—"),
            if summary.is_empty() {
                "Изменений нет."
            } else {
                &summary
            }
        );
        job.report = Some(report.clone());
        self.save_job(&key, &job)?;
        Ok((report, Some(id)))
    }

    fn save_job(&self, key: &str, job: &Job) -> Result<()> {
        self.db.kv_set(key, &serde_json::to_string(job)?)
    }

    async fn code_tool(&self, cwd: &str, id: &str, name: &str, mut args: Value) -> Value {
        if self.flags.safe()
            || !TOOLS.contains(&name)
            || policy::approval_reason(name, &args).is_some()
        {
            return json!({"ok":false,"error":"Tool is unavailable for this coding task"});
        }
        if !args.is_object() {
            return json!({"ok":false,"error":"Expected tool arguments as an object"});
        }
        let field = if name == "bash" { "cwd" } else { "path" };
        let path = Path::new(args[field].as_str().unwrap_or("."));
        let path = if path.is_absolute() {
            match path.strip_prefix(cwd) {
                Ok(rel) => rel,
                Err(_) => return json!({"ok":false,"error":"Use a path in the task worktree"}),
            }
        } else {
            path
        };
        if path
            .components()
            .any(|c| matches!(c, Component::ParentDir) || c.as_os_str() == ".git")
        {
            return json!({"ok":false,"error":"Use a path in the task worktree"});
        }
        args[field] = json!(Path::new(cwd).join(path));
        if name == "bash" {
            args["timeout_ms"] = json!(args["timeout_ms"].as_u64().unwrap_or(30_000).min(180_000));
        }
        match self.tools.call_once(id, name, &args).await {
            Ok(result) => {
                // Search tools return workspace-relative paths.
                let workspace_prefix = cwd.trim_start_matches("/workspace/");
                let mut result = result;
                if let Some(matches) = result["matches"].as_array_mut() {
                    for entry in matches {
                        let path = if name == "grep" {
                            &mut entry["path"]
                        } else {
                            entry
                        };
                        if let Some(rel) = path
                            .as_str()
                            .and_then(|s| s.strip_prefix(workspace_prefix))
                            .and_then(|s| s.strip_prefix('/'))
                        {
                            *path = json!(rel);
                        }
                    }
                }
                result
            }
            Err(e) => json!({"ok":false,"error":format!("{e:#}")}),
        }
    }

    async fn code_command(&self, cwd: &str, cmd: &[&str], timeout: u64) -> Result<Value> {
        self.tools
            .call_once(
                "code-command",
                "bash",
                &json!({"cmd":cmd,"cwd":cwd,"timeout_ms":timeout}),
            )
            .await
    }

    async fn code_git(&self, cwd: &str, args: &[&str]) -> Result<String> {
        let cmd = [&["git"][..], args].concat();
        let r = self.code_command(cwd, &cmd, 30_000).await?;
        ensure!(r["ok"] == true, "git {}: {}", args[0], command_output(&r));
        Ok(r["stdout"].as_str().unwrap_or("").trim().into())
    }

    async fn snapshot(&self, cwd: &str) -> Result<String> {
        self.code_git(cwd, &["add", "-A", "--", "."]).await?;
        self.code_git(cwd, &["write-tree"]).await
    }

    async fn code_check(&self, cwd: &str, project: &Project) -> Result<(String, String)> {
        let argv = project.check.iter().map(String::as_str).collect::<Vec<_>>();
        let result = self.code_command(cwd, &argv, 600_000).await?;
        let status = match result["exit"].as_i64() {
            Some(0) => "прошла".into(),
            Some(code) => format!("завершилась с кодом {code}"),
            None => "не завершилась".into(),
        };
        Ok((status, clamp(&command_output(&result), MAX_TOOL_CHARS)))
    }
}

fn command_output(result: &Value) -> String {
    ["stdout", "stderr", "error"]
        .into_iter()
        .filter_map(|k| result[k].as_str())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}
