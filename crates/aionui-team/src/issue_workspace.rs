//! Deterministic repository and Issue worktree admission for persistent Teams.

use std::path::{Path, PathBuf};

use aionui_project::ProjectService;
use tracing::{info, warn};

use crate::error::TeamError;
use crate::fresh_run_workspace;

#[derive(Debug, Clone)]
pub(crate) struct PreparedIssueWorkspace {
    pub repository_full_name: String,
    pub issue_number: u64,
    pub workspace: String,
    pub branch: String,
    pub base_ref: String,
    pub base_sha: String,
    pub reused: bool,
}

pub(crate) async fn prepare(
    projects: &ProjectService,
    user_id: &str,
    repository_full_name: &str,
    issue_number: u64,
    requested_base: Option<&str>,
) -> Result<PreparedIssueWorkspace, TeamError> {
    validate_repository_identity(repository_full_name)?;
    if issue_number == 0 {
        return reject("invalid_issue", "Issue number must be greater than zero");
    }

    let known = projects.list_local_workspace_paths(user_id).await.map_err(|_| {
        rejection(
            "project_state_unavailable",
            "could not read known local project workspaces",
        )
    })?;
    let (root, remote_urls) = resolve_repository(known, repository_full_name).await?;
    let base_resolution = if requested_base.is_some() {
        "explicit"
    } else {
        "remote_default"
    };
    let base_ref = resolve_base_ref(&root, requested_base).await?;
    let base_sha = refresh_and_pin_base(&root, &base_ref).await?;
    let (branch, worktree) = match discover_issue_worktree(&root, issue_number).await? {
        Some((branch, worktree)) => (branch, worktree),
        None => (
            format!("feat/issue-{issue_number}"),
            issue_worktree_path(&root, issue_number),
        ),
    };
    let reused = prepare_worktree(&root, &worktree, &branch, &base_ref, &base_sha).await?;

    verify_repository_identity(&root, &remote_urls, repository_full_name).await?;
    verify_base_pin(&root, &base_ref, &base_sha).await?;
    verify_issue_worktree(&root, &worktree, &branch, &base_ref, &base_sha).await?;

    let workspace = worktree.to_string_lossy().into_owned();
    info!(
        repository = repository_full_name,
        issue = issue_number,
        workspace = %workspace,
        branch,
        base_ref,
        base_resolution,
        outcome = if reused { "reused" } else { "created" },
        "Issue workspace prepared"
    );
    Ok(PreparedIssueWorkspace {
        repository_full_name: repository_full_name.to_owned(),
        issue_number,
        workspace,
        branch,
        base_ref,
        base_sha,
        reused,
    })
}

fn validate_repository_identity(value: &str) -> Result<(&str, &str), TeamError> {
    let Some((owner, repository)) = value.split_once('/') else {
        return reject("invalid_repository", "repository_full_name must be owner/repository");
    };
    let valid_component = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    };
    if value.matches('/').count() != 1 || !valid_component(owner) || !valid_component(repository) {
        return reject("invalid_repository", "repository_full_name must be owner/repository");
    }
    Ok((owner, repository))
}

async fn resolve_repository(known_paths: Vec<PathBuf>, requested: &str) -> Result<(PathBuf, Vec<String>), TeamError> {
    let mut roots = std::collections::BTreeSet::new();
    for known_path in known_paths {
        let root = match main_worktree_root(&known_path).await {
            Ok(Some(root)) => root,
            Ok(None) => continue,
            Err(_) => continue,
        };
        roots.insert(root);
    }

    let mut matches: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for canonical_root in roots {
        let output =
            match fresh_run_workspace::git_output(&canonical_root, ["remote", "get-url", "--all", "origin"]).await {
                Ok(output) if output.status.success() => output,
                _ => continue,
            };
        let urls: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        if urls.iter().any(|url| remote_matches_repository(url, requested)) {
            matches.push((canonical_root, urls));
        }
    }

    match matches.len() {
        0 => reject(
            "repository_not_found",
            "no known local workspace has the requested Git remote identity",
        ),
        1 => Ok(matches.remove(0)),
        _ => reject(
            "repository_ambiguous",
            "multiple known local repositories match the requested identity",
        ),
    }
}

/// Resolve any registered worktree to the main worktree for its shared Git
/// repository. The common directory is Git-owned identity; its parent is the
/// primary worktree only when Git confirms that it contains the repository's
/// `.git` directory and resolves back to the same common directory.
async fn main_worktree_root(known_path: &Path) -> Result<Option<PathBuf>, TeamError> {
    let common = match git_text(known_path, ["rev-parse", "--path-format=absolute", "--git-common-dir"]).await {
        Ok(common) => common,
        Err(_) => return Ok(None),
    };
    let common = match std::fs::canonicalize(common) {
        Ok(common) => common,
        Err(_) => return Ok(None),
    };
    if common.file_name().is_none_or(|name| name != ".git") {
        return Ok(None);
    }
    let Some(candidate) = common.parent() else {
        return Ok(None);
    };
    if !candidate.join(".git").is_dir() {
        return Ok(None);
    }
    let root = match std::fs::canonicalize(candidate) {
        Ok(root) => root,
        Err(_) => return Ok(None),
    };
    let top = match git_text(&root, ["rev-parse", "--show-toplevel"]).await {
        Ok(top) => top,
        Err(_) => return Ok(None),
    };
    let resolved_common = match git_text(&root, ["rev-parse", "--path-format=absolute", "--git-common-dir"]).await {
        Ok(common) => common,
        Err(_) => return Ok(None),
    };
    if Path::new(&top) != root || std::fs::canonicalize(resolved_common).ok().as_deref() != Some(&common) {
        return Ok(None);
    }
    Ok(Some(root))
}

#[derive(Debug)]
struct WorktreeEntry {
    path: PathBuf,
    branch: Option<String>,
}

async fn discover_issue_worktree(root: &Path, issue: u64) -> Result<Option<(String, PathBuf)>, TeamError> {
    let worktree_output = git_text(root, ["worktree", "list", "--porcelain"]).await?;
    let worktrees = parse_worktrees(&worktree_output);
    let local_output = git_text(root, ["for-each-ref", "--format=%(refname:short)", "refs/heads"]).await?;
    let local_branches: std::collections::BTreeSet<_> = local_output
        .lines()
        .map(str::trim)
        .filter(|branch| issue_branch_matches(branch, issue))
        .map(str::to_owned)
        .collect();
    let remote_output = git_text(
        root,
        ["for-each-ref", "--format=%(refname:strip=3)", "refs/remotes/origin"],
    )
    .await?;
    let remote_branches: std::collections::BTreeSet<_> = remote_output
        .lines()
        .map(str::trim)
        .filter(|branch| !branch.is_empty() && *branch != "HEAD" && issue_branch_matches(branch, issue))
        .map(str::to_owned)
        .collect();
    let associated: Vec<_> = worktrees
        .iter()
        .filter_map(|worktree| {
            worktree
                .branch
                .as_deref()
                .filter(|branch| issue_branch_matches(branch, issue))
                .map(|branch| (branch.to_owned(), worktree.path.clone()))
        })
        .collect();

    if associated.len() > 1 || local_branches.len() > 1 {
        return reject(
            "issue_worktree_ambiguous",
            "multiple local Issue branch or worktree associations match this Issue",
        );
    }
    match (associated.first(), local_branches.iter().next()) {
        (Some((branch, path)), Some(local_branch)) if branch == local_branch => {
            if path == root {
                return reject(
                    "issue_worktree_conflict",
                    "the matching Issue branch is checked out in the main worktree, not an isolated worktree",
                );
            }
            if remote_branches.iter().any(|remote| remote != branch) {
                return reject(
                    "issue_worktree_ambiguous",
                    "a conflicting remote Issue branch also matches this Issue",
                );
            }
            Ok(Some((branch.clone(), path.clone())))
        }
        (None, None) if remote_branches.is_empty() => Ok(None),
        (None, None) => reject(
            "issue_worktree_conflict",
            "a remote Issue branch exists without a verified local worktree association",
        ),
        _ => reject(
            "issue_worktree_conflict",
            "Issue branch refs and worktree metadata do not identify one exact safe association",
        ),
    }
}

fn parse_worktrees(output: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut path = None;
    let mut branch = None;
    let flush = |entries: &mut Vec<WorktreeEntry>, path: &mut Option<PathBuf>, branch: &mut Option<String>| {
        if let Some(path) = path.take() {
            entries.push(WorktreeEntry {
                path,
                branch: branch.take(),
            });
        } else {
            branch.take();
        }
    };
    for line in output.lines() {
        if line.is_empty() {
            flush(&mut entries, &mut path, &mut branch);
        } else if let Some(value) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(value));
        } else if let Some(value) = line.strip_prefix("branch refs/heads/") {
            branch = Some(value.to_owned());
        }
    }
    flush(&mut entries, &mut path, &mut branch);
    entries
}

fn issue_branch_matches(branch: &str, issue: u64) -> bool {
    let expected = issue.to_string();
    branch.split('/').any(|component| {
        component
            .strip_prefix("issue-")
            .is_some_and(|suffix| suffix == expected || suffix.starts_with(&format!("{expected}-")))
    })
}

fn remote_matches_repository(remote: &str, requested: &str) -> bool {
    let path = if let Some((_, path)) = remote.split_once("://") {
        path.split_once('/').map(|(_, path)| path).unwrap_or_default()
    } else if remote.contains('@')
        && let Some((_, path)) = remote.split_once(':')
    {
        path
    } else {
        remote
    };
    let path = path.replace('\\', "/");
    let path = path
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or(path.trim_end_matches('/'));
    let suffix = format!("/{requested}");
    path.eq_ignore_ascii_case(requested) || path.to_ascii_lowercase().ends_with(&suffix.to_ascii_lowercase())
}

async fn resolve_base_ref(root: &Path, requested: Option<&str>) -> Result<String, TeamError> {
    if let Some(base) = requested {
        validate_branch(root, base).await?;
        return Ok(base.to_owned());
    }
    let output =
        fresh_run_workspace::git_output(root, ["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"])
            .await
            .map_err(|_| {
                rejection(
                    "base_unresolved",
                    "could not resolve the configured default remote branch",
                )
            })?;
    if !output.status.success() {
        return reject(
            "base_unresolved",
            "supply an explicit base_ref when the default remote branch is unknown",
        );
    }
    let symbolic = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let branch = symbolic
        .strip_prefix("origin/")
        .ok_or_else(|| rejection("base_unresolved", "configured default remote branch is not on origin"))?;
    validate_branch(root, branch).await?;
    Ok(branch.to_owned())
}

async fn validate_branch(root: &Path, branch: &str) -> Result<(), TeamError> {
    if branch.is_empty() || branch.starts_with('-') {
        return reject("base_unresolved", "base_ref is not a valid branch name");
    }
    let output = fresh_run_workspace::git_output(root, ["check-ref-format", "--branch", branch])
        .await
        .map_err(|_| rejection("base_unresolved", "could not validate base_ref"))?;
    if output.status.success() {
        Ok(())
    } else {
        reject("base_unresolved", "base_ref is not a valid branch name")
    }
}

async fn refresh_and_pin_base(root: &Path, branch: &str) -> Result<String, TeamError> {
    let tracking_ref = format!("refs/remotes/origin/{branch}");
    let old_tip = git_text_optional(root, ["rev-parse", "--verify", &tracking_ref]).await;
    let live_tip = remote_tip(root, branch).await?;
    let fetch_ref = format!("refs/heads/{branch}:refs/remotes/origin/{branch}");
    let fetch =
        fresh_run_workspace::git_network_output(root, ["fetch", "--quiet", "--no-tags", "origin", fetch_ref.as_str()])
            .await;
    if !matches!(fetch, Ok(output) if output.status.success()) {
        return reject("base_refresh_failed", "could not refresh the requested origin base ref");
    }
    let fetched_tip = git_text_optional(root, ["rev-parse", "--verify", &tracking_ref]).await;
    if fetched_tip.as_deref() != Some(live_tip.as_str()) || remote_tip(root, branch).await? != live_tip {
        return reject("base_changed", "remote base evidence changed during preparation");
    }
    if let Some(old_tip) = old_tip.as_deref()
        && old_tip != live_tip
        && !is_ancestor(root, old_tip, &live_tip).await?
    {
        return reject(
            "base_rewritten",
            "the local base ref does not safely fast-forward to current remote evidence",
        );
    }
    Ok(live_tip)
}

async fn remote_tip(root: &Path, branch: &str) -> Result<String, TeamError> {
    let reference = format!("refs/heads/{branch}");
    remote_tip_optional(root, &reference)
        .await?
        .ok_or_else(|| rejection("base_unavailable", "origin does not expose the requested base ref"))
}

async fn remote_tip_optional(root: &Path, reference: &str) -> Result<Option<String>, TeamError> {
    let output = fresh_run_workspace::git_network_output(root, ["ls-remote", "--exit-code", "origin", reference]).await;
    let output = match output {
        Ok(output) if output.status.success() => output,
        Ok(output) if output.status.code() == Some(2) && output.stdout.is_empty() => return Ok(None),
        _ => {
            return reject(
                "remote_state_unavailable",
                "could not verify the requested remote ref state",
            );
        }
    };
    let line = String::from_utf8_lossy(&output.stdout);
    let mut parts = line.split_whitespace();
    let Some(sha) = parts.next() else {
        return reject(
            "remote_state_unavailable",
            "origin returned no ref evidence for the requested branch",
        );
    };
    let Some(found_ref) = parts.next() else {
        return reject(
            "remote_state_unavailable",
            "origin returned no ref evidence for the requested branch",
        );
    };
    if found_ref != reference || sha.len() != 40 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return reject(
            "remote_state_unavailable",
            "origin returned invalid ref evidence for the requested branch",
        );
    }
    Ok(Some(sha.to_owned()))
}

async fn prepare_worktree(
    root: &Path,
    worktree: &Path,
    branch: &str,
    base: &str,
    base_sha: &str,
) -> Result<bool, TeamError> {
    if worktree.exists() {
        verify_issue_worktree(root, worktree, branch, base, base_sha).await?;
        return Ok(true);
    }
    let branch_exists = fresh_run_workspace::git_output(
        root,
        ["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")],
    )
    .await
    .map(|output| output.status.success())
    .unwrap_or(false);
    if branch_exists {
        return reject(
            "worktree_conflict",
            "the Issue branch exists outside its expected worktree",
        );
    }
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent).map_err(|_| {
            rejection(
                "worktree_create_failed",
                "could not prepare the isolated worktree parent",
            )
        })?;
    }
    let start_point = format!("refs/remotes/origin/{base}");
    let output = fresh_run_workspace::git_output(
        root,
        [
            "worktree",
            "add",
            "--track",
            "-b",
            branch,
            worktree.to_string_lossy().as_ref(),
            start_point.as_str(),
        ],
    )
    .await
    .map_err(|_| rejection("worktree_create_failed", "could not create the isolated Issue worktree"))?;
    if !output.status.success() {
        return reject(
            "worktree_create_failed",
            "Git refused to create the isolated Issue worktree",
        );
    }
    verify_issue_worktree(root, worktree, branch, base, base_sha).await?;
    Ok(false)
}

async fn verify_issue_worktree(
    root: &Path,
    worktree: &Path,
    branch: &str,
    base: &str,
    base_sha: &str,
) -> Result<(), TeamError> {
    if !worktree.is_dir() {
        return reject(
            "worktree_conflict",
            "the expected Issue worktree path is not a directory",
        );
    }
    let top = git_text(worktree, ["rev-parse", "--show-toplevel"]).await?;
    let common_root = git_text(root, ["rev-parse", "--path-format=absolute", "--git-common-dir"]).await?;
    let common_worktree = git_text(worktree, ["rev-parse", "--path-format=absolute", "--git-common-dir"]).await?;
    let actual_branch = git_text(worktree, ["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
    let head = git_text(worktree, ["rev-parse", "--verify", "HEAD"]).await?;
    let upstream = git_text(
        worktree,
        ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
    )
    .await?;
    let expected_upstream = format!("origin/{base}");
    if Path::new(&top) != worktree
        || common_root != common_worktree
        || actual_branch != branch
        || head != base_sha
        || upstream != expected_upstream
    {
        return reject(
            "worktree_identity_mismatch",
            "existing worktree repository, Issue branch, base, or HEAD does not match",
        );
    }
    if fresh_run_workspace::has_operation_in_progress(worktree).await? {
        return reject("worktree_unsafe", "the Issue worktree has an in-progress Git operation");
    }
    let status = git_text(worktree, ["status", "--porcelain=v1", "--untracked-files=all"]).await?;
    if !status.is_empty() {
        return reject(
            "worktree_dirty",
            "the Issue worktree has uncommitted or untracked files",
        );
    }
    let issue_remote = format!("refs/heads/{branch}");
    if let Some(remote_head) = remote_tip_optional(root, &issue_remote).await?
        && remote_head != base_sha
    {
        return reject(
            "worktree_history_unexpected",
            "the remote Issue branch contains unexpected history",
        );
    }
    Ok(())
}

async fn verify_repository_identity(root: &Path, original_urls: &[String], requested: &str) -> Result<(), TeamError> {
    let output = fresh_run_workspace::git_output(root, ["remote", "get-url", "--all", "origin"])
        .await
        .map_err(|_| rejection("repository_changed", "could not recheck repository remote identity"))?;
    let urls: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    if !output.status.success()
        || urls != original_urls
        || !urls.iter().any(|url| remote_matches_repository(url, requested))
    {
        return reject(
            "repository_changed",
            "repository remote identity changed during preparation",
        );
    }
    Ok(())
}

async fn verify_base_pin(root: &Path, branch: &str, expected: &str) -> Result<(), TeamError> {
    let local_ref = format!("refs/remotes/origin/{branch}");
    if git_text_optional(root, ["rev-parse", "--verify", &local_ref])
        .await
        .as_deref()
        != Some(expected)
        || remote_tip(root, branch).await? != expected
    {
        return reject(
            "base_changed",
            "the requested base ref changed during Issue workspace preparation",
        );
    }
    Ok(())
}

async fn is_ancestor(root: &Path, older: &str, newer: &str) -> Result<bool, TeamError> {
    let output = fresh_run_workspace::git_output(root, ["merge-base", "--is-ancestor", older, newer])
        .await
        .map_err(|_| rejection("base_history_unresolved", "could not classify base ref history"))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => reject("base_history_unresolved", "could not classify base ref history"),
    }
}

async fn git_text<I, S>(root: &Path, args: I) -> Result<String, TeamError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = fresh_run_workspace::git_output(root, args)
        .await
        .map_err(|_| rejection("git_inspection_failed", "could not inspect Git workspace identity"))?;
    if !output.status.success() {
        return reject("git_inspection_failed", "could not inspect Git workspace identity");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

async fn git_text_optional<I, S>(root: &Path, args: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = fresh_run_workspace::git_output(root, args).await.ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn issue_worktree_path(root: &Path, issue: u64) -> PathBuf {
    root.parent().unwrap_or(root).join(format!(
        "{}-ISSUE-{issue}",
        root.file_name().unwrap_or_default().to_string_lossy()
    ))
}

fn reject<T>(reason: &'static str, message: &'static str) -> Result<T, TeamError> {
    Err(rejection(reason, message))
}

fn rejection(reason: &'static str, message: &'static str) -> TeamError {
    warn!(reason, "Issue workspace preparation rejected");
    TeamError::InvalidRequest(format!("Issue workspace preparation failed ({reason}): {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::Arc;

    use aionui_db::{Database, IProjectStore, SqliteProjectStore, init_database_memory};
    use aionui_project::canonical::to_file_uri;

    struct RepoFixture {
        _temp: tempfile::TempDir,
        _db: Database,
        projects: ProjectService,
        root: PathBuf,
        remote: PathBuf,
    }

    impl RepoFixture {
        async fn new(name: &str) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("workspace-with-unrelated-name");
            let remote = temp.path().join(format!("R3PORTME/{name}.git"));
            std::fs::create_dir_all(remote.parent().unwrap()).unwrap();
            git(temp.path(), ["init", "--quiet", "--bare", remote.to_str().unwrap()]);
            std::fs::create_dir_all(&root).unwrap();
            git(&root, ["init", "--quiet", "--initial-branch=feat/team-fresh-run"]);
            git(&root, ["config", "user.name", "Issue Workspace Test"]);
            git(&root, ["config", "user.email", "issue-workspace@example.invalid"]);
            git(&root, ["remote", "add", "origin", remote.to_str().unwrap()]);
            git(
                &root,
                ["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"],
            );
            std::fs::write(root.join("tracked.txt"), "initial\n").unwrap();
            git(&root, ["add", "tracked.txt"]);
            git(&root, ["commit", "--quiet", "-m", "initial"]);
            git(
                &root,
                ["push", "--quiet", "--set-upstream", "origin", "feat/team-fresh-run"],
            );
            git(&root, ["remote", "set-head", "origin", "feat/team-fresh-run"]);

            let db = init_database_memory().await.unwrap();
            let store: Arc<dyn IProjectStore> = Arc::new(SqliteProjectStore::new(db.pool().clone()));
            let projects = ProjectService::new(Arc::clone(&store), temp.path().join("temp-projects"));
            projects
                .create_standard("system_default_user", to_file_uri(&root).unwrap())
                .await
                .unwrap();
            Self {
                _temp: temp,
                _db: db,
                projects,
                root,
                remote,
            }
        }

        async fn prepare(
            &self,
            name: &str,
            issue: u64,
            base: Option<&str>,
        ) -> Result<PreparedIssueWorkspace, TeamError> {
            prepare(
                &self.projects,
                "system_default_user",
                &format!("R3PORTME/{name}"),
                issue,
                base,
            )
            .await
        }

        fn publish_commit(&self, file: &str, content: &str) {
            let publisher = self._temp.path().join("publisher");
            git(
                self._temp.path(),
                [
                    "clone",
                    "--quiet",
                    "--branch",
                    "feat/team-fresh-run",
                    self.remote.to_str().unwrap(),
                    publisher.to_str().unwrap(),
                ],
            );
            git(&publisher, ["config", "user.name", "Issue Workspace Test"]);
            git(&publisher, ["config", "user.email", "issue-workspace@example.invalid"]);
            std::fs::write(publisher.join(file), content).unwrap();
            git(&publisher, ["add", file]);
            git(&publisher, ["commit", "--quiet", "-m", "advance base"]);
            git(&publisher, ["push", "--quiet", "origin", "feat/team-fresh-run"]);
        }
    }

    fn git<I, S>(cwd: &Path, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = Command::new("git").args(args).current_dir(cwd).output().unwrap();
        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn remote_identity_uses_remote_metadata_and_normalizes_common_git_urls() {
        for remote in [
            "https://github.com/R3PORTME/AionCore.git",
            "ssh://git@github.com/R3PORTME/AionCore.git",
            "git@github.com:R3PORTME/AionCore.git",
            "C:\\Projects\\R3PORTME\\AionCore.git",
        ] {
            assert!(remote_matches_repository(remote, "R3PORTME/AionCore"));
        }
        assert!(remote_matches_repository(
            "https://github.com/r3portme/aioncore.git",
            "R3PORTME/AionCore"
        ));
        assert!(!remote_matches_repository(
            "https://github.com/other/AionCore.git",
            "R3PORTME/AionCore"
        ));
    }

    #[test]
    fn repository_identity_rejects_ambiguous_or_path_like_values() {
        assert!(validate_repository_identity("R3PORTME/AionCore").is_ok());
        assert!(validate_repository_identity("R3PORTME/AionCore/other").is_err());
        assert!(validate_repository_identity("../AionCore").is_err());
        assert!(validate_repository_identity("R3PORTME/").is_err());
    }

    #[tokio::test]
    async fn resolves_one_repository_from_persisted_workspace_and_ignores_its_directory_name() {
        let fixture = RepoFixture::new("AionCore").await;
        let prepared = fixture
            .prepare("AionCore", 21, Some("feat/team-fresh-run"))
            .await
            .unwrap();
        assert_eq!(prepared.repository_full_name, "R3PORTME/AionCore");
        assert_eq!(Path::new(&prepared.workspace), issue_worktree_path(&fixture.root, 21));
        assert_eq!(prepared.branch, "feat/issue-21");
        assert_eq!(prepared.base_ref, "feat/team-fresh-run");
        assert!(!prepared.reused);
    }

    #[tokio::test]
    async fn rejects_directory_name_match_when_remote_identity_differs() {
        let fixture = RepoFixture::new("DifferentRepo").await;
        let error = fixture
            .prepare("AionCore", 21, Some("feat/team-fresh-run"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("repository_not_found"));
    }

    #[tokio::test]
    async fn rejects_no_matching_local_repository() {
        let fixture = RepoFixture::new("DifferentRepo").await;
        let error = fixture
            .prepare("Missing", 21, Some("feat/team-fresh-run"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("repository_not_found"));
    }

    #[tokio::test]
    async fn rejects_ambiguous_matching_local_repositories() {
        let fixture = RepoFixture::new("AionCore").await;
        let duplicate = fixture._temp.path().join("second-checkout");
        git(
            fixture._temp.path(),
            [
                "clone",
                "--quiet",
                fixture.remote.to_str().unwrap(),
                duplicate.to_str().unwrap(),
            ],
        );
        let uri = to_file_uri(&duplicate).unwrap();
        fixture
            .projects
            .create_standard("system_default_user", uri)
            .await
            .unwrap();
        let error = fixture
            .prepare("AionCore", 21, Some("feat/team-fresh-run"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("repository_ambiguous"));
    }

    #[tokio::test]
    async fn linked_project_worktrees_resolve_to_one_main_repository_root() {
        let fixture = RepoFixture::new("AionCore").await;
        let linked = fixture._temp.path().join("registered-linked-worktree");
        git(
            &fixture.root,
            [
                "worktree",
                "add",
                "--quiet",
                "--track",
                "-b",
                "feat/linked-workspace",
                linked.to_str().unwrap(),
                "refs/remotes/origin/feat/team-fresh-run",
            ],
        );
        fixture
            .projects
            .create_standard("system_default_user", to_file_uri(&linked).unwrap())
            .await
            .unwrap();

        assert_eq!(main_worktree_root(&linked).await.unwrap().unwrap(), fixture.root);
        let (resolved, _) = resolve_repository(vec![fixture.root.clone(), linked], "R3PORTME/AionCore")
            .await
            .unwrap();
        assert_eq!(resolved, fixture.root);
    }

    #[tokio::test]
    async fn reuses_a_convention_compliant_issue_worktree_from_git_metadata() {
        let fixture = RepoFixture::new("AionCore").await;
        let existing = fixture._temp.path().join("custom-location/issue 21 checkout");
        git(
            &fixture.root,
            [
                "worktree",
                "add",
                "--quiet",
                "--track",
                "-b",
                "fix/issue-21-existing-convention-slug",
                existing.to_str().unwrap(),
                "refs/remotes/origin/feat/team-fresh-run",
            ],
        );
        fixture
            .projects
            .create_standard("system_default_user", to_file_uri(&existing).unwrap())
            .await
            .unwrap();

        let prepared = fixture
            .prepare("AionCore", 21, Some("feat/team-fresh-run"))
            .await
            .unwrap();
        assert!(prepared.reused);
        assert_eq!(prepared.branch, "fix/issue-21-existing-convention-slug");
        assert_eq!(Path::new(&prepared.workspace), existing);
    }

    #[tokio::test]
    async fn rejects_multiple_issue_worktree_associations() {
        let fixture = RepoFixture::new("AionCore").await;
        for (branch, directory) in [
            ("feat/issue-21-first", "issue-21-first"),
            ("fix/issue-21-second", "issue-21-second"),
        ] {
            let worktree = fixture._temp.path().join(directory);
            git(
                &fixture.root,
                [
                    "worktree",
                    "add",
                    "--quiet",
                    "--track",
                    "-b",
                    branch,
                    worktree.to_str().unwrap(),
                    "refs/remotes/origin/feat/team-fresh-run",
                ],
            );
        }
        let error = fixture
            .prepare("AionCore", 21, Some("feat/team-fresh-run"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("issue_worktree_ambiguous"));
    }

    #[tokio::test]
    async fn refreshes_a_stale_base_before_pinning_the_worktree() {
        let fixture = RepoFixture::new("AionCore").await;
        fixture.publish_commit("advanced.txt", "advanced\n");
        let prepared = fixture
            .prepare("AionCore", 21, Some("feat/team-fresh-run"))
            .await
            .unwrap();
        assert_eq!(
            git(Path::new(&prepared.workspace), ["rev-parse", "HEAD"]),
            prepared.base_sha
        );
        assert_eq!(
            git(&fixture.root, ["rev-parse", "refs/remotes/origin/feat/team-fresh-run"]),
            prepared.base_sha
        );
    }

    #[tokio::test]
    async fn rejects_a_rewritten_base_and_an_unexpected_remote_issue_branch() {
        let rewritten = RepoFixture::new("AionCore").await;
        let old_base = git(&rewritten.root, ["rev-parse", "HEAD"]);
        rewritten.publish_commit("next.txt", "next\n");
        git(&rewritten.root, ["fetch", "--quiet", "origin"]);
        git(
            &rewritten.remote,
            [
                "--git-dir",
                rewritten.remote.to_str().unwrap(),
                "update-ref",
                "refs/heads/feat/team-fresh-run",
                &old_base,
            ],
        );
        assert!(
            refresh_and_pin_base(&rewritten.root, "feat/team-fresh-run")
                .await
                .is_err()
        );

        let issue_branch = RepoFixture::new("AionCore").await;
        issue_branch
            .prepare("AionCore", 21, Some("feat/team-fresh-run"))
            .await
            .unwrap();
        git(
            &issue_branch.root,
            ["switch", "--quiet", "-c", "temporary-issue-candidate"],
        );
        std::fs::write(issue_branch.root.join("candidate.txt"), "remote candidate\n").unwrap();
        git(&issue_branch.root, ["add", "candidate.txt"]);
        git(&issue_branch.root, ["commit", "--quiet", "-m", "remote issue branch"]);
        git(
            &issue_branch.root,
            ["push", "--quiet", "origin", "HEAD:refs/heads/feat/issue-21"],
        );
        git(&issue_branch.root, ["switch", "--quiet", "feat/team-fresh-run"]);
        git(&issue_branch.root, ["branch", "-D", "temporary-issue-candidate"]);
        assert!(
            issue_branch
                .prepare("AionCore", 21, Some("feat/team-fresh-run"))
                .await
                .unwrap_err()
                .to_string()
                .contains("worktree_history_unexpected")
        );
    }

    #[tokio::test]
    async fn rejects_missing_or_unresolvable_base_ref() {
        let fixture = RepoFixture::new("AionCore").await;
        let missing = fixture.prepare("AionCore", 21, Some("feat/missing")).await.unwrap_err();
        assert!(missing.to_string().contains("base_unavailable"));
        let default = fixture.prepare("AionCore", 21, None).await.unwrap();
        assert_eq!(default.base_ref, "feat/team-fresh-run");
    }

    #[tokio::test]
    async fn creates_then_reuses_only_the_exact_clean_issue_worktree() {
        let fixture = RepoFixture::new("AionCore").await;
        let first = fixture
            .prepare("AionCore", 21, Some("feat/team-fresh-run"))
            .await
            .unwrap();
        let second = fixture
            .prepare("AionCore", 21, Some("feat/team-fresh-run"))
            .await
            .unwrap();
        assert!(!first.reused);
        assert!(second.reused);
        assert_eq!(first.workspace, second.workspace);
        assert_eq!(first.base_sha, second.base_sha);
        let freshness = fresh_run_workspace::inspect(&first.workspace).await.unwrap();
        fresh_run_workspace::require_pinned_head(&freshness, &first.base_sha).unwrap();
    }

    #[tokio::test]
    async fn rejects_conflicting_dirty_and_unexpected_history_worktrees() {
        let conflict = RepoFixture::new("AionCore").await;
        let target = issue_worktree_path(&conflict.root, 21);
        std::fs::create_dir_all(&target).unwrap();
        assert!(
            conflict
                .prepare("AionCore", 21, Some("feat/team-fresh-run"))
                .await
                .unwrap_err()
                .to_string()
                .contains("git_inspection_failed")
        );

        let dirty = RepoFixture::new("AionCore").await;
        let first = dirty
            .prepare("AionCore", 21, Some("feat/team-fresh-run"))
            .await
            .unwrap();
        std::fs::write(Path::new(&first.workspace).join("untracked.txt"), "keep\n").unwrap();
        assert!(
            dirty
                .prepare("AionCore", 21, Some("feat/team-fresh-run"))
                .await
                .unwrap_err()
                .to_string()
                .contains("worktree_dirty")
        );

        let operation = RepoFixture::new("AionCore").await;
        let first = operation
            .prepare("AionCore", 21, Some("feat/team-fresh-run"))
            .await
            .unwrap();
        let lock_path = git(Path::new(&first.workspace), ["rev-parse", "--git-path", "index.lock"]);
        let lock_path = Path::new(&first.workspace).join(lock_path);
        std::fs::write(lock_path, "in progress").unwrap();
        assert!(
            operation
                .prepare("AionCore", 21, Some("feat/team-fresh-run"))
                .await
                .unwrap_err()
                .to_string()
                .contains("worktree_unsafe")
        );

        let ahead = RepoFixture::new("AionCore").await;
        let first = ahead
            .prepare("AionCore", 21, Some("feat/team-fresh-run"))
            .await
            .unwrap();
        let branch = Path::new(&first.workspace);
        git(branch, ["config", "user.name", "Issue Workspace Test"]);
        git(branch, ["config", "user.email", "issue-workspace@example.invalid"]);
        std::fs::write(branch.join("candidate.txt"), "candidate\n").unwrap();
        git(branch, ["add", "candidate.txt"]);
        git(branch, ["commit", "--quiet", "-m", "unexpected candidate"]);
        assert!(
            ahead
                .prepare("AionCore", 21, Some("feat/team-fresh-run"))
                .await
                .unwrap_err()
                .to_string()
                .contains("worktree_identity_mismatch")
        );
    }

    #[tokio::test]
    async fn rejects_repository_or_base_changes_and_preserves_unrelated_files() {
        let fixture = RepoFixture::new("AionCore").await;
        let unrelated = fixture._temp.path().join("unrelated-worktree-data.txt");
        std::fs::write(&unrelated, "preserve\n").unwrap();
        fixture
            .prepare("AionCore", 21, Some("feat/team-fresh-run"))
            .await
            .unwrap();
        git(
            &fixture.root,
            [
                "remote",
                "set-url",
                "origin",
                "https://example.invalid/changed/repository.git",
            ],
        );
        assert!(
            verify_repository_identity(
                &fixture.root,
                &[fixture.remote.to_string_lossy().into_owned()],
                "R3PORTME/AionCore"
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("repository_changed")
        );
        assert_eq!(std::fs::read_to_string(unrelated).unwrap(), "preserve\n");

        let base_changed = RepoFixture::new("AionCore").await;
        let first = base_changed
            .prepare("AionCore", 21, Some("feat/team-fresh-run"))
            .await
            .unwrap();
        base_changed.publish_commit("next.txt", "next\n");
        refresh_and_pin_base(&base_changed.root, "feat/team-fresh-run")
            .await
            .unwrap();
        assert!(
            verify_base_pin(&base_changed.root, "feat/team-fresh-run", &first.base_sha)
                .await
                .unwrap_err()
                .to_string()
                .contains("base_changed")
        );
    }

    #[tokio::test]
    async fn supports_sequential_issues_without_reusing_another_issue_branch() {
        let fixture = RepoFixture::new("AionCore").await;
        let first = fixture
            .prepare("AionCore", 21, Some("feat/team-fresh-run"))
            .await
            .unwrap();
        let second = fixture
            .prepare("AionCore", 22, Some("feat/team-fresh-run"))
            .await
            .unwrap();
        assert_ne!(first.workspace, second.workspace);
        assert_ne!(first.branch, second.branch);
    }
}
