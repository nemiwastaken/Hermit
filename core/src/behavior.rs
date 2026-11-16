use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Same caps as prompt.rs: a longer file would be silently truncated there.
pub const PROFILE_CHARS: usize = 4_000;
pub const SKILL_CHARS: usize = 2_000;
pub const MEMORY_CHARS: usize = 6_000;
/// Commits made by the agent carry this author, so /rollback knows what it may undo.
const AUTHOR: &str = "Hermit";

pub fn update(dir: &str, path: &str, content: &str, note: Option<&str>) -> Result<String> {
    let cap = match path {
        "USER.md" | "user_profile.md" => PROFILE_CHARS,
        "MEMORY.md" => MEMORY_CHARS,
        p if skill_path(p) => SKILL_CHARS,
        _ => bail!("only USER.md, user_profile.md, MEMORY.md and skills/<name>.md can be changed"),
    };
    let len = content.chars().count();
    if len > cap {
        bail!("{path} is {len} chars, max {cap}");
    }

    let full = Path::new(dir).join(path);
    git(dir, &["rev-parse", "--git-dir"])
        .with_context(|| format!("{dir} is not a git repository"))?;
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        own_like(parent, Path::new(dir));
    }
    let old = fs::read(&full).ok();
    fs::write(&full, content).with_context(|| format!("writing {}", full.display()))?;
    own_like(&full, Path::new(dir));

    let changed = git(dir, &["status", "--porcelain", "--", path])?;
    if changed.trim().is_empty() {
        return Ok(format!("{path}: без изменений, коммит не нужен."));
    }

    let msg = format!("behavior: {}", note.unwrap_or(path));
    let msg = one_line(&msg);
    let written = git(dir, &["add", "--", path]).and_then(|_| {
        git(
            dir,
            &[
                "-c",
                &format!("user.name={AUTHOR}"),
                "-c",
                "user.email=hermit@localhost",
                "commit",
                "-m",
                &msg,
                // commit --only: never sweep up what the user staged themselves
                "--",
                path,
            ],
        )
    });
    if let Err(e) = written {
        restore(&full, old);
        return Err(e);
    }
    let hash = git(dir, &["rev-parse", "--short", "HEAD"])?;
    Ok(format!(
        "Записал {path} ({len} символов), коммит {}.",
        hash.trim()
    ))
}

/// Undo the newest agent change that is not already undone, with a revert
/// commit. Repeated calls walk further back instead of toggling.
pub fn rollback(dir: &str) -> Result<String> {
    let log = git(dir, &["log", "-200", "--format=%H%x1f%an%x1f%s%x1f%b%x1e"])?;
    let mut reverted = std::collections::HashSet::new();
    let mut entries = Vec::new();
    for entry in log.split('\u{1e}') {
        let mut f = entry.trim().split('\u{1f}');
        let (Some(hash), Some(author), Some(subject), Some(body)) =
            (f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        entries.push((hash.to_string(), author.to_string(), subject.to_string()));
        for line in body.lines() {
            if let Some(rest) = line.trim().strip_prefix("This reverts commit ") {
                reverted.insert(rest.trim().trim_end_matches('.').to_string());
            }
        }
    }
    let Some((hash, _, subject)) = entries.iter().find(|(h, author, subject)| {
        author == AUTHOR && subject.starts_with("behavior: ") && !reverted.contains(h)
    }) else {
        bail!("нечего откатывать: агент ещё ничего не менял");
    };
    git(
        dir,
        &[
            "-c",
            &format!("user.name={AUTHOR}"),
            "-c",
            "user.email=hermit@localhost",
            "revert",
            "--no-edit",
            hash,
        ],
    )?;
    let short = git(dir, &["rev-parse", "--short", "HEAD"])?;
    Ok(format!(
        "Откатил «{}» отдельным коммитом {}.",
        subject,
        short.trim()
    ))
}

pub fn changes(dir: &str) -> Result<String> {
    let log = git(dir, &["log", "--oneline", "-10"])?;
    let dirty = git(dir, &["status", "--short", "--untracked-files=no"])?;
    let mut out = format!("Последние коммиты:\n{}", log.trim_end());
    if !dirty.trim().is_empty() {
        out.push_str("\n\nНезакоммиченные правки:\n");
        out.push_str(dirty.trim_end());
    }
    Ok(out)
}

/// The profile is USER.md in new installs and user_profile.md in older ones.
/// If both exist, the newer one wins, so an agent that writes USER.md on an
/// upgraded install is not ignored.
pub fn profile_file(dir: &str) -> &'static str {
    let new = Path::new(dir).join("USER.md");
    let old = Path::new(dir).join("user_profile.md");
    match (new.exists(), old.exists()) {
        (true, true) => {
            let mtime = |p: &Path| p.metadata().and_then(|m| m.modified()).ok();
            if mtime(&new) >= mtime(&old) {
                "USER.md"
            } else {
                "user_profile.md"
            }
        }
        (true, false) => "USER.md",
        _ => "user_profile.md",
    }
}

/// What the agent currently has in its profile, memory and skills.
pub fn memory(dir: &str) -> Result<String> {
    let profile = fs::read_to_string(Path::new(dir).join(profile_file(dir))).unwrap_or_default();
    let profile = profile.trim();
    let mut out = String::from("Профиль:\n");
    if profile.is_empty() {
        out.push_str("(пусто)");
    } else {
        out.push_str(&clamp(profile, 1_500));
    }

    let stored = fs::read_to_string(Path::new(dir).join("MEMORY.md")).unwrap_or_default();
    out.push_str("\n\nПамять:\n");
    if stored.trim().is_empty() {
        out.push_str("(пусто)");
    } else {
        out.push_str(&clamp(stored.trim(), 2_000));
    }

    let mut skills: Vec<String> = fs::read_dir(Path::new(dir).join("skills"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n.ends_with(".md"))
        .collect();
    skills.sort();
    out.push_str("\n\nСкиллы: ");
    if skills.is_empty() {
        out.push_str("нет");
    } else {
        out.push_str(&skills.join(", "));
    }
    Ok(out)
}

/// One skill body, capped like the prompt used to cap it. The name is checked
/// before it touches the filesystem.
pub fn read_skill(dir: &str, name: &str) -> Result<String> {
    let name = name.trim().trim_end_matches(".md");
    if name.is_empty() || !skill_path(&format!("skills/{name}.md")) {
        bail!("unknown skill `{name}`");
    }
    let path = Path::new(dir).join("skills").join(format!("{name}.md"));
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(clamp(text.trim(), SKILL_CHARS))
}

/// Store a durable fact in MEMORY.md. Everything goes through update(), so it
/// is committed and /rollback can undo it.
pub fn remember(dir: &str, action: &str, text: &str, old: Option<&str>) -> Result<String> {
    let path = Path::new(dir).join("MEMORY.md");
    let mut content = fs::read_to_string(&path).unwrap_or_default();
    if content.trim().is_empty() {
        content = "# Memory\n".into();
    }
    let text = text.trim();
    let action = action.trim();
    if text.is_empty() {
        bail!("text is empty");
    }
    match action {
        "add" => {
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&format!("- {text}\n"));
        }
        "replace" => {
            let old = old.unwrap_or("").trim();
            if old.is_empty() {
                bail!("old is required for replace");
            }
            let mut done = false;
            let mut lines: Vec<String> = Vec::new();
            for line in content.lines() {
                if !done && line.contains(old) {
                    lines.push(format!("- {text}"));
                    done = true;
                } else {
                    lines.push(line.to_string());
                }
            }
            if !done {
                bail!("no memory line contains `{old}`");
            }
            content = lines.join("\n") + "\n";
        }
        "remove" => {
            let mut done = false;
            let mut lines: Vec<String> = Vec::new();
            for line in content.lines() {
                if line.contains(text) {
                    done = true;
                } else {
                    lines.push(line.to_string());
                }
            }
            if !done {
                bail!("no memory line contains `{text}`");
            }
            content = lines.join("\n") + "\n";
        }
        _ => bail!("action must be add, replace or remove"),
    }
    update(
        dir,
        "MEMORY.md",
        &content,
        Some(&format!("memory: {action}")),
    )
}

fn skill_path(p: &str) -> bool {
    let Some(name) = p.strip_prefix("skills/") else {
        return false;
    };
    !name.is_empty()
        && name.ends_with(".md")
        && !name.contains('/')
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Write back the old file, or remove a file that did not exist before.
fn restore(full: &Path, old: Option<Vec<u8>>) {
    match old {
        Some(bytes) => {
            let _ = fs::write(full, bytes);
        }
        None => {
            let _ = fs::remove_file(full);
        }
    }
}

/// The service runs as root while behavior/ belongs to the login user; a file
/// created by root would be uneditable for them afterwards.
fn own_like(target: &Path, dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = fs::metadata(dir) {
            let _ = std::os::unix::fs::chown(target, Some(meta.uid()), Some(meta.gid()));
        }
    }
    #[cfg(not(unix))]
    let _ = (target, dir);
}

fn one_line(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect()
}

fn clamp(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!("{}…", s.chars().take(max).collect::<String>())
}

/// Safe.directory is needed because the repo belongs to the login user, not root.
fn git(dir: &str, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-c")
        .arg(format!("safe.directory={dir}"))
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_add_replace_remove_and_commit() {
        let dir = std::env::temp_dir().join(format!("hermit-mem-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        git(dir.to_str().unwrap(), &["init", "-q", "-b", "main"]).unwrap();
        fs::write(dir.join("USER.md"), "profile").unwrap();
        git(dir.to_str().unwrap(), &["add", "-A"]).unwrap();
        git(
            dir.to_str().unwrap(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "init",
            ],
        )
        .unwrap();

        remember(dir.to_str().unwrap(), "add", "likes tea", None).unwrap();
        assert!(
            fs::read_to_string(dir.join("MEMORY.md"))
                .unwrap()
                .contains("likes tea")
        );
        remember(
            dir.to_str().unwrap(),
            "replace",
            "likes coffee",
            Some("tea"),
        )
        .unwrap();
        let text = fs::read_to_string(dir.join("MEMORY.md")).unwrap();
        assert!(text.contains("likes coffee") && !text.contains("tea"));
        remember(dir.to_str().unwrap(), "remove", "coffee", None).unwrap();
        assert!(
            !fs::read_to_string(dir.join("MEMORY.md"))
                .unwrap()
                .contains("coffee")
        );
        assert!(
            fs::read_to_string(dir.join("MEMORY.md"))
                .unwrap()
                .contains("Memory")
        );

        fs::create_dir_all(dir.join("skills")).unwrap();
        fs::write(dir.join("skills/deploy.md"), "run make").unwrap();
        assert_eq!(
            read_skill(dir.to_str().unwrap(), "deploy").unwrap(),
            "run make"
        );
        assert!(read_skill(dir.to_str().unwrap(), "../x").is_err());
        fs::remove_dir_all(&dir).ok();
    }
    use std::path::PathBuf;

    fn repo(name: &str) -> (PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("hermit-beh-{name}-{}", std::process::id()));
        fs::create_dir_all(dir.join("skills")).unwrap();
        fs::write(dir.join("user_profile.md"), "seed profile\n").unwrap();
        fs::write(dir.join("skills/deploy.md"), "seed skill\n").unwrap();
        let d = dir.to_str().unwrap().to_string();
        git(&d, &["init", "-q", "-b", "main"]).unwrap();
        git(
            &d,
            &[
                "-c",
                "user.name=nemi",
                "-c",
                "user.email=nemi@localhost",
                "add",
                "-A",
            ],
        )
        .unwrap();
        git(
            &d,
            &[
                "-c",
                "user.name=nemi",
                "-c",
                "user.email=nemi@localhost",
                "commit",
                "-qm",
                "seed",
            ],
        )
        .unwrap();
        (dir, d)
    }

    #[test]
    fn update_validates_and_commits() {
        let (dir, d) = repo("update");

        let out = update(
            &d,
            "user_profile.md",
            "любит краткие ответы\n",
            Some("tone"),
        )
        .unwrap();
        assert!(out.contains("коммит"), "{out}");
        assert_eq!(
            fs::read_to_string(dir.join("user_profile.md")).unwrap(),
            "любит краткие ответы\n"
        );
        let log = git(&d, &["log", "-1", "--format=%an|%s"]).unwrap();
        assert_eq!(log.trim(), "Hermit|behavior: tone");

        let same = update(&d, "user_profile.md", "любит краткие ответы\n", None).unwrap();
        assert!(same.contains("без изменений"), "{same}");

        assert!(update(&d, "../etc/passwd", "x", None).is_err());
        assert!(update(&d, "skills/a/b.md", "x", None).is_err());
        assert!(update(&d, "README.md", "x", None).is_err());
        assert!(update(&d, "skills/notes.txt", "x", None).is_err());
        assert!(update(&d, "user_profile.md", &"x".repeat(PROFILE_CHARS + 1), None).is_err());

        let plain = std::env::temp_dir().join(format!("hermit-beh-plain-{}", std::process::id()));
        fs::create_dir_all(&plain).unwrap();
        assert!(update(plain.to_str().unwrap(), "user_profile.md", "x", None).is_err());
        assert!(
            !plain.join("user_profile.md").exists(),
            "nothing written outside a repo"
        );
        std::fs::remove_dir_all(&plain).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn new_skill_is_created_and_rollback_reverts_it() {
        let (dir, d) = repo("skill");
        update(&d, "skills/git.md", "always branch first\n", None).unwrap();
        assert!(dir.join("skills/git.md").exists());

        let out = rollback(&d).unwrap();
        assert!(out.contains("Откатил"), "{out}");
        assert!(
            !dir.join("skills/git.md").exists(),
            "revert removed the skill"
        );
        assert_eq!(
            fs::read_to_string(dir.join("user_profile.md")).unwrap(),
            "seed profile\n"
        );

        // The seed commit belongs to the user: the agent must not revert it.
        assert!(rollback(&d).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn changes_and_memory_are_readable() {
        let (dir, d) = repo("read");
        update(&d, "skills/deploy.md", "deploy with make\n", None).unwrap();

        let log = changes(&d).unwrap();
        assert!(log.contains("behavior: skills/deploy.md"), "{log}");
        let memory = memory(&d).unwrap();
        assert!(memory.contains("seed profile"), "{memory}");
        assert!(memory.contains("deploy.md"), "{memory}");

        fs::write(dir.join("user_profile.md"), "manual edit\n").unwrap();
        assert!(changes(&d).unwrap().contains("user_profile.md"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
