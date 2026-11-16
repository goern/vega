//! `vega mux spawn --worktree`: get a git worktree for a branch and open it as
//! a herdr worktree workspace (nested under the repo in the sidebar).
//!
//! Worktrunk (`wt`) owns the layout and the lifecycle hooks (bootstrap), like
//! the herdr worktrunk plugin: `wt switch`, then `herdr worktree open`. vega
//! reimplements neither. Without `wt`, `herdr worktree create` is the fallback,
//! but only for repos without any worktrunk config: such a config states a
//! layout or hooks that herdr cannot honor, so vega fails instead.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;
use vega_herdr::{collapse_home, herdr, str_of, Failure};

/// A worktree workspace, ready for `herdr agent start`.
pub struct Opened {
    pub path: String,
    pub branch: String,
    pub workspace: String,
    pub pane: String,
    /// The tab the pane is in; its label carries the task.
    pub tab: String,
    /// `created from main via wt`, `reused via herdr`, …
    pub how: String,
}

#[derive(Debug, PartialEq)]
enum Backend {
    Wt,
    Herdr,
}

pub fn open(repo: &str, branch: &str, base: Option<&str>) -> Result<Opened, Failure> {
    let top = git(repo, &["rev-parse", "--show-toplevel"]).ok_or_else(|| {
        Failure::new(
            "not_a_repo",
            format!("{} is not inside a git repository", collapse_home(repo)),
            "pass --cwd <dir of the repo> to branch from",
        )
    })?;
    let backend = backend(
        wt_installed(),
        Path::new(&top).join(".config/wt.toml").exists(),
        user_config().exists(),
    )?;
    let (path, how) = match backend {
        Backend::Wt => wt_switch(&top, branch, base)?,
        Backend::Herdr => return herdr_create(&top, branch, base),
    };
    open_workspace(&path, branch, how)
}

/// hc needs only the worktree path; herdr-app owns its visual space.
pub fn hc_worktree(
    repo: &str,
    branch: &str,
    base: Option<&str>,
) -> Result<(String, String), Failure> {
    let top = git(repo, &["rev-parse", "--show-toplevel"]);
    let root = top.as_deref().unwrap_or(repo);
    match backend(
        wt_installed(),
        Path::new(root).join(".config/wt.toml").exists(),
        user_config().exists(),
    )? {
        Backend::Wt => wt_switch(repo, branch, base),
        // No wt and no worktrunk config: nothing to honor but the layout.
        Backend::Herdr => {
            let top = top.ok_or_else(|| {
                Failure::new(
                    "not_a_repo",
                    format!("{} is not inside a git repository", collapse_home(repo)),
                    "pass --cwd <dir of the repo> to branch from",
                )
            })?;
            git_worktree(&top, branch, base)
        }
    }
}

/// `<repo>.<branch>` next to the repo, wt's default layout; `/` in the branch
/// becomes `-`.
fn sibling_path(repo_name: &str, top: &Path, branch: &str) -> PathBuf {
    let dir = format!("{repo_name}.{}", branch.replace(['/', '\\'], "-"));
    top.parent().unwrap_or(top).join(dir)
}

/// The checkout path of `branch` in `git worktree list --porcelain` output.
fn worktree_of(porcelain: &str, branch: &str) -> Option<String> {
    let want = format!("branch refs/heads/{branch}");
    porcelain
        .split("\n\n")
        .find(|b| b.lines().any(|l| l == want))
        .and_then(|b| b.lines().find_map(|l| l.strip_prefix("worktree ")))
        .map(String::from)
}

/// The main checkout's directory name (`repo`, or `repo` for a bare `repo.git`).
fn repo_name(top: &str) -> String {
    let dir_name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
    git(
        top,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .and_then(|c| {
        let c = PathBuf::from(c);
        let root = if c.file_name().is_some_and(|n| n == ".git") {
            c.parent()?.to_path_buf()
        } else {
            c
        };
        dir_name(&root)
    })
    .or_else(|| dir_name(Path::new(top)))
    .unwrap_or_default()
    .trim_end_matches(".git")
    .to_string()
}

/// Plain `git worktree add`, idempotent: a worktree already holding the
/// branch is reused, an existing branch is checked out, else it is created
/// from `base` (default HEAD). No timeout: the checkout can take minutes.
fn git_worktree(top: &str, branch: &str, base: Option<&str>) -> Result<(String, String), Failure> {
    if let Some(list) = git(top, &["worktree", "list", "--porcelain"]) {
        if let Some(path) = worktree_of(&list, branch) {
            let how = match base {
                Some(_) => "reused via git; --base ignored, the branch exists",
                None => "reused via git",
            };
            return Ok((path, how.into()));
        }
    }
    let path = sibling_path(&repo_name(top), Path::new(top), branch)
        .display()
        .to_string();
    let exists = branch_exists(top, branch);
    let mut args = vec!["-C", top, "worktree", "add"];
    if exists {
        args.extend(["--", &path, branch]);
    } else {
        args.extend(["-b", branch, "--", &path]);
        if let Some(b) = base {
            args.push(b);
        }
    }
    let out = Command::new("git")
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Failure::new("git_failed", e.to_string(), "install git"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = err
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" / ");
        return Err(Failure::new(
            "git_worktree_failed",
            format!("git worktree add failed: {msg}"),
            "fix the cause above, or pick another --worktree branch",
        ));
    }
    let how = if exists {
        "created for the existing branch via git (no wt)".to_string()
    } else {
        format!("created from {} via git (no wt)", base.unwrap_or("HEAD"))
    };
    Ok((path, how))
}

/// wt when installed; else herdr, unless a worktrunk config asks for wt.
fn backend(wt: bool, project_config: bool, user_config: bool) -> Result<Backend, Failure> {
    match (wt, project_config || user_config) {
        (true, _) => Ok(Backend::Wt),
        (false, false) => Ok(Backend::Herdr),
        (false, true) => Err(Failure::new(
            "wt_not_found",
            "this repo has a worktrunk config (layout or hooks), but `wt` is not on PATH",
            "install worktrunk (https://worktrunk.dev), then spawn again",
        )),
    }
}

fn wt_installed() -> bool {
    Command::new("wt")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `~/.config/worktrunk/config.toml`, where `wt config show` finds it.
fn user_config() -> PathBuf {
    let base = env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env::var("HOME").unwrap_or_default()).join(".config"));
    base.join("worktrunk/config.toml")
}

/// `wt switch` the branch into a worktree; hooks run as configured. An existing
/// branch (local or on a remote) is switched to, never created. Returns the
/// worktree path and how it came to be.
fn wt_switch(repo: &str, branch: &str, base: Option<&str>) -> Result<(String, String), Failure> {
    let exists = branch_exists(repo, branch);
    let mut args = vec!["-C", repo, "switch"];
    if !exists {
        args.push("--create");
    }
    args.push(branch);
    if let (false, Some(b)) = (exists, base) {
        args.extend(["--base", b]);
    }
    // Never `--yes`: approving a project's hook commands is the user's call.
    args.extend(["--no-cd", "--format=json"]);
    let out = Command::new("wt")
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Failure::new("wt_not_found", e.to_string(), "install worktrunk"))?;
    let stderr = strip_ansi(&String::from_utf8_lossy(&out.stderr));
    let json: Option<Value> = serde_json::from_slice(&out.stdout).ok();
    let path = json.as_ref().map(|v| str_of(v, "path")).unwrap_or("");
    if !out.status.success() || path.is_empty() || path == "-" {
        return Err(wt_failure(&stderr, repo));
    }
    let v = json.as_ref().unwrap();
    let how = match (str_of(v, "action"), exists, base) {
        ("created", false, _) => format!("created from {} via wt", str_of(v, "base_branch")),
        ("created", true, _) => "created for the existing branch via wt".into(),
        (_, _, Some(_)) => "reused via wt; --base ignored, the branch exists".into(),
        _ => "reused via wt".into(),
    };
    Ok((path.to_string(), how))
}

/// Translates a failed `wt switch`: its error lines go into the message, the
/// progress noise is dropped.
fn wt_failure(stderr: &str, repo: &str) -> Failure {
    if stderr.contains("needs approval") {
        return Failure::new(
            "approval_required",
            "the repo's worktrunk hooks are not approved yet; no worktree was created",
            &format!(
                "ask the user to approve them with `wt -C {} config approvals add`; vega never approves hooks",
                collapse_home(repo)
            ),
        );
    }
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| {
            let git = l.starts_with("fatal:") || l.starts_with("error:");
            let hint = l.starts_with('↳') && !l.contains("Failed command");
            l.starts_with('✗') || git || hint
        })
        .collect();
    let message = if lines.is_empty() {
        stderr
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" / ")
    } else {
        lines.join(" / ")
    };
    Failure::new(
        "wt_failed",
        format!("wt switch failed: {message}"),
        "fix the cause above, or pick another --worktree branch",
    )
}

/// Local branch `b`, or `b` on any remote: `wt switch b` checks either out.
fn branch_exists(repo: &str, branch: &str) -> bool {
    git(
        repo,
        &[
            "for-each-ref",
            "--count=1",
            "--format=%(refname)",
            &format!("refs/heads/{branch}"),
            &format!("refs/remotes/*/{branch}"),
        ],
    )
    .is_some_and(|s| !s.is_empty())
}

/// Opens the worktree at `path` as a herdr worktree workspace under the repo's
/// root workspace, like the herdr worktrunk plugin. An already open workspace
/// gets a new tab: its root pane may host an agent or a shell in use.
fn open_workspace(path: &str, branch: &str, how: String) -> Result<Opened, Failure> {
    let list = herdr(&["worktree", "list", "--cwd", path, "--json"])?;
    let source = &list["result"]["source"];
    let root = str_of(source, "repo_root").to_string();
    // Without a root workspace herdr would label it after the checkout
    // directory (`repo.git` for a bare repo); create it with the repo's name.
    if source["source_workspace_id"].as_str().is_none() {
        let label = str_of(source, "repo_name").trim_end_matches(".git");
        herdr(&[
            "workspace",
            "create",
            "--cwd",
            &root,
            "--label",
            label,
            "--no-focus",
        ])?;
    }
    let v = herdr(&[
        "worktree",
        "open",
        "--cwd",
        &root,
        "--path",
        path,
        "--label",
        branch,
        "--no-focus",
        "--json",
    ])?;
    let r = &v["result"];
    let workspace = str_of(&r["workspace"], "workspace_id").to_string();
    let (pane, tab) = if r["already_open"].as_bool() == Some(true) {
        let t = herdr(&[
            "tab",
            "create",
            "--workspace",
            &workspace,
            "--cwd",
            path,
            "--no-focus",
        ])?;
        let p = &t["result"]["root_pane"];
        (
            str_of(p, "pane_id").to_string(),
            str_of(p, "tab_id").to_string(),
        )
    } else {
        let p = &r["root_pane"];
        (
            str_of(p, "pane_id").to_string(),
            str_of(p, "tab_id").to_string(),
        )
    };
    Ok(Opened {
        path: path.to_string(),
        branch: branch.to_string(),
        workspace,
        pane,
        tab,
        how,
    })
}

/// Without wt: herdr creates the worktree (its own layout,
/// `~/.herdr/worktrees/<repo>/<branch>`) and the workspace in one call. A
/// worktree herdr already has for the branch is reused.
fn herdr_create(repo: &str, branch: &str, base: Option<&str>) -> Result<Opened, Failure> {
    let list = herdr(&["worktree", "list", "--cwd", repo, "--json"])?;
    let existing = list["result"]["worktrees"]
        .as_array()
        .and_then(|ws| ws.iter().find(|w| w["branch"] == branch))
        .map(|w| str_of(w, "path").to_string());
    if let Some(path) = existing {
        let how = match base {
            Some(_) => "reused via herdr; --base ignored, the branch exists",
            None => "reused via herdr",
        };
        return open_workspace(&path, branch, how.into());
    }
    let mut args = vec![
        "worktree", "create", "--cwd", repo, "--branch", branch, "--label", branch,
    ];
    if let Some(b) = base {
        args.extend(["--base", b]);
    }
    args.extend(["--no-focus", "--json"]);
    let v = herdr(&args)?;
    let r = &v["result"];
    let p = &r["root_pane"];
    Ok(Opened {
        path: str_of(&r["worktree"], "path").to_string(),
        branch: branch.to_string(),
        workspace: str_of(&r["workspace"], "workspace_id").to_string(),
        pane: str_of(p, "pane_id").to_string(),
        tab: str_of(p, "tab_id").to_string(),
        how: format!("created from {} via herdr (no wt)", base.unwrap_or("HEAD")),
    })
}

/// `git -C <dir> <args>`, trimmed stdout on success.
fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Drops ANSI CSI sequences (`ESC [ … letter`), e.g. wt's colors.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_prefers_wt_and_refuses_herdr_for_a_wt_repo() {
        assert_eq!(backend(true, false, false).unwrap(), Backend::Wt);
        assert_eq!(backend(true, true, true).unwrap(), Backend::Wt);
        assert_eq!(backend(false, false, false).unwrap(), Backend::Herdr);
        assert_eq!(
            backend(false, true, false).unwrap_err().code,
            "wt_not_found"
        );
        assert_eq!(
            backend(false, false, true).unwrap_err().code,
            "wt_not_found"
        );
    }

    #[test]
    fn wt_failure_keeps_error_lines_and_drops_noise() {
        let stderr = "\u{1b}[1m✗ Branch x already exists\u{1b}[0m\n↳ To switch, run: wt switch x\n";
        let f = wt_failure(&strip_ansi(stderr), "/r");
        assert_eq!(f.code, "wt_failed");
        assert_eq!(
            f.message,
            "wt switch failed: ✗ Branch x already exists / ↳ To switch, run: wt switch x"
        );
    }

    #[test]
    fn wt_failure_keeps_the_git_error() {
        let stderr = "✗ Failed to create worktree for a..b from base main\n  Preparing worktree (new branch 'a..b')\n  fatal: 'a..b' is not a valid branch name\n  hint: See 'git help check-ref-format'\n↳ Failed command, exit code 255:\n  git worktree add -b a..b -- /r.a..b main\n";
        assert_eq!(
            wt_failure(stderr, "/r").message,
            "wt switch failed: ✗ Failed to create worktree for a..b from base main / fatal: 'a..b' is not a valid branch name"
        );
    }

    #[test]
    fn wt_failure_maps_hook_approval() {
        let stderr = "▲ vega needs approval to execute 1 command:\n✗ Cannot prompt for approval in non-interactive environment\n";
        assert_eq!(wt_failure(stderr, "/r").code, "approval_required");
    }

    #[test]
    fn sibling_path_follows_wt_layout() {
        assert_eq!(
            sibling_path("vega", Path::new("/src/vega"), "fix/x"),
            PathBuf::from("/src/vega.fix-x")
        );
    }

    #[test]
    fn worktree_of_finds_the_branch() {
        let l = "worktree /src/vega\nHEAD a\nbranch refs/heads/main\n\nworktree /src/vega.x\nHEAD b\nbranch refs/heads/x\n";
        assert_eq!(worktree_of(l, "x").as_deref(), Some("/src/vega.x"));
        assert_eq!(worktree_of(l, "xx"), None);
    }

    #[test]
    fn git_worktree_creates_honours_base_and_reuses() {
        let d = std::env::temp_dir().join(format!("vega-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let repo = d.join("r");
        std::fs::create_dir_all(&repo).unwrap();
        let g = |args: &[&str]| {
            let out = Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-C"])
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
        };
        g(&["init", "-q", "-b", "main"]);
        g(&["commit", "-q", "--allow-empty", "-m", "a"]);
        g(&["branch", "base"]);
        g(&["commit", "-q", "--allow-empty", "-m", "b"]);
        let top = repo.canonicalize().unwrap().display().to_string();
        let (p, how) = git_worktree(&top, "feat/y", Some("base")).unwrap();
        assert!(p.ends_with("/r.feat-y") && Path::new(&p).is_dir(), "{p}");
        assert!(how.starts_with("created from base"), "{how}");
        assert_eq!(
            git(&p, &["rev-parse", "HEAD"]),
            git(&top, &["rev-parse", "base"])
        );
        let (p2, how2) = git_worktree(&top, "feat/y", None).unwrap();
        assert_eq!(p, p2);
        assert_eq!(how2, "reused via git");
        // existing branch without a worktree: checked out, not recreated
        g(&["branch", "old"]);
        let (p3, how3) = git_worktree(&top, "old", None).unwrap();
        assert!(p3.ends_with("/r.old"), "{p3}");
        assert!(how3.contains("existing branch"), "{how3}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn strip_ansi_removes_color_codes() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m ok"), "red ok");
    }
}
