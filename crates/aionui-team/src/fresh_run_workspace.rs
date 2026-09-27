//! Git workspace freshness admission for Team fresh-run.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use aionui_runtime::{Builder, kill_process_tree};
use tracing::{info, warn};

use crate::error::TeamError;

const NETWORK_GIT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub(crate) enum FreshnessPlan {
    NonGit,
    Git {
        root: PathBuf,
        workspace: String,
        branch: String,
        upstream: String,
        remote: Option<String>,
        merge_ref: Option<String>,
        head: String,
        target: String,
        needs_fast_forward: bool,
    },
}

/// Issue admission pins a worktree to the exact base SHA it prepared. Reusing
/// the ordinary freshness path is safe only while both the checkout and its
/// refreshed upstream still equal that pin.
pub(crate) fn require_pinned_head(plan: &FreshnessPlan, expected: &str) -> Result<(), TeamError> {
    match plan {
        FreshnessPlan::Git {
            head,
            target,
            needs_fast_forward: false,
            ..
        } if head == expected && target == expected => Ok(()),
        _ => Err(TeamError::InvalidRequest(
            "Issue workspace admission failed (base_changed): prepared workspace no longer matches its pinned base"
                .to_owned(),
        )),
    }
}

/// Refresh evidence and classify the checkout without changing its worktree.
/// The caller must finish ownership/work admission before applying a fast-forward.
pub(crate) async fn inspect(workspace: &str) -> Result<FreshnessPlan, TeamError> {
    let root = match git_root(workspace).await? {
        None => return Ok(FreshnessPlan::NonGit),
        Some(root) => root,
    };
    let path = root.to_string_lossy().into_owned();

    let branch_output = git_output(&root, ["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .map_err(|_| rejection(&path, "git_inspection_failed", "could not inspect Git branch identity"))?;
    if !branch_output.status.success() {
        return reject(&path, "detached_head", "workspace is on a detached HEAD");
    }
    let branch = String::from_utf8_lossy(&branch_output.stdout).trim().to_owned();
    if branch.is_empty() {
        return reject(&path, "detached_head", "workspace is on a detached HEAD");
    }
    let upstream_output = git_output(
        &root,
        ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
    )
    .await
    .map_err(|_| {
        rejection(
            &path,
            "git_inspection_failed",
            "could not inspect Git upstream identity",
        )
    })?;
    if !upstream_output.status.success() {
        return reject(&path, "missing_upstream", "workspace branch has no resolvable upstream");
    }
    let upstream = String::from_utf8_lossy(&upstream_output.stdout).trim().to_owned();
    if upstream.is_empty() {
        return reject(&path, "missing_upstream", "workspace branch has no resolvable upstream");
    }
    let remote = git_text(&root, ["config", "--get", &format!("branch.{branch}.remote")])
        .await
        .ok();
    let merge_ref = git_text(&root, ["config", "--get", &format!("branch.{branch}.merge")])
        .await
        .ok();

    if has_operation_in_progress(&root).await? {
        return reject(
            &path,
            "operation_in_progress",
            "workspace has an in-progress Git operation",
        );
    }

    if let Some(remote) = remote.as_deref().filter(|name| !name.is_empty() && *name != ".") {
        let Some(merge_ref) = merge_ref.as_deref().filter(|reference| !reference.is_empty()) else {
            return reject(&path, "missing_upstream", "workspace branch has no resolvable upstream");
        };
        let refreshed = git_network_output(&root, ["fetch", "--quiet", "--no-tags", remote]).await;
        if !matches!(refreshed, Ok(output) if output.status.success()) {
            return reject(
                &path,
                "upstream_refresh_failed",
                "could not refresh the configured Git upstream",
            );
        }
        let live_ref = git_network_output(&root, ["ls-remote", "--exit-code", remote, merge_ref]).await;
        let live_tip = match live_ref {
            Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .next()
                .map(str::to_owned),
            _ => None,
        };
        let tracking_tip = git_text(&root, ["rev-parse", "--verify", &upstream]).await.ok();
        if live_tip.is_none() || live_tip != tracking_tip {
            return reject(
                &path,
                "upstream_refresh_failed",
                "could not re-establish the refreshed upstream tip",
            );
        }
    }

    // Re-establish the checkout identity after fetch, since fetch and other Git
    // clients can run concurrently with this admission check.
    let branch_after_fetch = git_text(&root, ["symbolic-ref", "--quiet", "--short", "HEAD"]).await;
    let upstream_after_fetch = git_text(
        &root,
        ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
    )
    .await;
    if !matches!(branch_after_fetch, Ok(ref value) if value == &branch)
        || !matches!(upstream_after_fetch, Ok(ref value) if value == &upstream)
    {
        return reject(
            &path,
            "identity_changed",
            "branch or upstream identity changed during freshness admission",
        );
    }

    let head = git_text(&root, ["rev-parse", "--verify", "HEAD"]).await?;
    let target = git_text(&root, ["rev-parse", "--verify", &upstream]).await?;
    let needs_fast_forward = if head == target {
        false
    } else if is_ancestor(&root, &head, &target).await? {
        ensure_fast_forward_safe(&root, &path, &head, &target).await?;
        true
    } else if is_ancestor(&root, &target, &head).await? {
        return reject(
            &path,
            "ahead",
            "workspace branch contains commits ahead of its upstream",
        );
    } else {
        return reject(&path, "diverged", "workspace branch has diverged from its upstream");
    };

    Ok(FreshnessPlan::Git {
        root,
        workspace: path,
        branch,
        upstream,
        remote: remote.filter(|name| !name.is_empty() && name != "."),
        merge_ref,
        head,
        target,
        needs_fast_forward,
    })
}

/// Apply a previously inspected plan after ownership and enqueue admission.
pub(crate) async fn apply(plan: FreshnessPlan) -> Result<(), TeamError> {
    let FreshnessPlan::Git {
        root,
        workspace,
        branch,
        upstream,
        remote,
        merge_ref,
        head,
        target,
        needs_fast_forward,
    } = plan
    else {
        return Ok(());
    };

    verify_identity(&root, &workspace, &branch, &upstream, &head, &target).await?;
    verify_remote_tip(
        &root,
        &workspace,
        remote.as_deref(),
        merge_ref.as_deref(),
        &upstream,
        &target,
    )
    .await?;
    if !needs_fast_forward {
        info!(
            workspace,
            branch,
            upstream,
            outcome = "current",
            "team fresh-run workspace admitted"
        );
        return Ok(());
    }

    ensure_fast_forward_safe(&root, &workspace, &head, &target).await?;
    // Git's ff-only merge refuses non-fast-forward and path collisions. There is
    // no reset/stash/clean or other destructive fallback.
    let result = git_output(&root, ["merge", "--ff-only", &target]).await;
    if !matches!(result, Ok(output) if output.status.success()) {
        return reject(
            &workspace,
            "fast_forward_failed",
            "safe fast-forward could not be completed",
        );
    }
    ensure_fast_forward_safe(&root, &workspace, &target, &target).await?;
    verify_identity(&root, &workspace, &branch, &upstream, &target, &target).await?;
    info!(
        workspace,
        branch,
        upstream,
        outcome = "fast_forwarded",
        "team fresh-run workspace admitted"
    );
    Ok(())
}

async fn git_root(workspace: &str) -> Result<Option<PathBuf>, TeamError> {
    match git_output(Path::new(workspace), ["rev-parse", "--show-toplevel"]).await {
        Ok(output) if output.status.success() => {
            let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            Ok(Some(PathBuf::from(root)))
        }
        Ok(output) if String::from_utf8_lossy(&output.stderr).contains("not a git repository") => {
            if has_git_metadata(Path::new(workspace)) {
                Err(rejection(
                    workspace,
                    "git_metadata_unavailable",
                    "could not inspect Git workspace metadata",
                ))
            } else {
                Ok(None)
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if has_git_metadata(Path::new(workspace)) {
                Err(rejection(
                    workspace,
                    "git_unavailable",
                    "Git workspace freshness cannot be verified",
                ))
            } else {
                Ok(None)
            }
        }
        _ => Err(rejection(
            workspace,
            "git_repository_identity_failed",
            "could not identify the Git workspace; freshness cannot be verified",
        )),
    }
}

fn has_git_metadata(workspace: &Path) -> bool {
    workspace.ancestors().any(|path| path.join(".git").exists())
}

pub(super) async fn git_output<I, S>(directory: &Path, args: I) -> std::io::Result<std::process::Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let command = git_builder(directory, args, false);
    command.output().await
}

fn git_builder<I, S>(directory: &Path, args: I, network: bool) -> Builder
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Builder::clean_cli("git");
    command.args(args).current_dir(directory).env("LC_ALL", "C");
    if network {
        apply_noninteractive_git_env(&mut command);
    }
    command
}

fn apply_noninteractive_git_env(command: &mut Builder) {
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum NetworkCommandError {
    Spawn,
    TimedOut,
    Wait,
    Read,
}

pub(super) async fn git_network_output<I, S>(directory: &Path, args: I) -> Result<Output, NetworkCommandError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    run_bounded_network_command(git_builder(directory, args, true), NETWORK_GIT_TIMEOUT).await
}

async fn run_bounded_network_command(command: Builder, timeout: Duration) -> Result<Output, NetworkCommandError> {
    use tokio::io::AsyncReadExt;

    let mut child = command.spawn().map_err(|_| NetworkCommandError::Spawn)?;
    let mut stdout = child.stdout.take().ok_or(NetworkCommandError::Read)?;
    let mut stderr = child.stderr.take().ok_or(NetworkCommandError::Read)?;
    let stdout_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await.map(|_| bytes)
    });

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => {
            let _ = kill_process_tree(&mut child).await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(NetworkCommandError::Wait);
        }
        Err(_) => {
            let _ = kill_process_tree(&mut child).await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(NetworkCommandError::TimedOut);
        }
    };
    let stdout = stdout_task
        .await
        .map_err(|_| NetworkCommandError::Read)?
        .map_err(|_| NetworkCommandError::Read)?;
    let stderr = stderr_task
        .await
        .map_err(|_| NetworkCommandError::Read)?
        .map_err(|_| NetworkCommandError::Read)?;
    Ok(Output { status, stdout, stderr })
}

async fn git_text<I, S>(directory: &Path, args: I) -> Result<String, TeamError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = git_output(directory, args).await.map_err(|_| {
        rejection(
            &directory.to_string_lossy(),
            "git_inspection_failed",
            "could not inspect Git workspace state",
        )
    })?;
    if !output.status.success() {
        return Err(rejection(
            &directory.to_string_lossy(),
            "git_inspection_failed",
            "could not inspect Git workspace state",
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

async fn git_bytes<I, S>(directory: &Path, args: I) -> Result<Vec<u8>, TeamError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = git_output(directory, args).await.map_err(|_| {
        rejection(
            &directory.to_string_lossy(),
            "git_inspection_failed",
            "could not inspect Git workspace state",
        )
    })?;
    if !output.status.success() {
        return Err(rejection(
            &directory.to_string_lossy(),
            "git_inspection_failed",
            "could not inspect Git workspace state",
        ));
    }
    Ok(output.stdout)
}

async fn verify_identity(
    root: &Path,
    workspace: &str,
    expected_branch: &str,
    expected_upstream: &str,
    expected_head: &str,
    expected_target: &str,
) -> Result<(), TeamError> {
    let branch = git_text(root, ["symbolic-ref", "--quiet", "--short", "HEAD"]).await;
    let upstream = git_text(
        root,
        ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
    )
    .await;
    let head = git_text(root, ["rev-parse", "--verify", "HEAD"]).await;
    let target = git_text(root, ["rev-parse", "--verify", expected_upstream]).await;
    if !matches!(branch, Ok(ref value) if value == expected_branch)
        || !matches!(upstream, Ok(ref value) if value == expected_upstream)
        || !matches!(head, Ok(ref value) if value == expected_head)
        || !matches!(target, Ok(ref value) if value == expected_target)
    {
        return reject(
            workspace,
            "identity_changed",
            "branch or upstream identity changed during freshness admission",
        );
    }
    Ok(())
}

async fn verify_remote_tip(
    root: &Path,
    workspace: &str,
    remote: Option<&str>,
    merge_ref: Option<&str>,
    upstream: &str,
    expected: &str,
) -> Result<(), TeamError> {
    let (Some(remote), Some(merge_ref)) = (remote, merge_ref) else {
        return Ok(());
    };
    let live = git_network_output(root, ["ls-remote", "--exit-code", remote, merge_ref]).await;
    let live_tip = match live {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .map(str::to_owned),
        _ => None,
    };
    let tracking_tip = git_text(root, ["rev-parse", "--verify", upstream]).await.ok();
    if live_tip.as_deref() != Some(expected) || tracking_tip.as_deref() != Some(expected) {
        return reject(
            workspace,
            "upstream_refresh_failed",
            "refreshed upstream tip changed during fresh-run admission",
        );
    }
    Ok(())
}

async fn ensure_fast_forward_safe(root: &Path, workspace: &str, head: &str, target: &str) -> Result<(), TeamError> {
    if has_operation_in_progress(root).await? {
        return reject(
            workspace,
            "operation_in_progress",
            "workspace has an in-progress Git operation",
        );
    }
    let tracked_status = git_text(
        root,
        [
            "status",
            "--porcelain=v1",
            "--untracked-files=no",
            "--ignore-submodules=none",
        ],
    )
    .await?;
    if !tracked_status.is_empty() {
        return reject(workspace, "dirty_worktree", "workspace has tracked or staged changes");
    }
    let mut unknown = git_bytes(root, ["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    let ignored = git_bytes(root, ["ls-files", "--others", "--ignored", "--exclude-standard", "-z"]).await?;
    unknown.extend_from_slice(&ignored);
    let incoming = git_bytes(root, ["ls-tree", "-r", "-z", "--name-only", target]).await?;
    if has_checkout_path_collision(&unknown, &incoming) {
        return reject(
            workspace,
            "untracked_path_conflict",
            "fast-forward would overwrite untracked or ignored user content",
        );
    }
    if git_text(root, ["rev-parse", "--verify", "HEAD"]).await? != head {
        return reject(
            workspace,
            "identity_changed",
            "workspace HEAD changed during freshness admission",
        );
    }
    Ok(())
}

fn has_checkout_path_collision(unknown: &[u8], incoming: &[u8]) -> bool {
    let unknown: HashSet<&[u8]> = unknown
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect();
    let incoming: HashSet<&[u8]> = incoming
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect();
    unknown.iter().any(|path| path_or_parent_exists(path, &incoming))
        || incoming.iter().any(|path| path_or_parent_exists(path, &unknown))
}

fn path_or_parent_exists(path: &[u8], candidates: &HashSet<&[u8]>) -> bool {
    candidates.contains(path)
        || path
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'/')
            .any(|(index, _)| candidates.contains(&path[..index]))
}

async fn is_ancestor(root: &Path, older: &str, newer: &str) -> Result<bool, TeamError> {
    match git_output(root, ["merge-base", "--is-ancestor", older, newer]).await {
        Ok(output) if output.status.success() => Ok(true),
        Ok(output) if output.status.code() == Some(1) => Ok(false),
        _ => Err(rejection(
            &root.to_string_lossy(),
            "history_classification_failed",
            "could not classify Git upstream history",
        )),
    }
}

pub(super) async fn has_operation_in_progress(root: &Path) -> Result<bool, TeamError> {
    let paths = [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "REBASE_HEAD",
        "rebase-apply",
        "rebase-merge",
        "sequencer",
        "BISECT_LOG",
        "index.lock",
    ];
    for path in paths {
        let resolved = git_text(root, ["rev-parse", "--git-path", path]).await?;
        let resolved = Path::new(&resolved);
        let resolved = if resolved.is_absolute() {
            resolved.to_path_buf()
        } else {
            root.join(resolved)
        };
        if resolved.exists() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn reject<T>(workspace: &str, reason: &'static str, message: &'static str) -> Result<T, TeamError> {
    Err(rejection(workspace, reason, message))
}

fn rejection(workspace: &str, reason: &'static str, message: &'static str) -> TeamError {
    warn!(workspace, reason, "team fresh-run Git workspace rejected");
    TeamError::InvalidRequest(format!("Git workspace admission failed ({reason}): {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct TestRepo {
        _temp: TempDir,
        root: PathBuf,
        remote: PathBuf,
    }

    impl TestRepo {
        async fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let root = temp.path().join("checkout");
            let remote = temp.path().join("origin.git");
            tokio::fs::create_dir(&root).await.unwrap();
            assert_git(temp.path(), ["init", "--bare", "--quiet", remote.to_str().unwrap()]).await;
            assert_git(&root, ["init", "--quiet", "--initial-branch=main"]).await;
            assert_git(&root, ["config", "user.name", "Test User"]).await;
            assert_git(&root, ["config", "user.email", "test@example.invalid"]).await;
            tokio::fs::write(root.join("tracked.txt"), "initial\n").await.unwrap();
            assert_git(&root, ["add", "tracked.txt"]).await;
            assert_git(&root, ["commit", "--quiet", "-m", "initial"]).await;
            assert_git(&root, ["remote", "add", "origin", remote.to_str().unwrap()]).await;
            assert_git(&root, ["push", "--quiet", "--set-upstream", "origin", "main"]).await;
            Self {
                _temp: temp,
                root,
                remote,
            }
        }

        async fn commit(&self, file: &str, content: &str, message: &str) {
            tokio::fs::write(self.root.join(file), content).await.unwrap();
            assert_git(&self.root, ["add", file]).await;
            assert_git(&self.root, ["commit", "--quiet", "-m", message]).await;
        }

        async fn publish(&self, file: &str, content: &str, message: &str) {
            let publisher = self._temp.path().join("publisher");
            assert_git(
                self._temp.path(),
                [
                    "clone",
                    "--quiet",
                    "--branch",
                    "main",
                    self.remote.to_str().unwrap(),
                    publisher.to_str().unwrap(),
                ],
            )
            .await;
            assert_git(&publisher, ["config", "user.name", "Test User"]).await;
            assert_git(&publisher, ["config", "user.email", "test@example.invalid"]).await;
            tokio::fs::write(publisher.join(file), content).await.unwrap();
            assert_git(&publisher, ["add", file]).await;
            assert_git(&publisher, ["commit", "--quiet", "-m", message]).await;
            assert_git(&publisher, ["push", "--quiet", "origin", "main"]).await;
        }

        async fn head(&self) -> String {
            git_text(&self.root, ["rev-parse", "HEAD"]).await.unwrap()
        }

        async fn git_path(&self, name: &str) -> PathBuf {
            let path = git_text(&self.root, ["rev-parse", "--git-path", name]).await.unwrap();
            let path = Path::new(&path);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                self.root.join(path)
            }
        }
    }

    async fn assert_git<I, S>(directory: &Path, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = git_output(directory, args).await.unwrap();
        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    async fn admit(workspace: &str) -> Result<(), TeamError> {
        apply(inspect(workspace).await?).await
    }

    fn assert_reason(error: TeamError, reason: &str) {
        assert!(
            error.to_string().contains(reason),
            "expected rejection reason {reason}, got: {error}"
        );
    }

    #[tokio::test]
    async fn non_git_workspace_keeps_existing_behavior() {
        let workspace = TempDir::new().unwrap();
        admit(workspace.path().to_str().unwrap()).await.unwrap();
    }

    #[tokio::test]
    async fn current_branch_is_admitted_without_checkout_mutation() {
        let repo = TestRepo::new().await;
        tokio::fs::write(repo.root.join("tracked.txt"), "uncommitted but current\n")
            .await
            .unwrap();
        let before = repo.head().await;
        admit(repo.root.to_str().unwrap()).await.unwrap();
        assert_eq!(repo.head().await, before);
        assert_eq!(
            tokio::fs::read_to_string(repo.root.join("tracked.txt")).await.unwrap(),
            "uncommitted but current\n"
        );
    }

    #[tokio::test]
    async fn clean_behind_branch_fast_forwards_to_refreshed_upstream() {
        let repo = TestRepo::new().await;
        repo.publish("tracked.txt", "upstream\n", "upstream change").await;
        admit(repo.root.to_str().unwrap()).await.unwrap();
        let head = repo.head().await;
        let upstream = git_text(&repo.root, ["rev-parse", "@{upstream}"]).await.unwrap();
        assert_eq!(head, upstream);
        assert_eq!(
            tokio::fs::read_to_string(repo.root.join("tracked.txt")).await.unwrap(),
            "upstream\n"
        );
    }

    #[tokio::test]
    async fn ahead_branch_is_rejected_without_resetting_team_workspace() {
        let repo = TestRepo::new().await;
        repo.commit("local.txt", "local\n", "local commit").await;
        let before = repo.head().await;
        assert_reason(admit(repo.root.to_str().unwrap()).await.unwrap_err(), "ahead");
        assert_eq!(repo.head().await, before);
    }

    #[tokio::test]
    async fn diverged_branch_is_rejected() {
        let repo = TestRepo::new().await;
        repo.commit("local.txt", "local\n", "local commit").await;
        repo.publish("remote.txt", "remote\n", "remote commit").await;
        let before = repo.head().await;
        assert_reason(admit(repo.root.to_str().unwrap()).await.unwrap_err(), "diverged");
        assert_eq!(repo.head().await, before);
    }

    #[tokio::test]
    async fn detached_head_and_missing_upstream_are_rejected() {
        let detached = TestRepo::new().await;
        assert_git(&detached.root, ["checkout", "--quiet", "--detach"]).await;
        assert_reason(
            admit(detached.root.to_str().unwrap()).await.unwrap_err(),
            "detached_head",
        );

        let untracked = TestRepo::new().await;
        assert_git(&untracked.root, ["branch", "--unset-upstream"]).await;
        assert_reason(
            admit(untracked.root.to_str().unwrap()).await.unwrap_err(),
            "missing_upstream",
        );
    }

    #[tokio::test]
    async fn upstream_identity_change_during_refresh_fails_closed() {
        let repo = TestRepo::new().await;
        let before = repo.head().await;
        let plan = inspect(repo.root.to_str().unwrap()).await.unwrap();
        assert_git(&repo.root, ["update-ref", "refs/remotes/origin/alternate", &before]).await;
        assert_git(&repo.root, ["branch", "--set-upstream-to=origin/alternate", "main"]).await;
        let error = apply(plan).await.unwrap_err();
        assert_reason(error, "identity_changed");
        assert_eq!(repo.head().await, before);
    }

    #[tokio::test]
    async fn fetch_failure_is_rejected_without_changing_head() {
        let repo = TestRepo::new().await;
        assert_git(&repo.root, ["remote", "set-url", "origin", "/missing/origin.git"]).await;
        let before = repo.head().await;
        assert_reason(
            admit(repo.root.to_str().unwrap()).await.unwrap_err(),
            "upstream_refresh_failed",
        );
        assert_eq!(repo.head().await, before);
    }

    #[tokio::test]
    async fn tracked_dirty_untracked_and_operation_states_are_rejected_and_preserved() {
        let dirty = TestRepo::new().await;
        dirty.publish("tracked.txt", "upstream\n", "upstream change").await;
        tokio::fs::write(dirty.root.join("tracked.txt"), "user edit\n")
            .await
            .unwrap();
        assert_reason(admit(dirty.root.to_str().unwrap()).await.unwrap_err(), "dirty_worktree");
        assert_eq!(
            tokio::fs::read_to_string(dirty.root.join("tracked.txt")).await.unwrap(),
            "user edit\n"
        );

        let operation = TestRepo::new().await;
        tokio::fs::write(operation.git_path("MERGE_HEAD").await, "in-progress\n")
            .await
            .unwrap();
        assert_reason(
            admit(operation.root.to_str().unwrap()).await.unwrap_err(),
            "operation_in_progress",
        );

        let cherry_pick = TestRepo::new().await;
        tokio::fs::write(cherry_pick.git_path("CHERRY_PICK_HEAD").await, "in-progress\n")
            .await
            .unwrap();
        assert_reason(
            admit(cherry_pick.root.to_str().unwrap()).await.unwrap_err(),
            "operation_in_progress",
        );

        let rebase = TestRepo::new().await;
        tokio::fs::create_dir_all(rebase.git_path("rebase-apply").await)
            .await
            .unwrap();
        assert_reason(
            admit(rebase.root.to_str().unwrap()).await.unwrap_err(),
            "operation_in_progress",
        );
    }

    #[tokio::test]
    async fn conflicting_untracked_file_is_never_overwritten() {
        let repo = TestRepo::new().await;
        tokio::fs::write(repo.root.join("incoming.txt"), "user file\n")
            .await
            .unwrap();
        repo.publish("incoming.txt", "remote file\n", "upstream add").await;
        let before = repo.head().await;
        assert_reason(
            admit(repo.root.to_str().unwrap()).await.unwrap_err(),
            "untracked_path_conflict",
        );
        assert_eq!(repo.head().await, before);
        assert_eq!(
            tokio::fs::read_to_string(repo.root.join("incoming.txt")).await.unwrap(),
            "user file\n"
        );
    }

    #[tokio::test]
    async fn conflicting_ignored_file_is_never_overwritten() {
        let repo = TestRepo::new().await;
        let exclude = repo.git_path("info/exclude").await;
        tokio::fs::write(exclude, "incoming.txt\n").await.unwrap();
        tokio::fs::write(repo.root.join("incoming.txt"), "ignored user file\n")
            .await
            .unwrap();
        repo.publish("incoming.txt", "remote file\n", "upstream add").await;
        let before = repo.head().await;
        assert_reason(
            admit(repo.root.to_str().unwrap()).await.unwrap_err(),
            "untracked_path_conflict",
        );
        assert_eq!(repo.head().await, before);
        assert_eq!(
            tokio::fs::read_to_string(repo.root.join("incoming.txt")).await.unwrap(),
            "ignored user file\n"
        );
    }

    #[tokio::test]
    async fn unrelated_untracked_and_ignored_build_output_survive_safe_fast_forward() {
        let repo = TestRepo::new().await;
        let exclude = repo.git_path("info/exclude").await;
        tokio::fs::write(&exclude, "build-output/\n").await.unwrap();
        tokio::fs::create_dir_all(repo.root.join("build-output/cache"))
            .await
            .unwrap();
        tokio::fs::write(repo.root.join("build-output/cache/index"), "keep ignored output\n")
            .await
            .unwrap();
        tokio::fs::write(repo.root.join("developer-notes.txt"), "keep untracked\n")
            .await
            .unwrap();
        repo.publish("tracked.txt", "upstream\n", "upstream change").await;

        admit(repo.root.to_str().unwrap()).await.unwrap();
        assert_eq!(
            repo.head().await,
            git_text(&repo.root, ["rev-parse", "@{upstream}"]).await.unwrap()
        );
        assert_eq!(
            tokio::fs::read_to_string(repo.root.join("build-output/cache/index"))
                .await
                .unwrap(),
            "keep ignored output\n"
        );
        assert_eq!(
            tokio::fs::read_to_string(repo.root.join("developer-notes.txt"))
                .await
                .unwrap(),
            "keep untracked\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn network_git_runner_disables_prompts_and_kills_timed_out_process_tree() {
        let mut command = Builder::clean_cli("sh");
        apply_noninteractive_git_env(&mut command);
        command.args([
            "-c",
            "test \"$GIT_TERMINAL_PROMPT\" = 0 && test \"$GCM_INTERACTIVE\" = Never && test \"$SSH_ASKPASS_REQUIRE\" = never && test \"$GIT_SSH_COMMAND\" = 'ssh -o BatchMode=yes' && sleep 30",
        ]);
        let started = std::time::Instant::now();
        let result = run_bounded_network_command(command, Duration::from_millis(100)).await;
        assert_eq!(result.unwrap_err(), NetworkCommandError::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
