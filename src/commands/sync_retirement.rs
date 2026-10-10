//! Effectful adapter for retiring BB-owned worktrees during sync.
//!
//! The policy side stays in `engine::sync_lifecycle`; this module uses BB's
//! registry as the ownership authority and addresses lifecycle calls by the
//! resolved environment ID only.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BbEnvironmentIdentity {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    registered_path: PathBuf,
}

/// Version 2 adds `bundle_prerequisites`; version 1 artifacts are full bundles.
const RECOVERY_MANIFEST_VERSION: u32 = 2;

// Patches are compared byte-for-byte and replayed with `git apply`, so user diff
// config (mnemonicPrefix, noprefix, color, external diff, textconv) must not shape them.
const UNSTAGED_PATCH_ARGS: [&str; 7] = [
    "diff",
    "--binary",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];
const STAGED_PATCH_ARGS: [&str; 9] = [
    "diff",
    "--binary",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--cached",
    "HEAD",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RecoveryManifest {
    version: u32,
    operation_id: String,
    branch: String,
    original_path: String,
    observed_head: String,
    branch_ref: String,
    /// Trunk commits the bundle omits. Empty means a self-contained bundle.
    #[serde(default)]
    bundle_prerequisites: Vec<BundlePrerequisite>,
    staged_patch: String,
    unstaged_patch: String,
    untracked: Vec<String>,
}

/// A bundle prerequisite: the exact merge-base of the archived tip with an
/// observed trunk ref. The recovery ref pins the tip, so every prerequisite
/// (an ancestor of the tip) stays in common Git storage with the bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct BundlePrerequisite {
    trunk_ref: String,
    trunk_oid: String,
    merge_base: String,
}

/// Capability returned only after the recovery data and manifest are durable.
#[derive(Debug, Clone)]
pub(crate) struct RecoveryReady {
    directory: PathBuf,
    manifest: RecoveryManifest,
}

impl RecoveryReady {
    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    pub(crate) fn operation_id(&self) -> &str {
        &self.manifest.operation_id
    }
}

fn git_output(workdir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workdir)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProtectedOperation {
    None,
    Known,
    Unknown,
}

/// Observe Git operations from the target worktree's own git directory.
/// Known merge/rebase markers keep their established diagnostic; newer or
/// otherwise unclassified markers remain protected with an unknown reason.
/// Inspection errors are propagated so callers can fail closed.
pub(crate) fn protected_operation(worktree: &Path) -> Result<ProtectedOperation> {
    let git_dir = git2::Repository::open(worktree)
        .with_context(|| format!("cannot inspect worktree {}", worktree.display()))?
        .path()
        .to_path_buf();
    for (marker, state) in [
        ("rebase-merge", ProtectedOperation::Known),
        ("rebase-apply", ProtectedOperation::Known),
        ("MERGE_HEAD", ProtectedOperation::Known),
        ("CHERRY_PICK_HEAD", ProtectedOperation::Known),
        ("REVERT_HEAD", ProtectedOperation::Known),
        ("REBASE_HEAD", ProtectedOperation::Known),
        ("BISECT_START", ProtectedOperation::Unknown),
        ("sequencer", ProtectedOperation::Unknown),
    ] {
        if git_dir.join(marker).try_exists()? {
            return Ok(state);
        }
    }
    Ok(ProtectedOperation::None)
}

pub(crate) fn has_protected_operation(worktree: &Path) -> Result<bool> {
    Ok(protected_operation(worktree)? != ProtectedOperation::None)
}

/// Delete only the observed local tip. Unlike `branch -D`, update-ref has
/// an expected-old-OID guard, so a ref advanced by teardown cannot be lost.
/// Preserve branch -D's checkout protection, including detached operations;
/// failed inspection is retention, never permission to remove a ref.
pub(crate) fn delete_branch_at_expected_oid(
    workdir: &Path,
    branch: &str,
    expected_oid: &str,
) -> Result<()> {
    for worktree in crate::git::GitRepo::list_worktrees_in(workdir)? {
        if worktree.branch.as_deref() == Some(branch) {
            bail!("branch is used by worktree {}", worktree.path.display());
        }
        // Fresh in-process read of the worktree's Git dir (its `.git` link);
        // a worktree that cannot be opened is retention, as before.
        if has_protected_operation(&worktree.path)? {
            bail!(
                "worktree has a protected operation: {}",
                worktree.path.display()
            );
        }
    }
    git_output(
        workdir,
        &[
            "update-ref",
            "--no-deref",
            "-d",
            &format!("refs/heads/{branch}"),
            expected_oid,
        ],
    )
    .context("branch head changed or guarded deletion failed")?;
    // Match branch -D's config cleanup, but only after the guarded ref delete
    // succeeds. Config errors cannot undo a completed ref deletion.
    let cleanup = Command::new("git")
        .args(["config", "--remove-section", &format!("branch.{branch}")])
        .current_dir(workdir)
        .output();
    match cleanup {
        Ok(out) if out.status.success() || out.status.code() == Some(5) => {}
        Ok(out) => eprintln!(
            "Deleted '{branch}', but could not remove its Git config: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(error) => eprintln!("Deleted '{branch}', but could not remove its Git config: {error}"),
    }
    Ok(())
}

fn write_git_output(workdir: &Path, args: &[&str], destination: &Path) -> Result<()> {
    let bytes = git_output(workdir, args)?;
    std::fs::write(destination, bytes)
        .with_context(|| format!("failed to write {}", destination.display()))
}

fn copy_untracked(source: &Path, destination: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(std::fs::read_link(source)?, destination)?;
        #[cfg(not(unix))]
        bail!("symlink recovery is unsupported on this platform");
    } else if metadata.is_dir() {
        std::fs::create_dir_all(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_untracked(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source, destination)?;
    } else {
        bail!(
            "unsupported untracked filesystem entry: {}",
            source.display()
        );
    }
    Ok(())
}

fn untracked_paths(workdir: &Path) -> Result<Vec<PathBuf>> {
    let output = git_output(
        workdir,
        // Include ignored local files too: they are just as recoverable as
        // ordinary untracked files and the worktree may be removed afterward.
        &["ls-files", "--others", "-z"],
    )?;
    let mut paths = Vec::new();
    for path in output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        #[cfg(unix)]
        let path = {
            use std::os::unix::ffi::OsStrExt;
            PathBuf::from(std::ffi::OsStr::from_bytes(path))
        };
        #[cfg(not(unix))]
        let path = PathBuf::from(std::str::from_utf8(path).context("untracked path is not UTF-8")?);
        if path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            bail!("Git returned an unsafe untracked path");
        }
        if path.to_str().is_none() {
            bail!("non-UTF-8 untracked paths cannot be represented in recovery manifest");
        }
        paths.push(path);
    }
    Ok(paths)
}

fn sync_file(path: &Path) -> Result<()> {
    std::fs::File::open(path)
        .with_context(|| format!("failed to open {} for durability sync", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))
}

fn sync_directory_chain(path: &Path, stop: &Path, sync: fn(&Path) -> Result<()>) -> Result<()> {
    let mut current = path;
    loop {
        sync(current)?;
        if current == stop {
            return Ok(());
        }
        current = current
            .parent()
            .context("recovery directory escaped common Git storage")?;
        if !current.starts_with(stop) {
            bail!("recovery directory is outside common Git storage");
        }
    }
}

/// Save the exact branch commit, staged/unstaged patches, and untracked files
/// outside the target worktree. Repeating a completed operation is idempotent;
/// a partial artifact fails closed and is left untouched for inspection.
pub(crate) fn save_recovery(
    common_git_dir: &Path,
    worktree_path: &Path,
    operation_id: &str,
    branch: &str,
    observed_head: &str,
    trunk_refs: &[String],
) -> Result<RecoveryReady> {
    save_recovery_with_sync(
        common_git_dir,
        worktree_path,
        operation_id,
        branch,
        observed_head,
        trunk_refs,
        sync_file,
    )
}

/// Observe each trunk ref and its merge-base with the archived tip. `None`
/// (use a self-contained bundle) when no trunk ref is observable, an observed
/// ref has no merge-base, or the tip is already in trunk (Git refuses an
/// empty bundle).
fn bundle_prerequisites(
    worktree: &Path,
    observed_head: &str,
    trunk_refs: &[String],
) -> Option<Vec<BundlePrerequisite>> {
    let read = |args: &[&str]| -> Option<String> {
        let out = git_output(worktree, args).ok()?;
        Some(String::from_utf8(out).ok()?.trim().to_string())
    };
    let mut prerequisites = Vec::new();
    for trunk_ref in trunk_refs {
        let Some(trunk_oid) = read(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{trunk_ref}^{{commit}}"),
        ]) else {
            continue;
        };
        let merge_base = read(&["merge-base", observed_head, &trunk_oid])?;
        if merge_base == observed_head {
            return None;
        }
        prerequisites.push(BundlePrerequisite {
            trunk_ref: trunk_ref.clone(),
            trunk_oid,
            merge_base,
        });
    }
    (!prerequisites.is_empty()).then_some(prerequisites)
}

/// Write the branch bundle. A trunk-bounded bundle is kept only after `git
/// bundle verify` accepts it in this repository; any failure falls back to the
/// self-contained bundle. Returns the prerequisites the written bundle needs.
fn create_branch_bundle(
    worktree: &Path,
    bundle: &Path,
    branch_ref: &str,
    observed_head: &str,
    trunk_refs: &[String],
) -> Result<Vec<BundlePrerequisite>> {
    let bundle_path = bundle.to_string_lossy().to_string();
    if let Some(prerequisites) = bundle_prerequisites(worktree, observed_head, trunk_refs) {
        let exclusions: Vec<String> = prerequisites
            .iter()
            .map(|prerequisite| format!("^{}", prerequisite.merge_base))
            .collect();
        let mut args = vec!["bundle", "create", "--quiet", &bundle_path, branch_ref];
        args.extend(exclusions.iter().map(String::as_str));
        if git_output(worktree, &args).is_ok()
            && git_output(worktree, &["bundle", "verify", "--quiet", &bundle_path]).is_ok()
        {
            return Ok(prerequisites);
        }
        let _ = std::fs::remove_file(bundle);
    }
    let bundle_status = Command::new("git")
        .args(["bundle", "create", &bundle_path, branch_ref])
        .current_dir(worktree)
        .status()
        .context("failed to create branch recovery bundle")?;
    if !bundle_status.success() {
        bail!("failed to create branch recovery bundle; target retained");
    }
    Ok(Vec::new())
}

fn save_recovery_with_sync(
    common_git_dir: &Path,
    worktree_path: &Path,
    operation_id: &str,
    branch: &str,
    observed_head: &str,
    trunk_refs: &[String],
    sync: fn(&Path) -> Result<()>,
) -> Result<RecoveryReady> {
    if operation_id.is_empty()
        || !operation_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        bail!("sync operation ID is invalid; target retained");
    }
    let worktree = canonical(worktree_path)?;
    let root = git_output(&worktree, &["rev-parse", "--show-toplevel"])?;
    if canonical(Path::new(std::str::from_utf8(&root)?.trim()))? != worktree {
        bail!("target is no longer the observed Git worktree root");
    }
    let head = String::from_utf8(git_output(&worktree, &["rev-parse", "HEAD^{commit}"])?)?
        .trim()
        .to_string();
    let current_branch = String::from_utf8(git_output(&worktree, &["branch", "--show-current"])?)?
        .trim()
        .to_string();
    if head != observed_head || current_branch != branch {
        bail!("target branch or head changed before recovery; retry sync");
    }

    let branch_ref = format!("refs/stax/recovery/{operation_id}/{branch}");
    let update = Command::new("git")
        .args(["update-ref", &branch_ref, observed_head])
        .current_dir(&worktree)
        .status()
        .context("failed to preserve branch commit under recovery ref")?;
    if !update.success() {
        bail!("could not preserve branch commit under recovery ref");
    }

    let short_head = &observed_head[..observed_head.len().min(12)];
    let safe_branch = branch
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let directory = common_git_dir
        .join("stax")
        .join("recovery")
        .join(operation_id)
        .join(format!("{safe_branch}-{short_head}"));
    let manifest_path = directory.join("manifest.json");
    if manifest_path.is_file() {
        let manifest: RecoveryManifest = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
        if matches!(manifest.version, 1 | RECOVERY_MANIFEST_VERSION)
            && manifest.operation_id == operation_id
            && manifest.branch == branch
            && manifest.original_path == worktree.to_string_lossy()
            && manifest.observed_head == observed_head
        {
            return Ok(RecoveryReady {
                directory,
                manifest,
            });
        }
        bail!("existing recovery artifact does not match this retirement; target retained");
    }
    if directory.exists() {
        bail!(
            "incomplete recovery artifact exists at {}; target retained",
            directory.display()
        );
    }
    std::fs::create_dir_all(&directory)?;
    sync_directory_chain(&directory, common_git_dir, sync)?;

    let bundle = directory.join("branch.bundle");
    let bundle_prerequisites =
        create_branch_bundle(&worktree, &bundle, &branch_ref, observed_head, trunk_refs)?;
    sync_file(&bundle)?;
    sync(&directory)?;

    let staged_patch = "staged.patch".to_string();
    let unstaged_patch = "unstaged.patch".to_string();
    write_git_output(
        &worktree,
        &STAGED_PATCH_ARGS,
        &directory.join(&staged_patch),
    )?;
    sync_file(&directory.join(&staged_patch))?;
    write_git_output(
        &worktree,
        &UNSTAGED_PATCH_ARGS,
        &directory.join(&unstaged_patch),
    )?;
    sync_file(&directory.join(&unstaged_patch))?;

    let paths = untracked_paths(&worktree)?;
    let mut manifest_paths = Vec::with_capacity(paths.len());
    for (index, relative) in paths.iter().enumerate() {
        let archive_path = format!("untracked/{index}");
        copy_untracked(&worktree.join(relative), &directory.join(&archive_path))?;
        sync_recovery_tree(&directory.join(&archive_path), sync)?;
        sync(&directory.join("untracked"))?;
        manifest_paths.push(format!("{}\t{archive_path}", relative.to_str().unwrap()));
    }

    let manifest = RecoveryManifest {
        version: RECOVERY_MANIFEST_VERSION,
        operation_id: operation_id.to_string(),
        branch: branch.to_string(),
        original_path: worktree.to_string_lossy().into_owned(),
        observed_head: observed_head.to_string(),
        branch_ref,
        bundle_prerequisites,
        staged_patch,
        unstaged_patch,
        untracked: manifest_paths,
    };
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    let temp = directory.join("manifest.json.tmp");
    std::fs::write(&temp, bytes)?;
    sync_file(&temp)?;
    std::fs::rename(temp, &manifest_path)?;
    sync(&directory)?;
    Ok(RecoveryReady {
        directory,
        manifest,
    })
}

fn sync_recovery_tree(path: &Path, sync: fn(&Path) -> Result<()>) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            sync_recovery_tree(&entry?.path(), sync)?;
        }
        sync(path)?;
    } else if !metadata.file_type().is_symlink() {
        sync(path)?;
    }
    Ok(())
}

/// Compare permission state that this platform can reliably recover for an
/// untracked regular file. Unix preserves the full chmod permission/special
/// bits; other platforms expose the portable read-only bit.
fn same_recoverable_permissions(source: &std::fs::Metadata, saved: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        source.permissions().mode() & 0o7777 == saved.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        source.permissions().readonly() == saved.permissions().readonly()
    }
}

/// Recheck the target against the durable archive immediately before removal.
pub(crate) fn verify_recovery(recovery: &RecoveryReady, worktree_path: &Path) -> Result<()> {
    let worktree = canonical(worktree_path)?;
    let manifest = &recovery.manifest;
    if worktree.as_path() != Path::new(&manifest.original_path) {
        bail!("worktree path changed after recovery; retry sync");
    }
    let root = git_output(&worktree, &["rev-parse", "--show-toplevel"])?;
    if canonical(Path::new(std::str::from_utf8(&root)?.trim()))? != worktree {
        bail!("target is no longer a worktree root; retry sync");
    }
    let head = String::from_utf8(git_output(&worktree, &["rev-parse", "HEAD^{commit}"])?)?
        .trim()
        .to_string();
    let branch = String::from_utf8(git_output(&worktree, &["branch", "--show-current"])?)?
        .trim()
        .to_string();
    if head != manifest.observed_head || branch != manifest.branch {
        bail!("branch head or checkout changed after recovery; retry sync");
    }

    let staged = git_output(&worktree, &STAGED_PATCH_ARGS)?;
    let archived_staged = std::fs::read(recovery.directory.join(&manifest.staged_patch))?;
    let unstaged = git_output(&worktree, &UNSTAGED_PATCH_ARGS)?;
    let archived_unstaged = std::fs::read(recovery.directory.join(&manifest.unstaged_patch))?;
    if staged != archived_staged || unstaged != archived_unstaged {
        bail!("tracked work changed after recovery; target retained for retry");
    }

    let current_untracked = untracked_paths(&worktree)?;
    let expected = manifest
        .untracked
        .iter()
        .map(|entry| entry.split_once('\t').map(|(path, _)| path.to_string()))
        .collect::<Option<std::collections::BTreeSet<_>>>()
        .context("invalid recovery manifest path")?;
    let current = current_untracked
        .iter()
        .map(|path| path.to_str().map(str::to_string))
        .collect::<Option<std::collections::BTreeSet<_>>>()
        .context("untracked path is not UTF-8")?;
    if current != expected {
        bail!("untracked files changed after recovery; target retained for retry");
    }
    for entry in &manifest.untracked {
        let (relative, archived) = entry
            .split_once('\t')
            .context("invalid recovery manifest path")?;
        let source = worktree.join(relative);
        let saved = recovery.directory.join(archived);
        let source_meta = std::fs::symlink_metadata(&source)?;
        let saved_meta = std::fs::symlink_metadata(&saved)?;
        if source_meta.file_type().is_symlink() != saved_meta.file_type().is_symlink()
            || source_meta.is_dir() != saved_meta.is_dir()
        {
            bail!("untracked file type changed after recovery; retry sync");
        }
        if source_meta.file_type().is_symlink() {
            if std::fs::read_link(source)? != std::fs::read_link(saved)? {
                bail!("untracked symlink changed after recovery; retry sync");
            }
        } else if source_meta.is_file() {
            if !same_recoverable_permissions(&source_meta, &saved_meta) {
                bail!("untracked file permissions changed after recovery; retry sync");
            }
            if std::fs::read(source)? != std::fs::read(saved)? {
                bail!("untracked file changed after recovery; retry sync");
            }
        }
    }
    Ok(())
}

/// Restore a complete recovery artifact into a checkout at the original head.
#[allow(dead_code)] // The same protocol is printed for manual recovery; tests execute it end to end.
pub(crate) fn restore_recovery(recovery: &RecoveryReady, destination: &Path) -> Result<()> {
    let manifest = &recovery.manifest;
    let destination = canonical(destination)?;
    let head = String::from_utf8(git_output(&destination, &["rev-parse", "HEAD^{commit}"])?)?
        .trim()
        .to_string();
    if head != manifest.observed_head {
        bail!("restore destination is not at the archived branch head");
    }
    let branch_ref = format!("refs/heads/{}", manifest.branch);
    let existing_head = Command::new("git")
        .args(["rev-parse", "--verify", &branch_ref])
        .current_dir(&destination)
        .output()?;
    if !existing_head.status.success()
        || String::from_utf8_lossy(&existing_head.stdout).trim() != manifest.observed_head
    {
        let bundle = recovery.directory.join("branch.bundle");
        let bundle_path = bundle.to_string_lossy().to_string();
        let refspec = format!("{}:{branch_ref}", manifest.branch_ref);
        let fetch = Command::new("git")
            .args(["fetch", &bundle_path, &refspec])
            .current_dir(&destination)
            .status()
            .context("failed to restore branch commit from recovery bundle")?;
        if !fetch.success() {
            bail!("failed to restore branch commit from recovery bundle");
        }
    }
    let current_branch =
        String::from_utf8(git_output(&destination, &["branch", "--show-current"])?)?
            .trim()
            .to_string();
    if current_branch != manifest.branch {
        let switched = Command::new("git")
            .args(["switch", &manifest.branch])
            .current_dir(&destination)
            .status()
            .context("failed to check out recovered branch")?;
        if !switched.success() {
            bail!("failed to check out recovered branch");
        }
    }
    let restored_head =
        String::from_utf8(git_output(&destination, &["rev-parse", "HEAD^{commit}"])?)?
            .trim()
            .to_string();
    if restored_head != manifest.observed_head {
        bail!("recovered branch does not match archived head");
    }
    for patch in [&manifest.staged_patch, &manifest.unstaged_patch] {
        let path = recovery.directory.join(patch);
        if std::fs::metadata(&path)?.len() == 0 {
            continue;
        }
        let path = path.to_string_lossy().to_string();
        let mut command = Command::new("git");
        command
            .current_dir(&destination)
            .args(["apply", "--binary"]);
        if patch == &manifest.staged_patch {
            command.arg("--cached");
        }
        let status = command
            .arg(&path)
            .status()
            .context("failed to apply recovery patch")?;
        if !status.success() {
            bail!("failed to apply recovery patch {patch}");
        }
        if patch == &manifest.staged_patch {
            let status = Command::new("git")
                .current_dir(&destination)
                .args(["apply", "--binary", &path])
                .status()
                .context("failed to restore staged files in the worktree")?;
            if !status.success() {
                bail!("failed to restore staged files in the worktree");
            }
        }
    }
    for entry in &manifest.untracked {
        let (original, archived) = entry
            .split_once('\t')
            .context("invalid recovery manifest path")?;
        let original = Path::new(original);
        if original.is_absolute()
            || original
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            bail!("unsafe path in recovery manifest");
        }
        copy_untracked(
            &recovery.directory.join(archived),
            &destination.join(original),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn fail_durability_sync(_: &Path) -> Result<()> {
        bail!("injected durability failure")
    }

    #[test]
    fn protected_operation_observation_fails_closed_when_git_dir_is_unreadable() {
        let directory = tempfile::tempdir().unwrap();
        assert!(has_protected_operation(directory.path()).is_err());
    }

    #[test]
    fn protected_operation_classifies_known_and_unclassified_markers() {
        let dir = fixture();
        let git_dir = git2::Repository::open(dir.path())
            .unwrap()
            .path()
            .to_path_buf();

        fs::write(git_dir.join("MERGE_HEAD"), "merge oid\n").unwrap();
        assert_eq!(
            protected_operation(dir.path()).unwrap(),
            ProtectedOperation::Known
        );
        fs::remove_file(git_dir.join("MERGE_HEAD")).unwrap();

        fs::create_dir(git_dir.join("sequencer")).unwrap();
        assert_eq!(
            protected_operation(dir.path()).unwrap(),
            ProtectedOperation::Unknown
        );
        fs::remove_dir(git_dir.join("sequencer")).unwrap();

        fs::write(git_dir.join("BISECT_START"), "head oid\n").unwrap();
        assert_eq!(
            protected_operation(dir.path()).unwrap(),
            ProtectedOperation::Unknown
        );
    }

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Sync test"]);
        fs::write(dir.path().join("tracked.txt"), "base\n").unwrap();
        git(dir.path(), &["add", "tracked.txt"]);
        git(dir.path(), &["commit", "-m", "base"]);
        git(dir.path(), &["switch", "-c", "topic"]);
        fs::write(dir.path().join("tracked.txt"), "branch\n").unwrap();
        git(dir.path(), &["commit", "-am", "topic"]);
        dir
    }

    fn fake_bb(dir: &Path, target: &Path, mode: &str) -> (BbCli, PathBuf, PathBuf) {
        let executable = dir.join("fake-bb.py");
        let log = dir.join("bb-argv.jsonl");
        let marker = dir.join("deleted");
        let script = r##"#!/usr/bin/env python3
import json, os, sys
a=sys.argv[1:]
with open(LOG_PATH, 'a') as f: f.write(json.dumps(a)+'\n')
mode=MODE_VALUE
root=ROOT_PATH
marker=MARKER_PATH
def out(value): print(json.dumps(value))
def fail(): sys.exit(3)
if a == ['status','--json']:
 if mode == 'sleep': import time; time.sleep(5)
 out({'thread': {'environment': {'hostId':'host-1','display':{'id':'current-env'}}}})
elif a[:2] == ['environment','list']:
 if mode == 'unavailable': fail()
 if mode == 'malformed': out({'not':'an array'})
 else:
  offset=a[a.index('--offset')+1]
  rows=[{'path':'/definitely/not/a/worktree'} for _ in range(200)] if mode == 'pagination' and offset == '0' else []
  if (mode != 'pagination' or offset == '200') and mode != 'absent':
   rows.append({'id':('current-env' if mode == 'current' else 'selected-env'),'path':(os.path.join(root,'nested') if mode == 'subdirectory' else (os.path.dirname(root) if mode == 'ancestor' else root)),'projectId':'project-1','environmentProviderId':'provider-1','managed':True,'isWorktree':True,'status':'ready'})
   if mode == 'duplicate': rows.append(dict(rows[-1]))
  out(rows)
elif a[:2] == ['environment','providers']:
 out([] if mode == 'provider-mismatch' else [{'id':'provider-1'}])
elif a[:2] == ['project','show']:
 out({'gitRemoteUrl':('https://elsewhere/repo.git' if mode == 'repository-mismatch' else 'https://example/repo.git')})
elif a[:2] == ['environment','show']:
 state='destroyed' if os.path.exists(marker) and mode != 'timeout' else 'ready'
 out({'path':(os.path.join(root,'nested') if mode == 'subdirectory' else root),'status':state})
elif a[:2] == ['thread','list']:
 out([{'id':('origin-thread' if mode == 'origin' else 'archive-thread'),'status':'completed'}] if mode in ('origin','archive-refusal') else ([{'id':'active-thread','status':'running'}] if mode == 'active' else []))
elif a[:2] == ['thread','show']:
 out({'thread':{'environmentId':'selected-env','status':'completed'}})
elif a[:2] == ['thread','archive']:
 if mode == 'archive-refusal': fail()
 out({'ok':True})
elif a[:2] == ['environment','delete']:
 if mode == 'refusal': fail()
 open(marker,'w').close(); out({'ok':True})
else: fail()
"##
        .replace("LOG_PATH", &serde_json::to_string(&log.to_string_lossy()).unwrap())
        .replace("MARKER_PATH", &serde_json::to_string(&marker.to_string_lossy()).unwrap())
        .replace("ROOT_PATH", &serde_json::to_string(&target.to_string_lossy()).unwrap())
        .replace("MODE_VALUE", &serde_json::to_string(mode).unwrap());
        fs::write(&executable, script).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        (
            BbCli {
                executable,
                origin_thread: None,
                deadline: None,
                current_thread: Arc::default(),
            },
            log,
            marker,
        )
    }

    fn add_remote(repo: &Path) {
        git(
            repo,
            &["remote", "add", "origin", "https://example/repo.git"],
        );
    }

    #[test]
    fn bb_owner_uses_unique_exact_identity_across_pages_from_subdirectory_cwd() {
        let dir = fixture();
        add_remote(dir.path());
        let nested = dir.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let (cli, log, _) = fake_bb(dir.path(), dir.path(), "pagination");
        let owner = resolve_bb_owner(&nested, "origin", dir.path(), Some(&cli))
            .unwrap()
            .unwrap();
        assert_eq!(owner.id, "selected-env");
        assert_eq!(owner.path, canonical(dir.path()).unwrap());
        let calls = fs::read_to_string(log).unwrap();
        assert!(
            calls.contains("200"),
            "pagination was not requested: {calls}"
        );
        assert!(
            calls.contains("\"--host\", \"host-1\""),
            "current host was not selected: {calls}"
        );
        assert!(!calls.contains("current-env\", \"--json"));
    }

    #[test]
    fn bb_owner_resolves_registered_workspace_subdirectory_to_git_root() {
        let dir = fixture();
        add_remote(dir.path());
        let nested = dir.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let (cli, _, marker) = fake_bb(dir.path(), dir.path(), "subdirectory");
        let owner = resolve_bb_owner(dir.path(), "origin", dir.path(), Some(&cli))
            .unwrap()
            .unwrap();
        assert_eq!(owner.id, "selected-env");
        assert_eq!(owner.path, canonical(dir.path()).unwrap());
        retire_bb_environment_with_timeout(&owner, &cli, Duration::from_secs(5)).unwrap();
        assert!(marker.exists());
    }

    #[test]
    fn bb_unknown_or_ambiguous_inventory_never_resolves_as_unmanaged() {
        let dir = fixture();
        add_remote(dir.path());
        for mode in [
            "malformed",
            "unavailable",
            "duplicate",
            "current",
            "provider-mismatch",
            "repository-mismatch",
        ] {
            let (cli, _, _) = fake_bb(dir.path(), dir.path(), mode);
            assert!(
                resolve_bb_owner(dir.path(), "origin", dir.path(), Some(&cli)).is_err(),
                "{mode} inventory must fail closed"
            );
        }
        let (cli, _, _) = fake_bb(dir.path(), dir.path(), "absent");
        assert!(
            resolve_bb_owner(dir.path(), "origin", dir.path(), Some(&cli))
                .unwrap()
                .is_none()
        );
        assert!(
            resolve_bb_owner(dir.path(), "origin", dir.path(), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn bb_owner_reuses_current_thread_but_rereads_registry_and_verdict() {
        let dir = fixture();
        add_remote(dir.path());
        let (cli, log, _) = fake_bb(dir.path(), dir.path(), "success");
        for _ in 0..2 {
            let owner = resolve_bb_owner(dir.path(), "origin", dir.path(), Some(&cli))
                .unwrap()
                .unwrap();
            assert_eq!(owner.id, "selected-env");
        }
        // Clones (each boundary sets its own deadline) share the observation.
        let bounded = cli
            .clone()
            .with_deadline(Instant::now() + Duration::from_secs(5));
        resolve_bb_owner(dir.path(), "origin", dir.path(), Some(&bounded)).unwrap();
        let calls: Vec<Vec<String>> = fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let count = |prefix: &[&str]| {
            calls
                .iter()
                .filter(|args| args.iter().zip(prefix).all(|(a, b)| a == b))
                .count()
        };
        assert_eq!(count(&["status"]), 1, "{calls:?}");
        assert_eq!(count(&["environment", "list"]), 3, "{calls:?}");
        assert_eq!(count(&["environment", "providers"]), 3, "{calls:?}");
        assert_eq!(count(&["project", "show"]), 3, "{calls:?}");
    }

    #[test]
    fn bb_owner_unresolvable_ancestor_registration_still_fails_closed() {
        // The registered path contains the target but is not a Git worktree:
        // the prefilter must still send it through the overlap check. A broken
        // gitfile keeps it unresolvable even when the temp root sits inside
        // another repository.
        let outer = tempfile::tempdir().unwrap();
        fs::write(outer.path().join(".git"), "gitdir: missing\n").unwrap();
        let repo = outer.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        add_remote(&repo);
        let (cli, _, _) = fake_bb(&repo, &repo, "ancestor");
        assert!(resolve_bb_owner(&repo, "origin", &repo, Some(&cli)).is_err());
    }

    #[test]
    fn bb_retirement_selects_exact_environment_and_refuses_active_origin_and_failed_delete() {
        let dir = fixture();
        for mode in ["active", "origin", "archive-refusal", "refusal"] {
            let (mut cli, log, marker) = fake_bb(dir.path(), dir.path(), mode);
            if mode == "origin" {
                cli.origin_thread = Some("origin-thread".to_string());
            }
            let identity = BbEnvironmentIdentity {
                id: "selected-env".to_string(),
                path: canonical(dir.path()).unwrap(),
                registered_path: canonical(dir.path()).unwrap(),
            };
            assert!(
                retire_bb_environment_with_timeout(&identity, &cli, Duration::from_millis(500))
                    .is_err()
            );
            let calls = fs::read_to_string(log).unwrap();
            if mode == "refusal" {
                assert!(calls.contains("delete"), "delete refusal was not exercised");
            } else {
                assert!(
                    !calls.contains("delete"),
                    "unsafe delete in {mode}: {calls}"
                );
            }
            assert!(!marker.exists());
        }

        let (cli, log, marker) = fake_bb(dir.path(), dir.path(), "success");
        let identity = BbEnvironmentIdentity {
            id: "selected-env".to_string(),
            path: canonical(dir.path()).unwrap(),
            registered_path: canonical(dir.path()).unwrap(),
        };
        retire_bb_environment_with_timeout(&identity, &cli, Duration::from_secs(5)).unwrap();
        assert!(marker.exists());
        let calls = fs::read_to_string(log).unwrap();
        assert!(calls.contains("selected-env"));
        assert!(calls.contains("delete"));
        let commands: Vec<Vec<String>> = calls
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            commands
                .iter()
                .any(|args| { args == &["environment", "delete", "selected-env", "--json"] })
        );
        assert!(commands.iter().all(|args| {
            !(args.first().map(String::as_str) == Some("thread")
                && args.get(1).map(String::as_str) == Some("archive"))
                || args.get(2).is_some_and(|id| id == "archive-thread")
        }));

        let (cli, _, _) = fake_bb(dir.path(), dir.path(), "timeout");
        assert!(retire_bb_environment_with_timeout(&identity, &cli, Duration::ZERO).is_err());

        let (cli, _, _) = fake_bb(dir.path(), dir.path(), "sleep");
        let started = Instant::now();
        assert!(
            retire_bb_environment_with_timeout(&identity, &cli, Duration::from_millis(100))
                .is_err()
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "BB child exceeded the shared deadline"
        );

        let changed_path = dir.path().join("changed-path");
        fs::create_dir(&changed_path).unwrap();
        let (cli, _, _) = fake_bb(dir.path(), dir.path(), "success");
        let changed_identity = BbEnvironmentIdentity {
            id: "selected-env".to_string(),
            path: canonical(&changed_path).unwrap(),
            registered_path: canonical(&changed_path).unwrap(),
        };
        assert!(
            retire_bb_environment_with_timeout(&changed_identity, &cli, Duration::ZERO).is_err()
        );
    }

    #[test]
    fn cycle3_bb_large_output_and_descendant_pipes_respect_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("bb.py");
        let marker = dir.path().join("effect");
        let started_marker = dir.path().join("started");
        let cli = BbCli {
            executable: executable.clone(),
            origin_thread: None,
            deadline: None,
            current_thread: Arc::default(),
        };
        fs::write(&executable, "#!/usr/bin/env python3\nimport json,sys\nsys.stderr.write('e'*1000000)\nprint(json.dumps({'data':'x'*1000000}))\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let out = cli
            .run_until(&[], Instant::now() + Duration::from_secs(5))
            .unwrap();
        assert_eq!(out["data"].as_str().unwrap().len(), 1000000);
        for wrapper_exits in [true, false] {
            let script = format!(
                "#!/usr/bin/env python3\nimport os,time\nif os.fork() == 0:\n open({:?},'w').close()\n time.sleep(0.8)\n open({:?},'w').close()\n os._exit(0)\n{}\n",
                started_marker,
                marker,
                if wrapper_exits {
                    "print('{}')"
                } else {
                    "time.sleep(5)"
                }
            );
            fs::write(&executable, script).unwrap();
            let start = Instant::now();
            assert!(
                cli.run_until(&[], start + Duration::from_millis(300))
                    .is_err()
            );
            assert!(start.elapsed() < Duration::from_secs(1));
            assert!(started_marker.exists(), "descendant never started");
            std::thread::sleep(Duration::from_millis(900));
            assert!(!marker.exists(), "descendant survived timeout");
            fs::remove_file(&started_marker).unwrap();
        }
        fs::write(
            &executable,
            format!(
                "#!/usr/bin/env python3\nopen({:?},'w').close()\nprint('{{}}')\n",
                marker
            ),
        )
        .unwrap();
        assert!(cli.run_until(&[], Instant::now()).is_err());
        assert!(!marker.exists(), "expired deadline still spawned a process");
    }

    #[test]
    fn recovery_patches_ignore_user_diff_prefix_config() {
        let dir = fixture();
        let repo = dir.path();
        fs::write(repo.join("tracked.txt"), "staged\n").unwrap();
        git(repo, &["add", "tracked.txt"]);
        fs::write(repo.join("tracked.txt"), "unstaged\n").unwrap();
        let head = String::from_utf8(git_output(repo, &["rev-parse", "HEAD"]).unwrap())
            .unwrap()
            .trim()
            .to_string();
        git(repo, &["config", "diff.noprefix", "true"]);
        let ready =
            save_recovery(&repo.join(".git"), repo, "op-test-1", "topic", &head, &[]).unwrap();

        git(repo, &["config", "--unset", "diff.noprefix"]);
        git(repo, &["config", "diff.mnemonicPrefix", "true"]);

        verify_recovery(&ready, repo).unwrap();
    }

    #[test]
    fn recovery_bundle_restores_branch_staged_unstaged_and_untracked_data() {
        let dir = fixture();
        let repo = dir.path();
        fs::write(repo.join("later.txt"), "local commit after merged head\n").unwrap();
        git(repo, &["add", "later.txt"]);
        git(repo, &["commit", "-m", "later local commit"]);
        fs::write(repo.join("tracked.txt"), "staged\n").unwrap();
        git(repo, &["add", "tracked.txt"]);
        fs::write(repo.join("tracked.txt"), "unstaged\n").unwrap();
        fs::write(repo.join("new.txt"), "untracked\n").unwrap();
        fs::write(repo.join("ignored.txt"), "ignored local configuration\n").unwrap();
        fs::write(repo.join(".git/info/exclude"), "ignored.txt\n").unwrap();
        let head = String::from_utf8(git_output(repo, &["rev-parse", "HEAD"]).unwrap())
            .unwrap()
            .trim()
            .to_string();
        let ready =
            save_recovery(&repo.join(".git"), repo, "op-test-1", "topic", &head, &[]).unwrap();
        verify_recovery(&ready, repo).unwrap();
        let bundle_heads = String::from_utf8(
            git_output(
                repo,
                &[
                    "bundle",
                    "list-heads",
                    ready.directory().join("branch.bundle").to_str().unwrap(),
                ],
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            bundle_heads.contains(&head),
            "later branch tip absent: {bundle_heads}"
        );

        git(repo, &["reset", "--hard", "HEAD"]);
        git(repo, &["clean", "-fd"]);
        git(repo, &["clean", "-fdX"]);
        restore_recovery(&ready, repo).unwrap();

        assert_eq!(
            fs::read_to_string(repo.join("tracked.txt")).unwrap(),
            "unstaged\n"
        );
        assert_eq!(
            fs::read_to_string(repo.join("new.txt")).unwrap(),
            "untracked\n"
        );
        assert_eq!(
            fs::read_to_string(repo.join("ignored.txt")).unwrap(),
            "ignored local configuration\n"
        );
        assert_eq!(
            fs::read_to_string(repo.join("later.txt")).unwrap(),
            "local commit after merged head\n"
        );
        let staged = String::from_utf8(
            git_output(repo, &["diff", "--cached", "--", "tracked.txt"]).unwrap(),
        )
        .unwrap();
        assert!(
            staged.contains("+staged"),
            "staged patch was not restored: {staged}"
        );
        let unstaged =
            String::from_utf8(git_output(repo, &["diff", "--", "tracked.txt"]).unwrap()).unwrap();
        assert!(
            unstaged.contains("+unstaged"),
            "unstaged patch was not restored: {unstaged}"
        );
    }

    fn rev_parse(repo: &Path, rev: &str) -> String {
        String::from_utf8(git_output(repo, &["rev-parse", rev]).unwrap())
            .unwrap()
            .trim()
            .to_string()
    }

    #[test]
    fn trunk_bounded_bundle_restores_deleted_branch_and_workspace_data() {
        let dir = fixture();
        let repo = dir.path();
        fs::write(repo.join("later.txt"), "local commit after merged head\n").unwrap();
        git(repo, &["add", "later.txt"]);
        git(repo, &["commit", "-m", "later local commit"]);
        fs::write(repo.join("tracked.txt"), "staged\n").unwrap();
        git(repo, &["add", "tracked.txt"]);
        fs::write(repo.join("tracked.txt"), "unstaged\n").unwrap();
        fs::write(repo.join("new.txt"), "untracked\n").unwrap();
        let head = rev_parse(repo, "HEAD");
        let trunk_refs = [
            "refs/heads/main".to_string(),
            "refs/remotes/origin/main".to_string(), // unobserved: skipped
        ];
        let ready = save_recovery(
            &repo.join(".git"),
            repo,
            "op-bounded",
            "topic",
            &head,
            &trunk_refs,
        )
        .unwrap();
        let main = rev_parse(repo, "main");
        assert_eq!(
            ready.manifest.bundle_prerequisites,
            vec![BundlePrerequisite {
                trunk_ref: "refs/heads/main".to_string(),
                trunk_oid: main.clone(),
                merge_base: main.clone(),
            }]
        );
        let saved: RecoveryManifest =
            serde_json::from_slice(&fs::read(ready.directory().join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(saved, ready.manifest);
        assert_eq!(saved.version, RECOVERY_MANIFEST_VERSION);
        let bundle = ready.directory().join("branch.bundle");
        let verified = Command::new("git")
            .args(["bundle", "verify", bundle.to_str().unwrap()])
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(verified.status.success());
        assert!(
            String::from_utf8_lossy(&verified.stdout).contains(&main),
            "bundle does not require the trunk merge-base"
        );
        verify_recovery(&ready, repo).unwrap();

        // Lose the branch and all workspace state; only the artifact remains.
        git(repo, &["switch", "--detach", "HEAD"]);
        git(repo, &["branch", "-D", "topic"]);
        git(repo, &["reset", "--hard", "HEAD"]);
        git(repo, &["clean", "-fd"]);
        restore_recovery(&ready, repo).unwrap();

        assert_eq!(rev_parse(repo, "refs/heads/topic"), head);
        assert_eq!(
            String::from_utf8(git_output(repo, &["branch", "--show-current"]).unwrap())
                .unwrap()
                .trim(),
            "topic"
        );
        assert_eq!(
            fs::read_to_string(repo.join("later.txt")).unwrap(),
            "local commit after merged head\n"
        );
        assert_eq!(
            fs::read_to_string(repo.join("tracked.txt")).unwrap(),
            "unstaged\n"
        );
        assert_eq!(
            fs::read_to_string(repo.join("new.txt")).unwrap(),
            "untracked\n"
        );
        let staged = String::from_utf8(
            git_output(repo, &["diff", "--cached", "--", "tracked.txt"]).unwrap(),
        )
        .unwrap();
        assert!(staged.contains("+staged"), "staged patch lost: {staged}");
    }

    #[test]
    fn bundle_falls_back_to_self_contained_when_prerequisites_are_unusable() {
        let dir = fixture();
        let repo = dir.path();
        let head = rev_parse(repo, "HEAD");
        // Tip already contained in trunk: a bounded bundle would be empty.
        git(repo, &["update-ref", "refs/heads/merged-trunk", &head]);
        // Unrelated trunk history: no merge-base to bound by.
        git(repo, &["switch", "--orphan", "unrelated"]);
        git(repo, &["commit", "--allow-empty", "-m", "unrelated root"]);
        git(repo, &["switch", "topic"]);
        for (operation, trunk) in [
            ("op-contained", "refs/heads/merged-trunk"),
            ("op-unrelated", "refs/heads/unrelated"),
            ("op-unobserved", "refs/heads/missing"),
        ] {
            let ready = save_recovery(
                &repo.join(".git"),
                repo,
                operation,
                "topic",
                &head,
                &[trunk.to_string()],
            )
            .unwrap();
            assert!(
                ready.manifest.bundle_prerequisites.is_empty(),
                "{operation}"
            );
            let verified = git_output(
                repo,
                &[
                    "bundle",
                    "verify",
                    ready.directory().join("branch.bundle").to_str().unwrap(),
                ],
            )
            .unwrap();
            assert!(
                String::from_utf8_lossy(&verified).contains("complete history"),
                "{operation}: {}",
                String::from_utf8_lossy(&verified)
            );
            verify_recovery(&ready, repo).unwrap();
        }
    }

    #[test]
    fn guarded_delete_reads_linked_worktree_operation_markers_fresh() {
        let dir = fixture();
        let repo = dir.path();
        git(repo, &["switch", "main"]);
        let head = rev_parse(repo, "topic");
        let linked = dir.path().join("linked");
        git(
            repo,
            &[
                "worktree",
                "add",
                "--detach",
                linked.to_str().unwrap(),
                "topic",
            ],
        );
        let admin = PathBuf::from(
            String::from_utf8(git_output(&linked, &["rev-parse", "--absolute-git-dir"]).unwrap())
                .unwrap()
                .trim(),
        );
        // A detached rebase still reserves its branch.
        fs::create_dir(admin.join("rebase-merge")).unwrap();
        let error = delete_branch_at_expected_oid(repo, "topic", &head).unwrap_err();
        assert!(error.to_string().contains("protected operation"), "{error}");
        assert_eq!(rev_parse(repo, "refs/heads/topic"), head);

        fs::remove_dir(admin.join("rebase-merge")).unwrap();
        delete_branch_at_expected_oid(repo, "topic", &head).unwrap();
        assert!(git_output(repo, &["rev-parse", "--verify", "refs/heads/topic"]).is_err());
    }

    #[test]
    fn version_one_manifest_without_prerequisites_still_parses() {
        let manifest: RecoveryManifest = serde_json::from_str(
            r#"{"version":1,"operation_id":"op","branch":"b","original_path":"/p",
                "observed_head":"h","branch_ref":"r","staged_patch":"s",
                "unstaged_patch":"u","untracked":[]}"#,
        )
        .unwrap();
        assert!(manifest.bundle_prerequisites.is_empty());
    }

    #[test]
    fn changed_target_head_fails_verification_and_keeps_recovery() {
        let dir = fixture();
        let repo = dir.path();
        let head = String::from_utf8(git_output(repo, &["rev-parse", "HEAD"]).unwrap())
            .unwrap()
            .trim()
            .to_string();
        let ready =
            save_recovery(&repo.join(".git"), repo, "op-test-2", "topic", &head, &[]).unwrap();
        git(repo, &["commit", "--allow-empty", "-m", "moved"]);
        assert!(verify_recovery(&ready, repo).is_err());
        assert!(ready.directory().join("manifest.json").exists());
    }

    #[test]
    fn changed_dirty_contents_after_recovery_keep_the_target_and_archive() {
        let dir = fixture();
        let repo = dir.path();
        fs::write(repo.join("new.txt"), "before\n").unwrap();
        let head = String::from_utf8(git_output(repo, &["rev-parse", "HEAD"]).unwrap())
            .unwrap()
            .trim()
            .to_string();
        let ready =
            save_recovery(&repo.join(".git"), repo, "op-test-3", "topic", &head, &[]).unwrap();
        fs::write(repo.join("new.txt"), "changed after archive\n").unwrap();
        assert!(verify_recovery(&ready, repo).is_err());
        assert_eq!(
            fs::read_to_string(repo.join("new.txt")).unwrap(),
            "changed after archive\n"
        );
        assert!(ready.directory().join("manifest.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn changed_untracked_permissions_after_recovery_keep_the_archive() {
        use std::os::unix::fs::PermissionsExt;

        let dir = fixture();
        let repo = dir.path();
        fs::write(repo.join("new.txt"), "same bytes\n").unwrap();
        fs::set_permissions(repo.join("new.txt"), fs::Permissions::from_mode(0o644)).unwrap();
        let head = rev_parse(repo, "HEAD");
        let ready = save_recovery(
            &repo.join(".git"),
            repo,
            "op-mode-change",
            "topic",
            &head,
            &[],
        )
        .unwrap();
        assert!(
            verify_recovery(&ready, repo).is_ok(),
            "unchanged mode is accepted"
        );

        fs::set_permissions(repo.join("new.txt"), fs::Permissions::from_mode(0o755)).unwrap();
        let error = verify_recovery(&ready, repo).unwrap_err();
        assert!(error.to_string().contains("permissions changed"), "{error}");
        assert_eq!(fs::read(repo.join("new.txt")).unwrap(), b"same bytes\n");
        assert!(ready.directory().join("manifest.json").exists());
    }

    #[test]
    fn restore_recovery_handles_empty_patch_combinations_and_rejects_malformed_patch() {
        for (operation, state) in [
            ("restore-clean", "clean"),
            ("restore-staged", "staged"),
            ("restore-unstaged", "unstaged"),
            ("restore-untracked", "untracked"),
        ] {
            let dir = fixture();
            let repo = dir.path();
            match state {
                "staged" => {
                    fs::write(repo.join("tracked.txt"), "staged\n").unwrap();
                    git(repo, &["add", "tracked.txt"]);
                }
                "unstaged" => fs::write(repo.join("tracked.txt"), "unstaged\n").unwrap(),
                "untracked" => fs::write(repo.join("new.txt"), "untracked\n").unwrap(),
                _ => {}
            }
            let head = rev_parse(repo, "HEAD");
            let ready =
                save_recovery(&repo.join(".git"), repo, operation, "topic", &head, &[]).unwrap();
            git(repo, &["reset", "--hard", "HEAD"]);
            git(repo, &["clean", "-fd"]);
            restore_recovery(&ready, repo).unwrap();

            let status =
                String::from_utf8(git_output(repo, &["status", "--porcelain"]).unwrap()).unwrap();
            match state {
                "clean" => assert!(status.is_empty(), "{status:?}"),
                "staged" => {
                    assert!(status.starts_with("M  tracked.txt"), "{status:?}");
                    assert_eq!(
                        fs::read_to_string(repo.join("tracked.txt")).unwrap(),
                        "staged\n"
                    );
                    let patch = String::from_utf8(
                        git_output(repo, &["diff", "--cached", "--", "tracked.txt"]).unwrap(),
                    )
                    .unwrap();
                    assert!(patch.contains("+staged"), "{patch}");
                }
                "unstaged" => {
                    assert!(status.starts_with(" M tracked.txt"), "{status:?}");
                    assert_eq!(
                        fs::read_to_string(repo.join("tracked.txt")).unwrap(),
                        "unstaged\n"
                    );
                    let patch = String::from_utf8(
                        git_output(repo, &["diff", "--", "tracked.txt"]).unwrap(),
                    )
                    .unwrap();
                    assert!(patch.contains("+unstaged"), "{patch}");
                }
                "untracked" => {
                    assert!(status.contains("?? new.txt"), "{status:?}");
                    assert_eq!(
                        fs::read_to_string(repo.join("new.txt")).unwrap(),
                        "untracked\n"
                    );
                }
                _ => unreachable!(),
            }
            assert_eq!(
                rev_parse(repo, "HEAD"),
                head,
                "branch data remains at archived head"
            );
        }

        let dir = fixture();
        let repo = dir.path();
        let head = rev_parse(repo, "HEAD");
        let ready = save_recovery(
            &repo.join(".git"),
            repo,
            "restore-malformed",
            "topic",
            &head,
            &[],
        )
        .unwrap();
        fs::write(ready.directory().join("staged.patch"), b"not a git patch\n").unwrap();
        assert!(
            restore_recovery(&ready, repo).is_err(),
            "nonempty malformed patches fail"
        );
    }

    #[test]
    fn failed_recovery_storage_does_not_remove_the_source_data() {
        let dir = fixture();
        let repo = dir.path();
        fs::write(repo.join("new.txt"), "keep me\n").unwrap();
        let head = String::from_utf8(git_output(repo, &["rev-parse", "HEAD"]).unwrap())
            .unwrap()
            .trim()
            .to_string();
        let error = save_recovery(
            &repo.join(".git/config"),
            repo,
            "op-test-4",
            "topic",
            &head,
            &[],
        );
        assert!(error.is_err());
        assert_eq!(
            fs::read_to_string(repo.join("new.txt")).unwrap(),
            "keep me\n"
        );
        assert_eq!(
            String::from_utf8(git_output(repo, &["rev-parse", "HEAD"]).unwrap())
                .unwrap()
                .trim(),
            head
        );
    }

    #[test]
    fn durability_failure_never_returns_recovery_ready_or_removes_source() {
        let dir = fixture();
        let repo = dir.path();
        fs::write(repo.join("new.txt"), "retain on sync failure\n").unwrap();
        let head = String::from_utf8(git_output(repo, &["rev-parse", "HEAD"]).unwrap())
            .unwrap()
            .trim()
            .to_string();
        let result = save_recovery_with_sync(
            &repo.join(".git"),
            repo,
            "op-durable-fail",
            "topic",
            &head,
            &[],
            fail_durability_sync,
        );
        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(repo.join("new.txt")).unwrap(),
            "retain on sync failure\n"
        );
        assert_eq!(
            String::from_utf8(git_output(repo, &["rev-parse", "HEAD"]).unwrap())
                .unwrap()
                .trim(),
            head
        );
    }
}

/// Host and environment of the BB thread running this sync. A process cannot
/// change either while it runs, so one `bb status` serves the operation.
/// Registry rows and ownership verdicts are never cached.
#[derive(Debug, Clone)]
struct BbCurrentThread {
    host_id: String,
    environment_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct BbCli {
    executable: PathBuf,
    origin_thread: Option<String>,
    deadline: Option<Instant>,
    current_thread: Arc<OnceLock<BbCurrentThread>>,
}

impl BbCli {
    #[cfg(test)]
    pub(super) fn test_cli(executable: PathBuf) -> Self {
        Self {
            executable,
            origin_thread: None,
            deadline: None,
            current_thread: Arc::default(),
        }
    }

    /// Every adapter built in this process shares the current-thread
    /// observation; the environment it reads cannot change mid-process.
    pub(crate) fn from_env() -> Option<Self> {
        static CURRENT_THREAD: OnceLock<Arc<OnceLock<BbCurrentThread>>> = OnceLock::new();
        std::env::var_os("BB_CLI").map(|executable| Self {
            executable: executable.into(),
            origin_thread: std::env::var("BB_THREAD_ID").ok(),
            deadline: None,
            current_thread: CURRENT_THREAD.get_or_init(Arc::default).clone(),
        })
    }

    fn current_thread(&self) -> Result<BbCurrentThread> {
        if let Some(current) = self.current_thread.get() {
            return Ok(current.clone());
        }
        let status = self.run(&["status", "--json"])?;
        let observed = BbCurrentThread {
            host_id: status["thread"]["environment"]["hostId"]
                .as_str()
                .context("BB status does not identify the current host")?
                .to_string(),
            environment_id: status["thread"]["environment"]["display"]["id"]
                .as_str()
                .context("BB status does not identify the current environment")?
                .to_string(),
        };
        Ok(self.current_thread.get_or_init(|| observed).clone())
    }

    pub(crate) fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    fn run(&self, args: &[&str]) -> Result<Value> {
        self.run_until(
            args,
            self.deadline
                .unwrap_or_else(|| Instant::now() + Duration::from_secs(20)),
        )
    }

    fn run_until(&self, args: &[&str], deadline: Instant) -> Result<Value> {
        use std::io::Read;
        if Instant::now() >= deadline {
            bail!("BB command deadline expired before spawn; retry sync");
        }
        let mut command = Command::new(&self.executable);
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Only this invocation and its descendants belong to this process group.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(not(unix))]
        bail!("bounded BB process teardown is unsupported on this platform");
        let mut child = command.spawn().context("failed to invoke BB CLI")?;
        let group = child.id();
        let (tx, rx) = std::sync::mpsc::channel();
        let stdout = child.stdout.take().context("missing BB stdout")?;
        let stderr = child.stderr.take().context("missing BB stderr")?;
        for (is_stdout, mut pipe) in [
            (true, Box::new(stdout) as Box<dyn Read + Send>),
            (false, Box::new(stderr) as Box<dyn Read + Send>),
        ] {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let result = pipe.read_to_end(&mut bytes).map(|_| bytes);
                let _ = tx.send((is_stdout, result));
            });
        }
        drop(tx);
        let mut stdout = None;
        let mut stderr = None;
        let result = (|| {
            loop {
                if Instant::now() >= deadline {
                    bail!(
                        "BB command timed out; environment state is uncertain and must be retried"
                    );
                }
                while let Ok((is_stdout, bytes)) = rx.try_recv() {
                    if is_stdout {
                        stdout = Some(bytes?);
                    } else {
                        stderr = Some(bytes?);
                    }
                }
                if let Some(status) = child.try_wait()?
                    && let (Some(stdout), Some(stderr)) = (&stdout, &stderr)
                {
                    if !status.success() {
                        bail!(
                            "BB command failed: {}",
                            String::from_utf8_lossy(stderr).trim()
                        );
                    }
                    return serde_json::from_slice(stdout)
                        .context("BB command returned invalid JSON");
                }
                std::thread::sleep(
                    Duration::from_millis(10)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        })();
        // A wrapper can exit while descendants still hold pipes or perform
        // effects. End the owned group even when the wrapper already exited.
        #[cfg(unix)]
        {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{group}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = child.wait();
        result
    }
}

fn canonical(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).with_context(|| format!("cannot resolve {}", path.display()))
}

fn git_worktree_root(path: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(path)
        .output()
        .with_context(|| format!("cannot resolve Git worktree for {}", path.display()))?;
    if !output.status.success() {
        bail!("registered BB path is not inside a Git worktree");
    }
    canonical(Path::new(String::from_utf8_lossy(&output.stdout).trim()))
}

fn git_remote_identity(workdir: &Path, remote: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["remote", "get-url", remote])
        .current_dir(workdir)
        .output()
        .context("failed to inspect repository remote")?;
    if !output.status.success() {
        bail!("cannot identify repository remote '{remote}'");
    }
    normalize_remote(String::from_utf8_lossy(&output.stdout).trim())
}

fn normalize_remote(remote: &str) -> Result<String> {
    let remote = remote.trim().trim_end_matches('/').trim_end_matches(".git");
    let normalized = if let Some(ssh) = remote.strip_prefix("git@") {
        ssh.replacen(':', "/", 1)
    } else {
        remote
            .strip_prefix("https://")
            .or_else(|| remote.strip_prefix("http://"))
            .or_else(|| remote.strip_prefix("ssh://"))
            .unwrap_or(remote)
            .to_string()
    };
    if normalized.is_empty() || normalized.contains(' ') {
        bail!("repository remote has no stable identity");
    }
    Ok(normalized.to_ascii_lowercase())
}

/// Resolve a BB owner only from an exact canonical worktree-root match on the
/// current host, with the registered provider and project remote verified.
/// `Ok(None)` means BB is not active in this process or no BB path matches.
pub(crate) fn resolve_bb_owner(
    repo_workdir: &Path,
    remote_name: &str,
    target_path: &Path,
    cli: Option<&BbCli>,
) -> Result<Option<BbEnvironmentIdentity>> {
    let Some(cli) = cli else {
        return Ok(None);
    };
    let BbCurrentThread {
        host_id,
        environment_id: current_env,
    } = cli.current_thread()?;
    let target = git_worktree_root(target_path)?;
    let repo_remote = git_remote_identity(repo_workdir, remote_name)?;

    const PAGE: usize = 200;
    let mut offset = 0usize;
    let mut exact = Vec::new();
    loop {
        let limit = PAGE.to_string();
        let offset_text = offset.to_string();
        let rows = cli.run(&[
            "environment",
            "list",
            "--host",
            &host_id,
            "--limit",
            &limit,
            "--offset",
            &offset_text,
            "--json",
        ])?;
        let rows = rows
            .as_array()
            .context("BB environment list is not an array")?;
        for row in rows {
            let path = row["path"]
                .as_str()
                .context("BB registry entry has no path; ownership is uncertain")?;
            let registered = Path::new(path);
            if !registered.is_absolute()
                || registered
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                bail!(
                    "BB registry path is not an absolute normalized path; ownership is uncertain"
                );
            }
            // A resolvable path disjoint from the target can neither be its
            // root nor overlap it, so it needs no Git lookup.
            if let Ok(resolved) = canonical(registered)
                && !resolved.starts_with(&target)
                && !target.starts_with(&resolved)
            {
                continue;
            }
            match git_worktree_root(registered) {
                Ok(root) if root == target => exact.push(row.clone()),
                Ok(_) => {}
                Err(error) => {
                    // A failed lookup cannot establish absence. Resolve the
                    // nearest existing ancestor only to detect possible overlap,
                    // never to grant ownership or a removal capability.
                    let mut ancestor = registered;
                    let mut suffix = Vec::new();
                    let candidate = loop {
                        if let Ok(root) = canonical(ancestor) {
                            let mut candidate = root;
                            for part in suffix.iter().rev() {
                                candidate.push(part);
                            }
                            break candidate;
                        }
                        suffix.push(
                            ancestor
                                .file_name()
                                .context("BB registry path cannot be resolved")?,
                        );
                        ancestor = ancestor
                            .parent()
                            .context("BB registry path has no resolvable ancestor")?;
                    };
                    if candidate.starts_with(&target) || target.starts_with(&candidate) {
                        bail!(
                            "registered BB workspace may own this worktree but cannot be resolved: {error}"
                        );
                    }
                }
            }
        }
        if rows.len() < PAGE {
            break;
        }
        offset += PAGE;
    }
    if exact.is_empty() {
        return Ok(None);
    }
    if exact.len() != 1 {
        bail!("multiple BB environments claim this exact worktree path");
    }

    let row = &exact[0];
    let id = row["id"]
        .as_str()
        .context("matching BB environment has no ID")?;
    if id == current_env.as_str() {
        bail!("target is the current BB environment");
    }
    let provider = row["environmentProviderId"]
        .as_str()
        .context("matching BB environment has no provider identity")?;
    let providers = cli.run(&[
        "environment",
        "providers",
        "--project",
        row["projectId"].as_str().unwrap_or_default(),
        "--machine",
        &host_id,
        "--json",
    ])?;
    let providers = providers
        .as_array()
        .context("BB provider list is not an array")?;
    if !providers
        .iter()
        .any(|item| item["id"].as_str() == Some(provider))
    {
        bail!("matching BB environment provider is not registered for this host/project");
    }
    if row["managed"].as_bool() != Some(true)
        || row["isWorktree"].as_bool() != Some(true)
        || row["status"].as_str() != Some("ready")
    {
        bail!("matching BB environment is not a ready managed worktree");
    }

    let project_id = row["projectId"]
        .as_str()
        .context("matching BB environment has no project ID")?;
    let project_id_arg = project_id.to_string();
    let project = cli.run(&["project", "show", &project_id_arg, "--json"])?;
    let project_remote = normalize_remote(
        project["gitRemoteUrl"]
            .as_str()
            .context("BB project has no repository remote identity")?,
    )?;
    if project_remote != repo_remote {
        bail!("BB environment project remote does not match this Git repository");
    }

    Ok(Some(BbEnvironmentIdentity {
        id: id.to_string(),
        path: target,
        registered_path: canonical(Path::new(row["path"].as_str().context("BB path missing")?))?,
    }))
}

/// Ask BB to archive only idle history and tear down the exact owned environment.
pub(crate) fn retire_bb_environment(identity: &BbEnvironmentIdentity, cli: &BbCli) -> Result<()> {
    let owned_cli = if cli.deadline.is_some() {
        cli.clone()
    } else {
        cli.clone()
            .with_deadline(Instant::now() + Duration::from_secs(20))
    };
    retire_bb_environment_with_timeout(identity, &owned_cli, Duration::from_secs(20))
}

fn retire_bb_environment_with_timeout(
    identity: &BbEnvironmentIdentity,
    cli: &BbCli,
    timeout: Duration,
) -> Result<()> {
    let deadline = cli.deadline.unwrap_or_else(|| Instant::now() + timeout);
    let status = cli.run_until(&["status", "--json"], deadline)?;
    let current_env = status["thread"]["environment"]["display"]["id"]
        .as_str()
        .context("BB status does not identify the current environment")?;
    if current_env == identity.id {
        bail!("refusing to retire the current environment");
    }

    let id = identity.id.as_str();
    let details = cli.run_until(&["environment", "show", id, "--json"], deadline)?;
    let current_path = details["path"]
        .as_str()
        .context("BB environment details have no path")?;
    if canonical(Path::new(current_path))? != identity.registered_path
        || git_worktree_root(Path::new(current_path))? != identity.path
    {
        bail!("BB environment path changed before teardown; retry sync");
    }

    let threads = cli.run_until(
        &[
            "thread",
            "list",
            "--environment",
            id,
            "--limit",
            "5000",
            "--json",
        ],
        deadline,
    )?;
    let threads = threads
        .as_array()
        .context("BB thread list is not an array")?;
    if threads.len() >= 5000 {
        bail!("BB thread inventory exceeded its safe limit; retry sync");
    }
    for thread in threads {
        let state = thread["status"].as_str().unwrap_or("unknown");
        if !matches!(state, "completed" | "failed" | "cancelled" | "archived") {
            bail!("BB environment has active or nonterminal thread work; retry after it stops");
        }
    }
    let origin_thread = cli.origin_thread.as_deref();
    for thread in threads {
        let thread_id = thread["id"]
            .as_str()
            .context("BB environment thread has no ID")?;
        if origin_thread == Some(thread_id) {
            bail!("refusing to archive the current origin thread");
        }
        let thread_details = cli.run_until(&["thread", "show", thread_id, "--json"], deadline)?;
        let detail = &thread_details["thread"];
        if detail["environmentId"].as_str() != Some(id)
            || !matches!(
                detail["status"].as_str().unwrap_or("unknown"),
                "completed" | "failed" | "cancelled" | "archived"
            )
        {
            bail!("BB thread ownership or status changed; retry sync");
        }
        cli.run_until(&["thread", "archive", thread_id, "--json"], deadline)?;
    }
    cli.run_until(&["environment", "delete", id, "--json"], deadline)?;

    loop {
        let state = cli.run_until(&["environment", "show", id, "--json"], deadline);
        match state {
            Ok(value) if value["status"].as_str() == Some("destroyed") => break,
            Err(_) => {
                bail!("BB teardown result cannot be confirmed; environment retained for retry")
            }
            _ if Instant::now() >= deadline => {
                bail!("BB teardown did not reach destroyed state before timeout; retry sync")
            }
            _ => std::thread::sleep(
                Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now())),
            ),
        }
    }
    Ok(())
}

pub(crate) fn bb_environment_has_active_threads(
    identity: &BbEnvironmentIdentity,
    cli: &BbCli,
) -> Result<bool> {
    let threads = cli.run(&[
        "thread",
        "list",
        "--environment",
        &identity.id,
        "--limit",
        "5000",
        "--json",
    ])?;
    let threads = threads
        .as_array()
        .context("BB thread list is not an array")?;
    if threads.len() >= 5000 {
        bail!("BB thread inventory exceeded its safe limit; retry sync");
    }
    Ok(threads.iter().any(|thread| {
        !matches!(
            thread["status"].as_str().unwrap_or("unknown"),
            "completed" | "failed" | "cancelled" | "archived"
        )
    }))
}
