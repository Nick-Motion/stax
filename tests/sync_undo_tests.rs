use crate::common::{OutputAssertions, TestRepo};

/// Helper: read the parentBranchName from stax metadata for `branch`.
fn read_parent_branch(repo: &TestRepo, branch: &str) -> String {
    let metadata_ref = format!("refs/branch-metadata/{}", branch);
    let out = repo.git(&["show", &metadata_ref]);
    assert!(
        out.status.success(),
        "failed to read metadata for {branch}: {}",
        TestRepo::stderr(&out)
    );
    let json: serde_json::Value =
        serde_json::from_str(&TestRepo::stdout(&out)).expect("invalid metadata JSON");
    json["parentBranchName"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Helper: read the parentBranchRevision from stax metadata for `branch`.
fn read_parent_revision(repo: &TestRepo, branch: &str) -> String {
    let metadata_ref = format!("refs/branch-metadata/{}", branch);
    let out = repo.git(&["show", &metadata_ref]);
    assert!(
        out.status.success(),
        "failed to read metadata for {branch}: {}",
        TestRepo::stderr(&out)
    );
    let json: serde_json::Value =
        serde_json::from_str(&TestRepo::stdout(&out)).expect("invalid metadata JSON");
    json["parentBranchRevision"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Helper: resolve a local ref to its SHA via git rev-parse (returns None if the ref doesn't
/// exist).
fn resolve_ref(repo: &TestRepo, refname: &str) -> Option<String> {
    let out = repo.git(&["rev-parse", "--verify", refname]);
    if out.status.success() {
        Some(TestRepo::stdout(&out).trim().to_string())
    } else {
        None
    }
}

/// Helper: check that a local branch exists.
fn branch_exists(repo: &TestRepo, branch: &str) -> bool {
    resolve_ref(repo, &format!("refs/heads/{branch}")).is_some()
}

/// Helper: count the number of stax receipts in `.git/stax/ops/`.
fn receipt_count(repo: &TestRepo) -> usize {
    let ops_dir = repo.path().join(".git/stax/ops");
    if !ops_dir.exists() {
        return 0;
    }
    std::fs::read_dir(&ops_dir)
        .expect("read ops dir")
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "json")
                .unwrap_or(false)
        })
        .count()
}

// ─── Test 1 ──────────────────────────────────────────────────────────────────

#[test]
fn sync_undo_restores_a_merged_branch_at_its_old_tip() {
    let repo = TestRepo::new_with_remote();

    // Create feature-a on main, push it, and merge it on the remote.
    repo.run_stax(&["bc", "feature-a"]);
    let feature = repo.current_branch();
    repo.create_file("feature-a.txt", "feature content");
    repo.commit("Feature A commit");
    let feature_sha_before = repo.get_commit_sha(&feature);

    repo.git(&["push", "-u", "origin", &feature]);

    repo.run_stax(&["t"]);
    repo.merge_branch_on_remote(&feature);
    repo.git(&["pull", "origin", "main"]);

    // Precondition: branch is already merged.
    let merged_out = repo.git(&["branch", "--merged", "main"]);
    let merged_str = TestRepo::stdout(&merged_out);
    assert!(
        merged_str.contains(&feature),
        "expected {feature} merged into main before sync"
    );

    // Run sync --force, which should delete the merged branch.
    let sync_out = repo.run_stax(&["sync", "--force"]);
    assert!(
        sync_out.status.success(),
        "sync failed: {}",
        TestRepo::stderr(&sync_out)
    );
    assert!(
        !branch_exists(&repo, &feature),
        "branch {feature} should have been deleted by sync"
    );

    // Metadata ref should also be gone.
    assert!(
        resolve_ref(&repo, &format!("refs/branch-metadata/{feature}")).is_none(),
        "metadata ref for {feature} should have been deleted by sync"
    );

    // Now undo.
    let undo_out = repo.run_stax(&["undo", "--yes"]);
    assert!(
        undo_out.status.success(),
        "undo failed: {}",
        TestRepo::stderr(&undo_out)
    );

    // Branch head should be restored to the pre-delete SHA.
    assert!(
        branch_exists(&repo, &feature),
        "undo should have restored the branch {feature}"
    );
    let feature_sha_after_undo = repo.get_commit_sha(&feature);
    assert_eq!(
        feature_sha_before, feature_sha_after_undo,
        "undo should restore the branch to its original tip"
    );

    // Metadata ref should also be restored.
    assert!(
        resolve_ref(&repo, &format!("refs/branch-metadata/{feature}")).is_some(),
        "undo should restore the metadata ref for {feature}"
    );
}

#[test]
fn sync_retires_clean_merged_linked_worktree_and_undo_keeps_directory_retired() {
    let repo = TestRepo::new_with_remote();
    repo.run_stax(&["bc", "linked-cleanup"]);
    let feature = repo.current_branch();
    repo.create_file("linked.txt", "feature\n");
    repo.commit("feature");
    repo.git(&["push", "-u", "origin", &feature]);
    repo.run_stax(&["t"]);

    let parent = tempfile::tempdir().expect("linked worktree parent");
    let linked = parent.path().join("linked");
    let add = repo.git(&[
        "worktree",
        "add",
        linked.to_str().expect("worktree path"),
        &feature,
    ]);
    assert!(
        add.status.success(),
        "worktree add failed: {}",
        TestRepo::stderr(&add)
    );
    repo.merge_branch_on_remote(&feature);
    repo.git(&["pull", "origin", "main"]);

    let sync = repo.run_stax(&["sync", "--force"]);
    assert!(
        sync.status.success(),
        "sync failed: {}",
        TestRepo::stderr(&sync)
    );
    assert!(!linked.exists(), "merged linked worktree must be removed");
    assert!(
        !branch_exists(&repo, &feature),
        "merged branch must be deleted"
    );

    let recovery_root = repo.path().join(".git/stax/recovery");
    let manifest = std::fs::read_dir(&recovery_root)
        .expect("recovery root")
        .flat_map(|operation| std::fs::read_dir(operation.unwrap().path()).unwrap())
        .map(|entry| entry.unwrap().path().join("manifest.json"))
        .find(|path| {
            std::fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .is_some_and(|json| json["branch"] == feature)
        })
        .expect("recovery manifest for merged branch");
    assert!(manifest.parent().unwrap().join("branch.bundle").is_file());

    let undo = repo.run_stax(&["undo", "--yes"]);
    assert!(
        undo.status.success(),
        "undo failed: {}",
        TestRepo::stderr(&undo)
    );
    assert!(
        branch_exists(&repo, &feature),
        "undo should restore branch ref"
    );
    assert!(
        !linked.exists(),
        "undo must not promise to recreate retired worktree"
    );
}

#[test]
fn sync_archives_dirty_linked_worktree_before_removal() {
    let repo = TestRepo::new_with_remote();
    repo.run_stax(&["bc", "linked-dirty-cleanup"]);
    let feature = repo.current_branch();
    repo.create_file("linked.txt", "feature\n");
    repo.commit("feature");
    repo.git(&["push", "-u", "origin", &feature]);
    repo.run_stax(&["t"]);

    let parent = tempfile::tempdir().expect("linked worktree parent");
    let linked = parent.path().join("linked");
    let add = repo.git(&[
        "worktree",
        "add",
        linked.to_str().expect("worktree path"),
        &feature,
    ]);
    assert!(
        add.status.success(),
        "worktree add failed: {}",
        TestRepo::stderr(&add)
    );
    std::fs::write(linked.join("linked.txt"), "staged\n").unwrap();
    let add = std::process::Command::new("git")
        .args(["add", "linked.txt"])
        .current_dir(&linked)
        .output()
        .unwrap();
    assert!(add.status.success());
    std::fs::write(linked.join("linked.txt"), "unstaged\n").unwrap();
    std::fs::write(linked.join("untracked.txt"), "untracked\n").unwrap();
    std::fs::write(linked.join("ignored-local.txt"), "ignored local config\n").unwrap();
    let mut exclude = std::fs::OpenOptions::new()
        .append(true)
        .open(repo.path().join(".git/info/exclude"))
        .unwrap();
    use std::io::Write;
    writeln!(exclude, "ignored-local.txt").unwrap();

    repo.merge_branch_on_remote(&feature);
    repo.git(&["pull", "origin", "main"]);
    let sync = repo.run_stax(&["sync", "--force"]);
    assert!(
        sync.status.success(),
        "sync failed: {}",
        TestRepo::stderr(&sync)
    );
    assert!(
        !linked.exists(),
        "recovered dirty linked worktree must be removed; stdout: {}; stderr: {}",
        TestRepo::stdout(&sync),
        TestRepo::stderr(&sync)
    );

    let recovery_root = repo.path().join(".git/stax/recovery");
    let manifest = std::fs::read_dir(&recovery_root)
        .expect("recovery root")
        .flat_map(|operation| std::fs::read_dir(operation.unwrap().path()).unwrap())
        .map(|entry| entry.unwrap().path().join("manifest.json"))
        .find(|path| {
            std::fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .is_some_and(|json| json["branch"] == feature)
        })
        .expect("recovery manifest for dirty branch");
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    let recovery_dir = manifest.parent().unwrap();
    assert!(recovery_dir.join("branch.bundle").is_file());
    assert!(
        !std::fs::read(recovery_dir.join("staged.patch"))
            .unwrap()
            .is_empty()
    );
    assert!(
        !std::fs::read(recovery_dir.join("unstaged.patch"))
            .unwrap()
            .is_empty()
    );
    assert!(
        json["untracked"]
            .as_array()
            .is_some_and(|files| !files.is_empty())
    );
    let ignored_archive = json["untracked"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|entry| {
            let value = entry.as_str()?;
            let (path, archived) = value.split_once('\t')?;
            (path == "ignored-local.txt").then(|| recovery_dir.join(archived))
        })
        .expect("ignored local file must be listed in recovery manifest");
    assert_eq!(
        std::fs::read_to_string(ignored_archive).unwrap(),
        "ignored local config\n"
    );
}

#[cfg(unix)]
#[test]
fn sync_retains_linked_worktree_when_pre_remove_hook_starts_bisect_marker() {
    use std::os::unix::fs::PermissionsExt;

    let repo = TestRepo::new_with_remote();
    repo.run_stax(&["bc", "linked-hook-operation"]);
    let branch = repo.current_branch();
    repo.create_file("feature.txt", "merged\n");
    repo.commit("feature");
    repo.git(&["push", "-u", "origin", &branch]);
    repo.run_stax(&["t"]);
    let parent = tempfile::tempdir().unwrap();
    let linked = parent.path().join("linked");
    assert!(
        repo.git(&["worktree", "add", linked.to_str().unwrap(), &branch])
            .status
            .success()
    );
    repo.merge_branch_on_remote(&branch);
    repo.git(&["pull", "origin", "main"]);

    let hook = parent.path().join("pre-remove.sh");
    let marker = std::path::PathBuf::from(
        TestRepo::stdout(&repo.git(&[
            "-C",
            linked.to_str().unwrap(),
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "BISECT_START",
        ]))
        .trim(),
    );
    std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&hook, permissions).unwrap();
    std::fs::write(
        repo.path().join("stax.toml"),
        format!(
            "[worktree.hooks]\npre_remove = {}\n",
            serde_json::to_string(hook.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();

    let sync = repo.run_stax(&["sync", "--force"]);
    assert!(
        sync.status.success(),
        "sync failed: {}",
        TestRepo::stderr(&sync)
    );
    assert!(marker.is_file(), "pre_remove hook must create the marker");
    assert!(
        linked.is_dir(),
        "operation started by hook must retain the checkout"
    );
    assert!(
        branch_exists(&repo, &branch),
        "retained checkout branch must remain"
    );
}

#[cfg(unix)]
#[test]
fn sync_pre_remove_permission_change_retains_worktree_and_unchanged_mode_allows_removal() {
    use std::os::unix::fs::PermissionsExt;

    for (name, change_mode) in [("chmod-change", true), ("chmod-control", false)] {
        let repo = TestRepo::new_with_remote();
        repo.run_stax(&["bc", name]);
        let branch = repo.current_branch();
        repo.create_file("feature.txt", "merged\n");
        repo.commit("feature");
        repo.git(&["push", "-u", "origin", &branch]);
        repo.run_stax(&["t"]);
        let parent = tempfile::tempdir().unwrap();
        let linked = parent.path().join("linked");
        assert!(
            repo.git(&["worktree", "add", linked.to_str().unwrap(), &branch])
                .status
                .success()
        );
        let script = linked.join("recoverable.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
        repo.merge_branch_on_remote(&branch);
        repo.git(&["pull", "origin", "main"]);

        let hook = parent.path().join("pre-remove.sh");
        let hook_body = if change_mode {
            "#!/bin/sh\nchmod 755 recoverable.sh\n"
        } else {
            "#!/bin/sh\nexit 0\n"
        };
        std::fs::write(&hook, hook_body).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            repo.path().join("stax.toml"),
            format!(
                "[worktree.hooks]\npre_remove = {}\n",
                serde_json::to_string(hook.to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();

        let sync = repo.run_stax(&["sync", "--force"]);
        assert!(sync.status.success(), "{name}: {}", TestRepo::stderr(&sync));
        if change_mode {
            assert!(linked.is_dir(), "permission-only edit must retain worktree");
            assert!(
                branch_exists(&repo, &branch),
                "permission-only edit must retain branch"
            );
            assert_eq!(
                std::fs::metadata(&script).unwrap().permissions().mode() & 0o777,
                0o755
            );
        } else {
            assert!(!linked.exists(), "unchanged mode should permit removal");
            assert!(
                !branch_exists(&repo, &branch),
                "unchanged mode should permit branch removal"
            );
        }
    }
}

#[test]
fn sync_retains_linked_worktree_with_attached_no_checkout_bisect() {
    let repo = TestRepo::new_with_remote();
    repo.run_stax(&["bc", "linked-bisect-operation"]);
    let branch = repo.current_branch();
    repo.create_file("feature.txt", "merged\n");
    repo.commit("feature");
    repo.git(&["push", "-u", "origin", &branch]);
    repo.run_stax(&["t"]);
    let parent = tempfile::tempdir().unwrap();
    let linked = parent.path().join("linked");
    assert!(
        repo.git(&["worktree", "add", linked.to_str().unwrap(), &branch])
            .status
            .success()
    );
    repo.merge_branch_on_remote(&branch);
    repo.git(&["pull", "origin", "main"]);

    let good_revision = format!("{branch}^");
    let bisect = repo.git(&[
        "-C",
        linked.to_str().unwrap(),
        "bisect",
        "start",
        "--no-checkout",
        &branch,
        &good_revision,
    ]);
    assert!(
        bisect.status.success(),
        "bisect start failed: {}",
        TestRepo::stderr(&bisect)
    );
    assert!(
        std::fs::read_dir(repo.path().join(".git/worktrees"))
            .unwrap()
            .any(|entry| {
                let admin = entry.unwrap().path();
                admin.join("BISECT_START").exists()
            }),
        "git bisect start must create worktree operation state"
    );

    let sync = repo.run_stax(&["sync", "--force"]);
    assert!(
        sync.status.success(),
        "sync failed: {}",
        TestRepo::stderr(&sync)
    );
    assert!(linked.is_dir(), "bisect-active checkout must be retained");
    assert!(
        branch_exists(&repo, &branch),
        "bisect-active branch must remain"
    );
}

#[test]
fn sync_retains_upstream_gone_linked_pr_with_ancestor_tip_when_pr_state_is_unknown() {
    let repo = TestRepo::new_with_remote();
    repo.run_stax(&["bc", "gone-linked-pr"]);
    let branch = repo.current_branch();
    repo.create_file("linked-pr.txt", "integrated work\n");
    repo.commit("linked PR work");
    repo.git(&["push", "-u", "origin", &branch]);
    let tip = repo.get_commit_sha(&branch);

    let parent_revision = TestRepo::stdout(&repo.git(&["rev-parse", "main"]))
        .trim()
        .to_string();
    let metadata = serde_json::json!({
        "parentBranchName": "main",
        "parentBranchRevision": parent_revision,
        "prInfo": { "number": 991, "state": "OPEN", "isDraft": false }
    })
    .to_string();
    let mut hash = std::process::Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(repo.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    hash.stdin
        .as_mut()
        .unwrap()
        .write_all(metadata.as_bytes())
        .unwrap();
    let hash = hash.wait_with_output().unwrap();
    assert!(hash.status.success());
    let metadata_oid = TestRepo::stdout(&hash).trim().to_string();
    repo.git(&[
        "update-ref",
        &format!("refs/branch-metadata/{branch}"),
        &metadata_oid,
    ]);

    repo.git(&["checkout", "main"]);
    repo.git(&["merge", "--ff-only", &branch]);
    repo.git(&["push", "origin", "main"]);
    repo.git(&["push", "origin", "--delete", &branch]);

    let sync = repo.run_stax(&["sync", "--no-delete", "--force", "--delete-upstream-gone"]);
    assert!(
        sync.status.success(),
        "sync failed: {}",
        TestRepo::stderr(&sync)
    );
    assert!(
        repo.list_branches().contains(&branch),
        "linked PR branch at {tip} must remain when remote disappears and fresh PR state is unavailable"
    );
    assert_eq!(repo.get_commit_sha(&branch), tip);
}

#[test]
fn sync_does_not_retire_patch_equivalent_non_ancestor_branch() {
    let repo = TestRepo::new_with_remote();
    repo.run_stax(&["bc", "patch-equivalent"]);
    let branch = repo.current_branch();
    repo.create_file("patch-equivalent.txt", "same patch\n");
    repo.commit("patch-equivalent change");
    repo.git(&["push", "-u", "origin", &branch]);
    let tip = repo.get_commit_sha(&branch);

    repo.git(&["checkout", "main"]);
    // A different message guarantees a different object even with identical
    // Docker author/committer identities and second-resolution timestamps.
    repo.git(&["cherry-pick", "--no-commit", &tip])
        .assert_success();
    repo.git(&[
        "commit",
        "-m",
        "Independent integration of equivalent patch",
    ])
    .assert_success();
    assert_ne!(repo.get_commit_sha("main"), tip);
    assert_eq!(
        repo.git(&["merge-base", "--is-ancestor", &tip, "main"])
            .status
            .code(),
        Some(1)
    );
    let metadata_ref = format!("refs/branch-metadata/{branch}");
    let metadata_before = TestRepo::stdout(&repo.git(&["show", &metadata_ref]));
    repo.git(&["push", "origin", "main"]);
    repo.git(&["push", "origin", "--delete", &branch]);

    let sync = repo.run_stax(&["sync", "--force"]);
    assert!(
        sync.status.success(),
        "sync failed: {}",
        TestRepo::stderr(&sync)
    );
    assert!(
        repo.list_branches().contains(&branch),
        "patch equivalence without ancestry or merged-PR proof must remain informational"
    );
    assert_eq!(repo.get_commit_sha(&branch), tip);
    assert_eq!(
        TestRepo::stdout(&repo.git(&["show", &metadata_ref])),
        metadata_before
    );
    assert!(!repo.path().join(".git/stax/recovery").exists());
}

#[test]
fn sync_keeps_locked_and_operation_protected_worktrees_unchanged() {
    for protection in ["locked", "rebase"] {
        let repo = TestRepo::new_with_remote();
        repo.run_stax(&["bc", &format!("protected-{protection}")]);
        let branch = repo.current_branch();
        repo.create_file("protected.txt", "merged content\n");
        repo.commit("protected branch commit");
        repo.git(&["push", "-u", "origin", &branch]);
        repo.git(&["checkout", "main"]);
        let parent = tempfile::tempdir().expect("linked worktree parent");
        let linked = parent.path().join("protected");
        let add = repo.git(&["worktree", "add", linked.to_str().unwrap(), &branch]);
        assert!(
            add.status.success(),
            "worktree add failed: {}",
            TestRepo::stderr(&add)
        );
        repo.git(&["merge", "--ff-only", &branch]);
        repo.git(&["push", "origin", "main"]);

        if protection == "locked" {
            let lock = repo.git(&["worktree", "lock", linked.to_str().unwrap()]);
            assert!(
                lock.status.success(),
                "worktree lock failed: {}",
                TestRepo::stderr(&lock)
            );
        } else {
            let state = std::process::Command::new("git")
                .args(["rev-parse", "--git-path", "rebase-merge"])
                .current_dir(&linked)
                .output()
                .unwrap();
            assert!(state.status.success());
            std::fs::create_dir_all(TestRepo::stdout(&state).trim()).unwrap();
        }
        let head_before = repo.git(&["-C", linked.to_str().unwrap(), "rev-parse", "HEAD"]);
        let branch_before = repo.git(&["-C", linked.to_str().unwrap(), "branch", "--show-current"]);
        let metadata_before = repo.git(&["show", &format!("refs/branch-metadata/{branch}")]);

        let refs_before = TestRepo::stdout(&repo.git(&["show-ref"]));
        let plan = repo.run_stax(&["sync", "--dry-run", "--json", "--force"]);
        assert!(plan.status.success(), "{}", TestRepo::stderr(&plan));
        let json: serde_json::Value = serde_json::from_slice(&plan.stdout).unwrap();
        let candidate = json["merged_candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == branch)
            .unwrap();
        assert_eq!(
            candidate["disposition"], "would_keep_worktree",
            "{protection}: {json}"
        );
        assert_eq!(TestRepo::stdout(&repo.git(&["show-ref"])), refs_before);
        assert!(!repo.path().join(".git/stax/recovery").exists());

        let sync = repo.run_stax(&["sync", "--force"]);
        assert!(
            sync.status.success(),
            "{protection} sync failed: {}",
            TestRepo::stderr(&sync)
        );
        assert!(linked.exists(), "{protection} worktree must remain");
        assert!(
            repo.list_branches().contains(&branch),
            "{protection} branch must remain"
        );
        assert_eq!(
            TestRepo::stdout(&repo.git(&["-C", linked.to_str().unwrap(), "rev-parse", "HEAD"])),
            TestRepo::stdout(&head_before)
        );
        assert_eq!(
            TestRepo::stdout(&repo.git(&[
                "-C",
                linked.to_str().unwrap(),
                "branch",
                "--show-current"
            ])),
            TestRepo::stdout(&branch_before)
        );
        assert_eq!(
            TestRepo::stdout(&repo.git(&["show", &format!("refs/branch-metadata/{branch}")])),
            TestRepo::stdout(&metadata_before)
        );
    }
}

// ─── Test 2 ──────────────────────────────────────────────────────────────────

#[test]
fn sync_undo_restores_reparented_child_metadata() {
    let repo = TestRepo::new_with_remote();

    // Build stack: main → a → b
    repo.run_stax(&["bc", "branch-a"]);
    let branch_a = repo.current_branch();
    repo.create_file("a.txt", "a");
    repo.commit("Branch A commit");
    repo.git(&["push", "-u", "origin", &branch_a]);

    repo.run_stax(&["bc", "branch-b"]);
    let branch_b = repo.current_branch();
    repo.create_file("b.txt", "b");
    repo.commit("Branch B commit");

    // Merge A on the remote (so sync sees it as merged).
    repo.run_stax(&["t"]);
    repo.merge_branch_on_remote(&branch_a);
    repo.git(&["pull", "origin", "main"]);

    let merged_out = repo.git(&["branch", "--merged", "main"]);
    let merged_str = TestRepo::stdout(&merged_out);
    assert!(
        merged_str.contains(&branch_a),
        "expected {branch_a} merged into main before sync"
    );

    // Verify b's parent is a before sync.
    let b_parent_before = read_parent_branch(&repo, &branch_b);
    assert_eq!(b_parent_before, branch_a, "b should point to a before sync");

    // sync --force: deletes a and reparents b → main.
    let sync_out = repo.run_stax(&["sync", "--force"]);
    assert!(
        sync_out.status.success(),
        "sync failed: {}",
        TestRepo::stderr(&sync_out)
    );
    assert!(
        !branch_exists(&repo, &branch_a),
        "branch_a should have been deleted"
    );

    // After sync, b's parent should have moved away from a.
    let b_parent_after_sync = read_parent_branch(&repo, &branch_b);
    assert_ne!(
        b_parent_after_sync, branch_a,
        "after sync, b should no longer point to the deleted a"
    );

    // Undo — should restore a AND restore b's parent → a.
    let undo_out = repo.run_stax(&["undo", "--yes"]);
    assert!(
        undo_out.status.success(),
        "undo failed: {}",
        TestRepo::stderr(&undo_out)
    );

    assert!(
        branch_exists(&repo, &branch_a),
        "undo should restore branch_a"
    );

    let b_parent_after_undo = read_parent_branch(&repo, &branch_b);
    assert_eq!(
        b_parent_after_undo, branch_a,
        "undo should restore b's parent metadata back to a"
    );
}

// ─── Test 3 ──────────────────────────────────────────────────────────────────

#[test]
fn sync_that_changes_nothing_leaves_the_previous_receipt_undoable() {
    let repo = TestRepo::new_with_remote();

    // Create a branch, commit, and restack to produce a restack receipt.
    repo.run_stax(&["bc", "feature-noop"]);
    let feature = repo.current_branch();
    repo.create_file("feature.txt", "feature");
    repo.commit("Feature commit");

    repo.run_stax(&["t"]);
    repo.create_file("main-update.txt", "main update");
    repo.commit("Main update");

    repo.run_stax(&["checkout", &feature]);
    let restack_out = repo.run_stax(&["restack", "--quiet"]);
    assert!(
        restack_out.status.success(),
        "restack failed: {}",
        TestRepo::stderr(&restack_out)
    );

    let receipts_after_restack = receipt_count(&repo);
    assert!(
        receipts_after_restack >= 1,
        "expected at least one receipt after restack"
    );

    let feature_sha_after_restack = repo.get_commit_sha(&feature);

    // Now run sync with no remote changes — nothing should be snapshotted.
    // (No remote, so fetch either uses a dummy or is skipped for a no-change run.)
    // We just run sync with --force to skip interactive prompts; since nothing changed
    // on the remote, the sync transaction is a no-op and leaves no new receipt.
    let sync_out = repo.run_stax(&["sync", "--force"]);
    assert!(
        sync_out.status.success(),
        "sync failed: {}",
        TestRepo::stderr(&sync_out)
    );

    let receipts_after_noop_sync = receipt_count(&repo);
    assert_eq!(
        receipts_after_restack, receipts_after_noop_sync,
        "a no-op sync must not create a new receipt (lazy snapshot guard)"
    );

    // Undo should still undo the RESTACK (the previous receipt), not the sync.
    let undo_out = repo.run_stax(&["undo", "--yes"]);
    assert!(
        undo_out.status.success(),
        "undo after no-op sync failed: {}",
        TestRepo::stderr(&undo_out)
    );

    // After undoing the restack, feature should be back at its pre-restack SHA.
    // (The feature commit SHA changed during restack so post-undo != post-restack.)
    let feature_sha_after_undo = repo.get_commit_sha(&feature);
    assert_ne!(
        feature_sha_after_restack, feature_sha_after_undo,
        "undo should have reversed the restack, changing the feature branch SHA"
    );
}

// ─── Test 4 ──────────────────────────────────────────────────────────────────

#[test]
fn sync_undo_restores_trunk_after_fast_forward() {
    let repo = TestRepo::new_with_remote();

    // Capture trunk SHA before any remote changes.
    let trunk_sha_before = repo.get_commit_sha("main");

    // Create feature and push it (not on trunk).
    repo.run_stax(&["bc", "feature-ff"]);
    let feature = repo.current_branch();
    repo.create_file("feature.txt", "feature");
    repo.commit("Feature commit");
    repo.git(&["push", "-u", "origin", &feature]);

    // Simulate a remote commit on trunk (so sync has to fast-forward trunk).
    repo.simulate_remote_commit("remote-main.txt", "from remote", "Remote main commit");

    // Checkout feature so we're NOT on trunk — this exercises the non-checkout
    // path (bare git update-ref fast-forward), which is where plan_trunk_move is
    // called in that code branch.
    let sync_out = repo.run_stax(&["sync", "--force"]);
    assert!(
        sync_out.status.success(),
        "sync failed: {}",
        TestRepo::stderr(&sync_out)
    );

    let trunk_sha_after_sync = repo.get_commit_sha("main");
    assert_ne!(
        trunk_sha_before, trunk_sha_after_sync,
        "trunk should have moved during sync"
    );

    // Undo — should restore trunk to its pre-sync SHA.
    let undo_out = repo.run_stax(&["undo", "--yes"]);
    assert!(
        undo_out.status.success(),
        "undo failed: {}",
        TestRepo::stderr(&undo_out)
    );

    let trunk_sha_after_undo = repo.get_commit_sha("main");
    assert_eq!(
        trunk_sha_before, trunk_sha_after_undo,
        "undo should restore trunk to its pre-sync SHA"
    );
}

// ─── Test 5 ──────────────────────────────────────────────────────────────────

#[test]
fn sync_undo_then_redo_replays_deletions_and_trunk() {
    let repo = TestRepo::new_with_remote();

    // Build stack: main → feature-del (to be deleted by sync).
    repo.run_stax(&["bc", "feature-del"]);
    let feature = repo.current_branch();
    repo.create_file("del.txt", "del");
    repo.commit("Feature del commit");
    repo.git(&["push", "-u", "origin", &feature]);

    // Merge feature on remote so sync will delete it.
    repo.run_stax(&["t"]);
    repo.merge_branch_on_remote(&feature);

    // Pull so local main sees the merged commit first.
    repo.git(&["pull", "origin", "main"]);

    // Simulate a remote commit on trunk AFTER the pull so sync has something to fast-forward.
    repo.simulate_remote_commit("remote-trunk.txt", "from remote", "Remote trunk commit");

    let trunk_sha_before_sync = repo.get_commit_sha("main");
    let feature_sha = resolve_ref(&repo, &format!("refs/heads/{feature}"))
        .expect("feature branch should exist before sync");

    // Keep trunk un-checked-out, while leaving the merged deletion target
    // unoccupied: sync now protects the caller's primary checkout as well.
    // A separate unmerged caller preserves this test's non-checked-out trunk
    // path and all branch deletion / undo / redo assertions.
    repo.run_stax(&["bc", "undo-redo-caller"]);
    repo.create_file("caller.txt", "ongoing caller work");
    repo.commit("caller work must survive sync");
    let caller = repo.current_branch();
    let caller_head = repo.get_commit_sha("HEAD");

    let sync_out = repo.run_stax(&["sync", "--force"]);
    assert!(
        sync_out.status.success(),
        "sync failed: {}",
        TestRepo::stderr(&sync_out)
    );

    assert_eq!(repo.current_branch(), caller);
    assert_eq!(repo.get_commit_sha("HEAD"), caller_head);

    // Verify sync deleted feature and advanced trunk.
    assert!(
        !branch_exists(&repo, &feature),
        "feature should have been deleted by sync"
    );
    let trunk_sha_after_sync = repo.get_commit_sha("main");
    assert_ne!(
        trunk_sha_before_sync, trunk_sha_after_sync,
        "trunk should have moved during sync"
    );

    // Undo — restore feature and roll back trunk.
    let undo_out = repo.run_stax(&["undo", "--yes", "--no-push"]);
    assert!(
        undo_out.status.success(),
        "undo failed: {}",
        TestRepo::stderr(&undo_out)
    );

    assert!(
        branch_exists(&repo, &feature),
        "undo should restore the deleted feature branch"
    );
    assert_eq!(
        resolve_ref(&repo, &format!("refs/heads/{feature}")).as_deref(),
        Some(feature_sha.as_str()),
        "undo should restore feature to its original SHA"
    );
    let trunk_sha_after_undo = repo.get_commit_sha("main");
    assert_eq!(
        trunk_sha_before_sync, trunk_sha_after_undo,
        "undo should roll trunk back to its pre-sync SHA"
    );

    // Redo — delete feature again and re-advance trunk.
    let redo_out = repo.run_stax(&["redo", "--yes", "--no-push"]);
    assert!(
        redo_out.status.success(),
        "redo failed: {}",
        TestRepo::stderr(&redo_out)
    );

    assert!(
        !branch_exists(&repo, &feature),
        "redo should delete feature again"
    );
    let trunk_sha_after_redo = repo.get_commit_sha("main");
    assert_eq!(
        trunk_sha_after_sync, trunk_sha_after_redo,
        "redo should re-advance trunk to the post-sync SHA"
    );
}

// ─── Test 6 ──────────────────────────────────────────────────────────────────

#[test]
fn sync_restack_undo_rolls_back_trunk_fast_forward() {
    // Regression test: sync --restack used to leave a duplicate `main` entry in
    // the receipt (plan_trunk_move added it with the pre-ff OID; plan_branches
    // added it again with the post-ff OID).  On undo, apply_transaction iterated
    // both entries, first resetting main to pre-ff, then forward to post-ff again
    // — leaving trunk un-rolled-back.  The dedup fix in add_local_ref keeps only
    // the earliest snapshot, so undo now correctly restores trunk.
    let repo = TestRepo::new_with_remote();

    // Create a feature branch off main.
    repo.run_stax(&["bc", "feature-restack"]);
    let feature = repo.current_branch();
    repo.create_file("feature.txt", "feature");
    repo.commit("Feature commit");

    // Return to main so sync can fast-forward it in the non-checkout path.
    repo.run_stax(&["t"]);

    // Capture trunk SHA before any sync activity.
    let trunk_sha_before_sync = repo.get_commit_sha("main");

    // Advance origin/main so sync has a fast-forward to perform.
    repo.simulate_remote_commit(
        "remote-sync-restack.txt",
        "remote",
        "Remote commit for sync",
    );

    // Run sync with restack so both plan_trunk_move AND plan_branches(restack_scope) fire.
    let sync_out = repo.run_stax(&["sync", "--force", "--restack"]);
    assert!(
        sync_out.status.success(),
        "sync --force --restack failed: {}",
        TestRepo::stderr(&sync_out)
    );

    let trunk_sha_after_sync = repo.get_commit_sha("main");
    assert_ne!(
        trunk_sha_before_sync, trunk_sha_after_sync,
        "trunk should have moved during sync"
    );

    // Undo — trunk must be rolled all the way back to the pre-sync SHA.
    let undo_out = repo.run_stax(&["undo", "--yes", "--no-push"]);
    assert!(
        undo_out.status.success(),
        "undo failed: {}",
        TestRepo::stderr(&undo_out)
    );

    let trunk_sha_after_undo = repo.get_commit_sha("main");
    assert_eq!(
        trunk_sha_before_sync, trunk_sha_after_undo,
        "undo should restore trunk to its pre-sync SHA (duplicate receipt entry bug)"
    );

    // Feature branch should still exist (nothing was deleted by this sync).
    assert!(
        branch_exists(&repo, &feature),
        "feature branch should still exist after undo"
    );
}

// ─── Test 7 ──────────────────────────────────────────────────────────────────

#[test]
fn sync_restack_conflict_undo_restores_branch_metadata() {
    // Regression test: sync's restack phase only snapshotted branch heads, not
    // branch-metadata refs. When a later branch in the restack scope hit a
    // conflict and sync stopped, undo restored earlier branches' commits but
    // left their rewritten parentBranchRevision in place.
    let repo = TestRepo::new_with_remote();

    // Build stack: main → branch-a → branch-b.
    repo.run_stax(&["bc", "branch-a"]);
    let branch_a = repo.current_branch();
    repo.create_file("a.txt", "a");
    repo.commit("Branch A commit");

    repo.run_stax(&["bc", "branch-b"]);
    repo.create_file("shared.txt", "from branch b");
    repo.commit("Branch B commit");

    let main_sha_before = repo.get_commit_sha("main");
    let a_sha_before = repo.get_commit_sha(&branch_a);
    let a_parent_rev_before = read_parent_revision(&repo, &branch_a);
    assert_eq!(
        a_parent_rev_before, main_sha_before,
        "branch-a's recorded parent revision should be main's sha before sync"
    );

    // Advance remote main with a conflicting change to shared.txt, so branch-b's
    // rebase conflicts while branch-a's does not.
    repo.simulate_remote_commit(
        "shared.txt",
        "from remote main",
        "Remote conflicting commit",
    );

    let sync_out = repo.run_stax(&["sync", "--force", "--restack"]);
    assert!(
        !sync_out.status.success(),
        "sync should stop on the branch-b conflict"
    );

    assert_ne!(
        read_parent_revision(&repo, &branch_a),
        a_parent_rev_before,
        "sync should have rewritten branch-a's metadata before hitting the conflict"
    );

    repo.abort_rebase();

    let undo_out = repo.run_stax(&["undo", "--yes", "--no-push"]);
    assert!(
        undo_out.status.success(),
        "undo failed: {}",
        TestRepo::stderr(&undo_out)
    );

    assert_eq!(
        repo.get_commit_sha(&branch_a),
        a_sha_before,
        "undo should restore branch-a to its pre-sync commit"
    );
    assert_eq!(
        read_parent_revision(&repo, &branch_a),
        a_parent_rev_before,
        "undo should restore branch-a's metadata to its pre-sync parent revision"
    );
    assert_eq!(
        repo.get_commit_sha("main"),
        main_sha_before,
        "undo should restore main to its pre-sync sha"
    );

    // A *failed* sync's receipt cannot be redone (can_redo() requires
    // OpStatus::Success) — there is no coherent "completed" state to replay
    // back to. Confirm that's still rejected cleanly rather than silently
    // reapplying a half-finished restack.
    let redo_out = repo.run_stax(&["redo", "--yes", "--no-push"]);
    assert!(
        !redo_out.status.success(),
        "redo of a failed sync's receipt should be rejected, not silently reapplied"
    );
    assert_eq!(
        repo.get_commit_sha(&branch_a),
        a_sha_before,
        "rejected redo must leave branch-a exactly where undo left it"
    );
}

// ─── Test 8 ──────────────────────────────────────────────────────────────────

#[test]
fn sync_restack_success_undo_then_redo_round_trips_branch_metadata() {
    // Companion to sync_restack_conflict_undo_restores_branch_metadata: that test
    // covers the undo direction after a *failed* restack. This test covers a
    // *successful* restack, where the receipt's after-OIDs for the metadata ref
    // must also be recorded so redo can replay the branch head and its rewritten
    // parentBranchRevision back together.
    let repo = TestRepo::new_with_remote();

    repo.run_stax(&["bc", "branch-a"]);
    let branch_a = repo.current_branch();
    repo.create_file("a.txt", "a");
    repo.commit("Branch A commit");

    let main_sha_before = repo.get_commit_sha("main");
    let a_sha_before = repo.get_commit_sha(&branch_a);
    let a_parent_rev_before = read_parent_revision(&repo, &branch_a);
    assert_eq!(
        a_parent_rev_before, main_sha_before,
        "branch-a's recorded parent revision should be main's sha before sync"
    );

    // Advance remote main with an unrelated file so branch-a's rebase succeeds
    // cleanly (no conflict).
    repo.simulate_remote_commit("remote.txt", "remote", "Remote commit for restack");

    let sync_out = repo.run_stax(&["sync", "--force", "--restack"]);
    assert!(
        sync_out.status.success(),
        "sync --force --restack failed: {}",
        TestRepo::stderr(&sync_out)
    );

    let main_sha_after_sync = repo.get_commit_sha("main");
    let a_sha_after_sync = repo.get_commit_sha(&branch_a);
    let a_parent_rev_after_sync = read_parent_revision(&repo, &branch_a);
    assert_ne!(
        a_sha_after_sync, a_sha_before,
        "sync should have rebased branch-a onto the new main"
    );
    assert_eq!(
        a_parent_rev_after_sync, main_sha_after_sync,
        "sync should have rewritten branch-a's parent revision to the new main sha"
    );

    let undo_out = repo.run_stax(&["undo", "--yes", "--no-push"]);
    assert!(
        undo_out.status.success(),
        "undo failed: {}",
        TestRepo::stderr(&undo_out)
    );
    assert_eq!(
        repo.get_commit_sha(&branch_a),
        a_sha_before,
        "undo should restore branch-a to its pre-sync commit"
    );
    assert_eq!(
        read_parent_revision(&repo, &branch_a),
        a_parent_rev_before,
        "undo should restore branch-a's metadata to its pre-sync parent revision"
    );

    let redo_out = repo.run_stax(&["redo", "--yes", "--no-push"]);
    assert!(
        redo_out.status.success(),
        "redo failed: {}",
        TestRepo::stderr(&redo_out)
    );
    assert_eq!(
        repo.get_commit_sha(&branch_a),
        a_sha_after_sync,
        "redo should re-apply branch-a's restacked commit"
    );
    assert_eq!(
        read_parent_revision(&repo, &branch_a),
        a_parent_rev_after_sync,
        "redo should re-apply branch-a's rewritten parent revision alongside its commit"
    );
    assert_eq!(
        repo.get_commit_sha("main"),
        main_sha_after_sync,
        "redo should re-advance trunk to the post-sync sha"
    );
}

// ─── Test 9 ──────────────────────────────────────────────────────────────────

#[test]
fn sync_restack_conflict_undo_restores_metadata_for_every_successful_branch_in_a_three_branch_stack()
 {
    // Extends sync_restack_conflict_undo_restores_branch_metadata to a 3-branch
    // stack (main -> branch-a -> branch-b -> branch-c), where the two bottom
    // branches restack successfully before the top one conflicts. Undo must
    // restore BOTH branch-a's and branch-b's commit + metadata, not just the
    // bottommost one.
    let repo = TestRepo::new_with_remote();

    repo.run_stax(&["bc", "branch-a"]);
    let branch_a = repo.current_branch();
    repo.create_file("a.txt", "a");
    repo.commit("Branch A commit");

    repo.run_stax(&["bc", "branch-b"]);
    let branch_b = repo.current_branch();
    repo.create_file("b.txt", "b");
    repo.commit("Branch B commit");

    repo.run_stax(&["bc", "branch-c"]);
    repo.create_file("shared.txt", "from branch c");
    repo.commit("Branch C commit");

    let main_sha_before = repo.get_commit_sha("main");
    let a_sha_before = repo.get_commit_sha(&branch_a);
    let a_parent_rev_before = read_parent_revision(&repo, &branch_a);
    let b_sha_before = repo.get_commit_sha(&branch_b);
    let b_parent_rev_before = read_parent_revision(&repo, &branch_b);
    assert_eq!(
        a_parent_rev_before, main_sha_before,
        "branch-a's recorded parent revision should be main's sha before sync"
    );
    assert_eq!(
        b_parent_rev_before, a_sha_before,
        "branch-b's recorded parent revision should be branch-a's sha before sync"
    );

    // Advance remote main with a conflicting change to shared.txt, so
    // branch-c's rebase conflicts while branch-a's and branch-b's do not.
    repo.simulate_remote_commit(
        "shared.txt",
        "from remote main",
        "Remote conflicting commit",
    );

    let sync_out = repo.run_stax(&["sync", "--force", "--restack"]);
    assert!(
        !sync_out.status.success(),
        "sync should stop on the branch-c conflict"
    );

    assert_ne!(
        read_parent_revision(&repo, &branch_a),
        a_parent_rev_before,
        "sync should have rewritten branch-a's metadata before hitting the conflict"
    );
    assert_ne!(
        read_parent_revision(&repo, &branch_b),
        b_parent_rev_before,
        "sync should have rewritten branch-b's metadata before hitting the conflict"
    );

    repo.abort_rebase();

    let undo_out = repo.run_stax(&["undo", "--yes", "--no-push"]);
    assert!(
        undo_out.status.success(),
        "undo failed: {}",
        TestRepo::stderr(&undo_out)
    );

    assert_eq!(
        repo.get_commit_sha(&branch_a),
        a_sha_before,
        "undo should restore branch-a to its pre-sync commit"
    );
    assert_eq!(
        read_parent_revision(&repo, &branch_a),
        a_parent_rev_before,
        "undo should restore branch-a's metadata to its pre-sync parent revision"
    );
    assert_eq!(
        repo.get_commit_sha(&branch_b),
        b_sha_before,
        "undo should restore branch-b to its pre-sync commit"
    );
    assert_eq!(
        read_parent_revision(&repo, &branch_b),
        b_parent_rev_before,
        "undo should restore branch-b's metadata to its pre-sync parent revision"
    );
    assert_eq!(
        repo.get_commit_sha("main"),
        main_sha_before,
        "undo should restore main to its pre-sync sha"
    );
}
