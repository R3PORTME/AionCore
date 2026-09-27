//! Git workspace freshness admission for Team fresh-run.

use std::path::{Path, PathBuf};

use aionui_runtime::Builder;
use tracing::{info, warn};

use crate::error::TeamError;

/// Ensure a Git-backed workspace is current before fresh-run mutates Team state.
/// Non-Git workspaces retain the existing fresh-run behavior.
pub(crate) async fn ensure_fresh(workspace: &str) -> Result<(), TeamError> {
    let root = match git_root(workspace).await? {
        None => return Ok(()),
        Some(root) => root,
    };
    let path = root.to_string_lossy().into_owned();

    let branch = match git_text(&root, ["symbolic-ref", "--quiet", "--short", "HEAD"]).await {
        Ok(branch) if !branch.is_empty() => branch,
        _ => return reject(&path, "detached_head", "workspace is on a detached HEAD"),
    };
    let upstream = match git_text(
        &root,
        ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
    )
    .await
    {
        Ok(upstream) if !upstream.is_empty() => upstream,
        _ => return reject(&path, "missing_upstream", "workspace branch has no resolvable upstream"),
    };
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
        let refreshed = git_output(&root, ["fetch", "--quiet", "--no-tags", remote]).await;
        if !matches!(refreshed, Ok(output) if output.status.success()) {
            return reject(
                &path,
                "upstream_refresh_failed",
                "could not refresh the configured Git upstream",
            );
        }
        let live_ref = git_output(&root, ["ls-remote", "--exit-code", remote, merge_ref]).await;
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
    if head == target {
        let final_branch = git_text(&root, ["symbolic-ref", "--quiet", "--short", "HEAD"]).await;
        let final_upstream = git_text(
            &root,
            ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
        )
        .await;
        let final_head = git_text(&root, ["rev-parse", "--verify", "HEAD"]).await;
        let final_target = git_text(&root, ["rev-parse", "--verify", &upstream]).await;
        if !matches!(final_branch, Ok(ref value) if value == &branch)
            || !matches!(final_upstream, Ok(ref value) if value == &upstream)
            || !matches!(final_head, Ok(ref value) if value == &head)
            || !matches!(final_target, Ok(ref value) if value == &target)
        {
            return reject(
                &path,
                "identity_changed",
                "branch or upstream identity changed during freshness admission",
            );
        }
        info!(workspace = %path, branch, upstream, outcome = "current", "team fresh-run workspace admitted");
        return Ok(());
    }

    if is_ancestor(&root, &head, &target).await? {
        ensure_clean(&root, &path).await?;
        if has_operation_in_progress(&root).await? {
            return reject(
                &path,
                "operation_in_progress",
                "workspace has an in-progress Git operation",
            );
        }
        let branch_before_ff = git_text(&root, ["symbolic-ref", "--quiet", "--short", "HEAD"]).await;
        let upstream_before_ff = git_text(
            &root,
            ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
        )
        .await;
        let head_before_ff = git_text(&root, ["rev-parse", "--verify", "HEAD"]).await;
        let target_before_ff = git_text(&root, ["rev-parse", "--verify", &upstream]).await;
        if !matches!(branch_before_ff, Ok(ref value) if value == &branch)
            || !matches!(upstream_before_ff, Ok(ref value) if value == &upstream)
            || !matches!(head_before_ff, Ok(ref value) if value == &head)
            || !matches!(target_before_ff, Ok(ref value) if value == &target)
        {
            return reject(
                &path,
                "identity_changed",
                "branch or upstream identity changed before fast-forward",
            );
        }

        // Git's ff-only merge refuses non-fast-forward and checkout collisions;
        // no reset/stash/clean or other destructive fallback is used.
        let fast_forward = git_output(&root, ["merge", "--ff-only", &target]).await;
        if !matches!(fast_forward, Ok(output) if output.status.success()) {
            return reject(&path, "fast_forward_failed", "safe fast-forward could not be completed");
        }

        ensure_clean(&root, &path).await?;
        if has_operation_in_progress(&root).await? {
            return reject(
                &path,
                "operation_in_progress",
                "workspace has an in-progress Git operation",
            );
        }
        let final_branch = git_text(&root, ["symbolic-ref", "--quiet", "--short", "HEAD"]).await;
        let final_upstream = git_text(
            &root,
            ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
        )
        .await;
        let final_head = git_text(&root, ["rev-parse", "--verify", "HEAD"]).await;
        let final_target = git_text(&root, ["rev-parse", "--verify", &upstream]).await;
        if !matches!(final_branch, Ok(ref value) if value == &branch)
            || !matches!(final_upstream, Ok(ref value) if value == &upstream)
            || !matches!(final_head, Ok(ref value) if value == &target)
            || !matches!(final_target, Ok(ref value) if value == &target)
        {
            return reject(
                &path,
                "identity_changed",
                "branch or upstream identity changed during fast-forward",
            );
        }

        info!(workspace = %path, branch, upstream, outcome = "fast_forwarded", "team fresh-run workspace admitted");
        return Ok(());
    }

    if is_ancestor(&root, &target, &head).await? {
        return reject(
            &path,
            "ahead",
            "workspace branch contains commits ahead of its upstream",
        );
    }
    reject(&path, "diverged", "workspace branch has diverged from its upstream")
}

async fn git_root(workspace: &str) -> Result<Option<PathBuf>, TeamError> {
    match git_output(Path::new(workspace), ["rev-parse", "--show-toplevel"]).await {
        Ok(output) if output.status.success() => {
            let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            Ok(Some(PathBuf::from(root)))
        }
        Ok(output) if String::from_utf8_lossy(&output.stderr).contains("not a git repository") => {
            if has_git_metadata(Path::new(workspace)) {
                Err(TeamError::InvalidRequest(
                    "could not inspect Git workspace metadata; freshness cannot be verified".to_owned(),
                ))
            } else {
                Ok(None)
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if has_git_metadata(Path::new(workspace)) {
                Err(TeamError::InvalidRequest(
                    "Git is unavailable; workspace freshness cannot be verified".to_owned(),
                ))
            } else {
                Ok(None)
            }
        }
        _ => Err(TeamError::InvalidRequest(
            "could not identify the Git workspace; freshness cannot be verified".to_owned(),
        )),
    }
}

fn has_git_metadata(workspace: &Path) -> bool {
    workspace.ancestors().any(|path| path.join(".git").exists())
}

async fn git_output<I, S>(directory: &Path, args: I) -> std::io::Result<std::process::Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Builder::clean_cli("git");
    command.args(args).current_dir(directory).env("LC_ALL", "C");
    command.output().await
}

async fn git_text<I, S>(directory: &Path, args: I) -> Result<String, TeamError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = git_output(directory, args)
        .await
        .map_err(|_| TeamError::InvalidRequest("could not inspect Git workspace state".to_owned()))?;
    if !output.status.success() {
        return Err(TeamError::InvalidRequest(
            "could not inspect Git workspace state".to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

async fn ensure_clean(root: &Path, workspace: &str) -> Result<(), TeamError> {
    let status = git_text(root, ["status", "--porcelain=v1", "--untracked-files=all"]).await?;
    let unknown_files = git_text(root, ["ls-files", "--others", "--ignored", "--exclude-standard"]).await?;
    if !status.is_empty() || !unknown_files.is_empty() {
        return reject(
            workspace,
            "dirty_worktree",
            "workspace has tracked, staged, or untracked user content",
        );
    }
    Ok(())
}

async fn is_ancestor(root: &Path, older: &str, newer: &str) -> Result<bool, TeamError> {
    match git_output(root, ["merge-base", "--is-ancestor", older, newer]).await {
        Ok(output) if output.status.success() => Ok(true),
        Ok(output) if output.status.code() == Some(1) => Ok(false),
        _ => Err(TeamError::InvalidRequest(
            "could not classify Git upstream history".to_owned(),
        )),
    }
}

async fn has_operation_in_progress(root: &Path) -> Result<bool, TeamError> {
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
    warn!(workspace, reason, "team fresh-run Git workspace rejected");
    Err(TeamError::InvalidRequest(message.to_owned()))
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

    #[tokio::test]
    async fn non_git_workspace_keeps_existing_behavior() {
        let workspace = TempDir::new().unwrap();
        ensure_fresh(workspace.path().to_str().unwrap()).await.unwrap();
    }

    #[tokio::test]
    async fn current_branch_is_admitted_without_checkout_mutation() {
        let repo = TestRepo::new().await;
        tokio::fs::write(repo.root.join("tracked.txt"), "uncommitted but current\n")
            .await
            .unwrap();
        let before = repo.head().await;
        ensure_fresh(repo.root.to_str().unwrap()).await.unwrap();
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
        ensure_fresh(repo.root.to_str().unwrap()).await.unwrap();
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
        assert!(ensure_fresh(repo.root.to_str().unwrap()).await.is_err());
        assert_eq!(repo.head().await, before);
    }

    #[tokio::test]
    async fn diverged_branch_is_rejected() {
        let repo = TestRepo::new().await;
        repo.commit("local.txt", "local\n", "local commit").await;
        repo.publish("remote.txt", "remote\n", "remote commit").await;
        let before = repo.head().await;
        assert!(ensure_fresh(repo.root.to_str().unwrap()).await.is_err());
        assert_eq!(repo.head().await, before);
    }

    #[tokio::test]
    async fn detached_head_and_missing_upstream_are_rejected() {
        let detached = TestRepo::new().await;
        assert_git(&detached.root, ["checkout", "--quiet", "--detach"]).await;
        assert!(ensure_fresh(detached.root.to_str().unwrap()).await.is_err());

        let untracked = TestRepo::new().await;
        assert_git(&untracked.root, ["branch", "--unset-upstream"]).await;
        assert!(ensure_fresh(untracked.root.to_str().unwrap()).await.is_err());
    }

    #[tokio::test]
    async fn fetch_failure_is_rejected_without_changing_head() {
        let repo = TestRepo::new().await;
        assert_git(&repo.root, ["remote", "set-url", "origin", "/missing/origin.git"]).await;
        let before = repo.head().await;
        assert!(ensure_fresh(repo.root.to_str().unwrap()).await.is_err());
        assert_eq!(repo.head().await, before);
    }

    #[tokio::test]
    async fn tracked_dirty_untracked_and_operation_states_are_rejected_and_preserved() {
        let dirty = TestRepo::new().await;
        dirty.publish("tracked.txt", "upstream\n", "upstream change").await;
        tokio::fs::write(dirty.root.join("tracked.txt"), "user edit\n")
            .await
            .unwrap();
        assert!(ensure_fresh(dirty.root.to_str().unwrap()).await.is_err());
        assert_eq!(
            tokio::fs::read_to_string(dirty.root.join("tracked.txt")).await.unwrap(),
            "user edit\n"
        );

        let untracked = TestRepo::new().await;
        untracked.publish("tracked.txt", "upstream\n", "upstream change").await;
        tokio::fs::write(untracked.root.join("user.txt"), "keep\n")
            .await
            .unwrap();
        assert!(ensure_fresh(untracked.root.to_str().unwrap()).await.is_err());
        assert_eq!(
            tokio::fs::read_to_string(untracked.root.join("user.txt"))
                .await
                .unwrap(),
            "keep\n"
        );

        let operation = TestRepo::new().await;
        let merge_head = git_text(&operation.root, ["rev-parse", "--git-path", "MERGE_HEAD"])
            .await
            .unwrap();
        let merge_head = Path::new(&merge_head);
        let merge_head = if merge_head.is_absolute() {
            merge_head.to_path_buf()
        } else {
            operation.root.join(merge_head)
        };
        tokio::fs::write(merge_head, "in-progress\n").await.unwrap();
        assert!(ensure_fresh(operation.root.to_str().unwrap()).await.is_err());
    }

    #[tokio::test]
    async fn conflicting_untracked_file_is_never_overwritten() {
        let repo = TestRepo::new().await;
        tokio::fs::write(repo.root.join("incoming.txt"), "user file\n")
            .await
            .unwrap();
        repo.publish("incoming.txt", "remote file\n", "upstream add").await;
        let before = repo.head().await;
        assert!(ensure_fresh(repo.root.to_str().unwrap()).await.is_err());
        assert_eq!(repo.head().await, before);
        assert_eq!(
            tokio::fs::read_to_string(repo.root.join("incoming.txt")).await.unwrap(),
            "user file\n"
        );
    }

    #[tokio::test]
    async fn conflicting_ignored_file_is_never_overwritten() {
        let repo = TestRepo::new().await;
        let exclude = git_text(&repo.root, ["rev-parse", "--git-path", "info/exclude"])
            .await
            .unwrap();
        let exclude = Path::new(&exclude);
        let exclude = if exclude.is_absolute() {
            exclude.to_path_buf()
        } else {
            repo.root.join(exclude)
        };
        tokio::fs::write(exclude, "incoming.txt\n").await.unwrap();
        tokio::fs::write(repo.root.join("incoming.txt"), "ignored user file\n")
            .await
            .unwrap();
        repo.publish("incoming.txt", "remote file\n", "upstream add").await;
        let before = repo.head().await;
        assert!(ensure_fresh(repo.root.to_str().unwrap()).await.is_err());
        assert_eq!(repo.head().await, before);
        assert_eq!(
            tokio::fs::read_to_string(repo.root.join("incoming.txt")).await.unwrap(),
            "ignored user file\n"
        );
    }
}
