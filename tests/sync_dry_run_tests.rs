use crate::common::TestRepo;

/// Helper: resolve a local ref to its SHA (None if ref doesn't exist).
fn resolve_ref(repo: &TestRepo, refname: &str) -> Option<String> {
    let out = repo.git(&["rev-parse", "--verify", refname]);
    if out.status.success() {
        Some(TestRepo::stdout(&out).trim().to_string())
    } else {
        None
    }
}

/// Helper: return true if the local branch exists.
fn branch_exists(repo: &TestRepo, branch: &str) -> bool {
    resolve_ref(repo, &format!("refs/heads/{branch}")).is_some()
}

/// Helper: count stax receipts so we can assert that --dry-run writes none.
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

/// Helper: return the number of entries in `git stash list`.
fn stash_count(repo: &TestRepo) -> usize {
    let out = repo.git(&["stash", "list"]);
    TestRepo::stdout(&out)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

/// Snapshot remote-tracking refs as refname/OID pairs so dry-run tests can
/// verify that probing origin did not update local remote state.
fn remote_tracking_refs(repo: &TestRepo) -> String {
    let out = repo.git(&[
        "for-each-ref",
        "--format=%(refname) %(objectname)",
        "refs/remotes/origin",
    ]);
    assert!(
        out.status.success(),
        "failed to snapshot remote-tracking refs: {}",
        TestRepo::stderr(&out)
    );
    TestRepo::stdout(&out)
}

/// Snapshot FETCH_HEAD, preserving both its bytes and whether Git has created
/// it yet. `git ls-remote` must not change either state.
fn fetch_head(repo: &TestRepo) -> Option<Vec<u8>> {
    std::fs::read(repo.path().join(".git/FETCH_HEAD")).ok()
}

// ─── Test (a) ────────────────────────────────────────────────────────────────
// --dry-run reports a merged branch but deletes nothing: branch still exists,
// metadata ref still resolves, no receipt written, git status unchanged.

#[test]
fn dry_run_reports_merged_branch_and_deletes_nothing() {
    let repo = TestRepo::new_with_remote();

    // Create feature-a, push it, merge it on the remote.
    repo.run_stax(&["bc", "feature-dry"]);
    let feature = repo.current_branch();
    repo.create_file("feature-dry.txt", "dry content");
    repo.commit("Feature dry commit");
    repo.git(&["push", "-u", "origin", &feature]);

    repo.run_stax(&["t"]);
    repo.merge_branch_on_remote(&feature);
    repo.git(&["pull", "origin", "main"]);

    let pre_receipts = receipt_count(&repo);
    let sha_before = resolve_ref(&repo, &format!("refs/heads/{feature}"));
    let meta_before = resolve_ref(&repo, &format!("refs/branch-metadata/{feature}"));

    // Run sync --dry-run; should exit 0 and report the merged branch.
    let out = repo.run_stax(&["sync", "--dry-run", "--force"]);
    assert!(
        out.status.success(),
        "--dry-run should always exit 0; stderr: {}",
        TestRepo::stderr(&out)
    );
    let stdout = TestRepo::stdout(&out);
    assert!(
        stdout.contains(&feature) || stdout.contains("merged"),
        "--dry-run should mention the merged branch or 'merged'; got:\n{stdout}"
    );

    // Nothing deleted.
    assert!(
        branch_exists(&repo, &feature),
        "--dry-run must NOT delete {feature}"
    );
    let sha_after = resolve_ref(&repo, &format!("refs/heads/{feature}"));
    assert_eq!(
        sha_before, sha_after,
        "--dry-run must not move the branch tip"
    );
    let meta_after = resolve_ref(&repo, &format!("refs/branch-metadata/{feature}"));
    assert_eq!(
        meta_before, meta_after,
        "--dry-run must not change metadata ref"
    );
    assert_eq!(
        receipt_count(&repo),
        pre_receipts,
        "--dry-run must write no receipt"
    );
}

// ─── Test (b) ────────────────────────────────────────────────────────────────
// Clean, up-to-date repo → "no changes made", no "would delete" lines.

#[test]
fn dry_run_clean_repo_says_no_changes() {
    let repo = TestRepo::new_with_remote();

    // Push main to remote so remote has a trunk ref.
    repo.git(&["push", "-u", "origin", "main"]);

    let out = repo.run_stax(&["sync", "--dry-run"]);
    assert!(
        out.status.success(),
        "--dry-run should exit 0; stderr: {}",
        TestRepo::stderr(&out)
    );

    let stdout = TestRepo::stdout(&out);
    let combined = stdout.clone() + &TestRepo::stderr(&out);

    assert!(
        combined.contains("no changes")
            || combined.contains("up to date")
            || combined.contains("nothing to do"),
        "--dry-run on clean repo should indicate nothing to do; got:\n{combined}"
    );
    // No "would delete" lines expected.
    assert!(
        !stdout.contains("would delete"),
        "--dry-run clean repo should not say 'would delete'; stdout:\n{stdout}"
    );
}

// ─── Test (b.1) ──────────────────────────────────────────────────────────────
// Remote discovery must use ls-remote, not fetch: both text and JSON plans leave
// FETCH_HEAD and remote-tracking refs untouched even when origin/main advances.

#[test]
fn dry_run_remote_probe_does_not_update_fetch_head_or_tracking_refs() {
    let repo = TestRepo::new_with_remote();
    let tracking_before = remote_tracking_refs(&repo);
    let fetch_head_before = fetch_head(&repo);

    // Advance origin without fetching locally. A fetch-based implementation
    // would update origin/main and FETCH_HEAD here.
    repo.simulate_remote_commit("remote-only.txt", "remote", "remote-only commit");

    for args in [
        ["sync", "--dry-run"].as_slice(),
        ["sync", "--dry-run", "--json"].as_slice(),
    ] {
        let out = repo.run_stax(args);
        assert!(
            out.status.success(),
            "{} should succeed: {}",
            args.join(" "),
            TestRepo::stderr(&out)
        );
        assert_eq!(
            fetch_head(&repo),
            fetch_head_before,
            "{} must not modify .git/FETCH_HEAD",
            args.join(" ")
        );
        assert_eq!(
            remote_tracking_refs(&repo),
            tracking_before,
            "{} must not update refs/remotes/origin/*",
            args.join(" ")
        );
    }
}

// ─── Test (c) ────────────────────────────────────────────────────────────────
// --restack lists stale branches, SHAs unchanged after the command.

#[test]
fn dry_run_restack_lists_stale_branches_without_rewriting_refs() {
    let repo = TestRepo::new_with_remote();

    // Build stack: main → feat-c; push main to remote so trunk can be probed.
    repo.git(&["push", "-u", "origin", "main"]);
    repo.run_stax(&["bc", "feat-c"]);
    let feature = repo.current_branch();
    repo.create_file("c.txt", "c");
    repo.commit("C commit");

    let sha_before =
        resolve_ref(&repo, &format!("refs/heads/{feature}")).expect("feat-c should exist");
    let main_sha_before = resolve_ref(&repo, "refs/heads/main").expect("main should exist");

    // Run --dry-run --restack.
    let out = repo.run_stax(&["sync", "--dry-run", "--restack"]);
    assert!(
        out.status.success(),
        "--dry-run --restack should exit 0; stderr: {}",
        TestRepo::stderr(&out)
    );

    // SHAs must be byte-identical after the command.
    let sha_after =
        resolve_ref(&repo, &format!("refs/heads/{feature}")).expect("feat-c should still exist");
    let main_sha_after = resolve_ref(&repo, "refs/heads/main").expect("main should still exist");

    assert_eq!(
        sha_before, sha_after,
        "--dry-run --restack must not move feat-c"
    );
    assert_eq!(
        main_sha_before, main_sha_after,
        "--dry-run --restack must not move main"
    );
}

// ─── Test (d) ────────────────────────────────────────────────────────────────
// Dirty tree: left unstashed after --dry-run; stash list empty, file unmodified.

#[test]
fn dry_run_dirty_tree_left_unstashed() {
    let repo = TestRepo::new_with_remote();
    repo.git(&["push", "-u", "origin", "main"]);

    // Create an uncommitted change.
    repo.create_file("dirty.txt", "dirty content");

    let out = repo.run_stax(&["sync", "--dry-run"]);
    assert!(
        out.status.success(),
        "--dry-run with dirty tree should exit 0; stderr: {}",
        TestRepo::stderr(&out)
    );

    // Stash list must remain empty.
    assert_eq!(
        stash_count(&repo),
        0,
        "--dry-run must not stash the working tree"
    );

    // The file must still be present and unmodified.
    let content = std::fs::read_to_string(repo.path().join("dirty.txt"))
        .expect("dirty.txt should still exist");
    assert_eq!(
        content, "dirty content",
        "--dry-run must not modify dirty.txt"
    );

    // Output should mention "would stash" or "dirty" but NOT that it was stashed.
    let stdout = TestRepo::stdout(&out);
    assert!(
        stdout.contains("dirty") || stdout.contains("stash"),
        "--dry-run should report dirty tree; stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("✓ Stashed"),
        "--dry-run must not report that it stashed changes"
    );
}

// ─── Test (e) ────────────────────────────────────────────────────────────────
// --continue is rejected by clap (conflicts_with).

#[test]
fn dry_run_rejects_continue_flag() {
    let repo = TestRepo::new_with_remote();
    let out = repo.run_stax(&["sync", "--dry-run", "--continue"]);
    assert!(
        !out.status.success(),
        "--dry-run --continue should be rejected by clap"
    );
    let stderr = TestRepo::stderr(&out);
    assert!(
        stderr.contains("cannot be used with") || stderr.contains("conflicts"),
        "expected a 'conflicts' error; got: {stderr}"
    );
}

// ─── Test (f) ────────────────────────────────────────────────────────────────
// --force emits a stderr warning but still exits 0.

#[test]
fn dry_run_warns_force_is_ignored() {
    let repo = TestRepo::new_with_remote();
    repo.git(&["push", "-u", "origin", "main"]);

    let out = repo.run_stax(&["sync", "--dry-run", "--force"]);
    assert!(
        out.status.success(),
        "--dry-run --force should exit 0; stderr: {}",
        TestRepo::stderr(&out)
    );
    let stderr = TestRepo::stderr(&out);
    assert!(
        stderr.contains("--force") || stderr.contains("ignored"),
        "expected warning about --force; stderr:\n{stderr}"
    );
}

// ─── Test (g) ────────────────────────────────────────────────────────────────
// --delete-upstream-gone protects branches with unique commits.

#[test]
fn dry_run_delete_upstream_gone_protects_unpushed_work() {
    let repo = TestRepo::new_with_remote();
    repo.git(&["push", "-u", "origin", "main"]);

    // Create a branch, push it to establish tracking, then add a local-only commit
    // AFTER the push. This produces a genuine [gone] upstream once we delete the
    // remote branch, with unique local work that must not be discarded.
    repo.run_stax(&["bc", "upstream-gone-local-work"]);
    let branch = repo.current_branch();
    repo.create_file("pushed.txt", "pushed");
    repo.commit("Pushed commit");
    repo.git(&["push", "-u", "origin", &branch]);

    // Local-only commit — never published to origin.
    repo.create_file("local-only.txt", "local only");
    repo.commit("Local-only commit");

    // Delete the remote branch to create a [gone] upstream.
    repo.git(&["checkout", "main"]);
    repo.git(&["push", "origin", "--delete", &branch]);
    repo.git(&["checkout", &branch]);

    let out = repo.run_stax(&["sync", "--dry-run", "--delete-upstream-gone"]);
    assert!(
        out.status.success(),
        "--dry-run --delete-upstream-gone should exit 0; stderr: {}",
        TestRepo::stderr(&out)
    );

    // Branch must still exist.
    assert!(
        branch_exists(&repo, &branch),
        "--dry-run must not delete {branch}"
    );

    let stdout = TestRepo::stdout(&out);
    let combined = stdout + &TestRepo::stderr(&out);
    // Should report protection (unique commits).
    assert!(
        combined.contains("protected") || combined.contains("unique"),
        "expected mention of protection for unpushed work; combined:\n{combined}"
    );
}

// ─── Test (h) ────────────────────────────────────────────────────────────────
// --plan alias produces the same output as --dry-run.

#[test]
fn plan_alias_output_equals_dry_run_output() {
    let repo = TestRepo::new_with_remote();
    repo.git(&["push", "-u", "origin", "main"]);

    let dry_out = repo.run_stax(&["sync", "--dry-run"]);
    let plan_out = repo.run_stax(&["sync", "--plan"]);

    // Both should succeed.
    assert!(dry_out.status.success(), "--dry-run failed");
    assert!(plan_out.status.success(), "--plan failed");

    // Stdout should be identical (strip timing-sensitive lines if any).
    let dry_stdout = TestRepo::stdout(&dry_out);
    let plan_stdout = TestRepo::stdout(&plan_out);

    assert_eq!(
        dry_stdout, plan_stdout,
        "--plan and --dry-run should produce identical stdout"
    );
}

#[test]
fn dry_run_and_execution_agree_on_dirty_linked_worktree_recovery() {
    // Independently built, identical states: planning cannot prepare execution.
    for (dry_run, upstream_only) in [true, false]
        .into_iter()
        .flat_map(|dry| [false, true].map(move |upstream| (dry, upstream)))
    {
        let repo = TestRepo::new_with_remote();
        assert!(repo.run_stax(&["bc", "dirty-linked-plan"]).status.success());
        let branch = repo.current_branch();
        repo.create_file("linked.txt", "committed\n");
        repo.commit("linked feature");
        assert!(
            repo.git(&["push", "-u", "origin", &branch])
                .status
                .success()
        );
        assert!(repo.run_stax(&["t"]).status.success());
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("target");
        assert!(
            repo.git(&["worktree", "add", target.to_str().unwrap(), &branch])
                .status
                .success()
        );
        std::fs::write(target.join("linked.txt"), "staged local work\n").unwrap();
        assert!(
            repo.git(&["-C", target.to_str().unwrap(), "add", "linked.txt"])
                .status
                .success()
        );
        std::fs::write(target.join("linked.txt"), "dirty local work\n").unwrap();
        std::fs::write(target.join("untracked.txt"), "untracked local work\n").unwrap();
        repo.merge_branch_on_remote(&branch);
        assert!(repo.git(&["pull", "origin", "main"]).status.success());
        if upstream_only {
            use crate::common::OutputAssertions;
            repo.git(&["push", "origin", "--delete", &branch])
                .assert_success();
        }
        let refs_before = TestRepo::stdout(&repo.git(&["show-ref"]));
        let index_before =
            TestRepo::stdout(&repo.git(&["-C", target.to_str().unwrap(), "show", ":linked.txt"]));
        let inventory_before = TestRepo::stdout(&repo.git(&["worktree", "list", "--porcelain"]));
        assert!(inventory_before.contains(&format!("branch refs/heads/{branch}")));
        let fetch_before = fetch_head(&repo);
        let receipts_before = receipt_count(&repo);
        let mut args = if dry_run {
            vec!["sync", "--dry-run", "--json", "--force"]
        } else {
            vec!["sync", "--json", "--force"]
        };
        if upstream_only {
            args.extend(["--no-delete", "--delete-upstream-gone"]);
        }
        let out = repo.run_stax(&args);
        assert!(
            out.status.success(),
            "{}\n{}",
            TestRepo::stdout(&out),
            TestRepo::stderr(&out)
        );
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        if dry_run {
            assert_eq!(TestRepo::stdout(&repo.git(&["show-ref"])), refs_before);
            assert_eq!(
                TestRepo::stdout(&repo.git(&["worktree", "list", "--porcelain"])),
                inventory_before
            );
            assert_eq!(
                TestRepo::stdout(&repo.git(&[
                    "-C",
                    target.to_str().unwrap(),
                    "show",
                    ":linked.txt"
                ])),
                index_before
            );
            assert_eq!(fetch_head(&repo), fetch_before);
            assert_eq!(receipt_count(&repo), receipts_before);
            assert_eq!(stash_count(&repo), 0);
            assert!(!repo.path().join(".git/stax/recovery").exists());
            assert_eq!(
                std::fs::read_to_string(target.join("linked.txt")).unwrap(),
                "dirty local work\n"
            );
            assert_eq!(
                std::fs::read_to_string(target.join("untracked.txt")).unwrap(),
                "untracked local work\n"
            );
            let candidate = json[if upstream_only {
                "upstream_gone_deletable"
            } else {
                "merged_candidates"
            }]
            .as_array()
            .expect("merged candidates")
            .iter()
            .find(|c| c["name"] == branch)
            .expect("linked candidate");
            assert_eq!(
                candidate["disposition"], "would_recover_then_delete",
                "{json}"
            );
        } else {
            assert!(!target.exists(), "{json}");
            assert!(!branch_exists(&repo, &branch));
            assert!(resolve_ref(&repo, &format!("refs/branch-metadata/{branch}")).is_none());
            let manifest = std::fs::read_dir(repo.path().join(".git/stax/recovery"))
                .unwrap()
                .flat_map(|op| std::fs::read_dir(op.unwrap().path()).unwrap())
                .map(|entry| entry.unwrap().path().join("manifest.json"))
                .find(|p| p.is_file())
                .expect("durable recovery manifest");
            let saved: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
            assert_eq!(saved["branch"], branch);
            let dir = manifest.parent().unwrap();
            assert!(dir.join("branch.bundle").is_file());
            assert!(
                std::fs::read_to_string(dir.join("unstaged.patch"))
                    .unwrap()
                    .contains("+dirty local work")
            );
            let entry = saved["untracked"]
                .as_array()
                .unwrap()
                .iter()
                .find_map(|v| v.as_str()?.strip_prefix("untracked.txt\t"))
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.join(entry)).unwrap(),
                "untracked local work\n"
            );
            // Restore using only the external archive, after the target is gone.
            // This verifies content and staged/unstaged separation at the Git boundary.
            use crate::common::OutputAssertions;
            let restored = parent.path().join("restored");
            std::fs::create_dir(&restored).unwrap();
            let git =
                |args: &[&str]| repo.git(&[&["-C", restored.to_str().unwrap()][..], args].concat());
            git(&["init"]).assert_success();
            git(&[
                "fetch",
                dir.join("branch.bundle").to_str().unwrap(),
                saved["branch_ref"].as_str().unwrap(),
            ])
            .assert_success();
            git(&["checkout", "--detach", "FETCH_HEAD"]).assert_success();
            git(&[
                "apply",
                "--index",
                "--binary",
                dir.join("staged.patch").to_str().unwrap(),
            ])
            .assert_success();
            git(&[
                "apply",
                "--binary",
                dir.join("unstaged.patch").to_str().unwrap(),
            ])
            .assert_success();
            std::fs::copy(dir.join(entry), restored.join("untracked.txt")).unwrap();
            assert_eq!(
                std::fs::read_to_string(restored.join("linked.txt")).unwrap(),
                "dirty local work\n"
            );
            assert_eq!(
                TestRepo::stdout(&git(&["show", ":linked.txt"])),
                "staged local work\n"
            );
            assert_eq!(
                std::fs::read_to_string(restored.join("untracked.txt")).unwrap(),
                "untracked local work\n"
            );
        }
    }
}

/// A merged, published branch whose checkout lives outside the sync caller.
fn linked_retirement_fixture() -> (TestRepo, tempfile::TempDir, std::path::PathBuf, String) {
    use crate::common::OutputAssertions;
    let repo = TestRepo::new_with_remote();
    repo.run_stax(&["bc", "retirement-target"]).assert_success();
    let branch = repo.current_branch();
    repo.create_file("target.txt", "merged work\n");
    repo.commit("merged work");
    repo.git(&["push", "-u", "origin", &branch])
        .assert_success();
    repo.git(&["checkout", "main"]).assert_success();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    repo.git(&["worktree", "add", target.to_str().unwrap(), &branch])
        .assert_success();
    repo.git(&["merge", "--ff-only", &branch]).assert_success();
    repo.git(&["push", "origin", "main"]).assert_success();
    (repo, dir, target, branch)
}

#[cfg(unix)]
#[test]
fn dry_run_and_execution_retain_bb_subdirectory_owners() {
    use crate::common::OutputAssertions;
    use std::os::unix::fs::PermissionsExt;
    for mode in [
        "active",
        "current",
        "unknown",
        "missing-active",
        "missing-current",
        "unresolvable-active",
        "unresolvable-current",
        "unreadable-active",
        "unreadable-current",
    ] {
        for (dry_run, upstream_only) in [true, false]
            .into_iter()
            .flat_map(|dry| [false, true].map(move |upstream| (dry, upstream)))
        {
            let (repo, dir, target, branch) = linked_retirement_fixture();
            if mode.starts_with("unresolvable") {
                std::fs::write(target.join("web"), "not a directory").unwrap();
            } else if !mode.starts_with("missing") {
                std::fs::create_dir(target.join("web")).unwrap();
                if mode.starts_with("unreadable") {
                    std::fs::set_permissions(
                        target.join("web"),
                        std::fs::Permissions::from_mode(0o000),
                    )
                    .unwrap();
                }
            }
            let log = dir.path().join("bb.jsonl");
            let cli = dir.path().join("bb.py");
            let remote = TestRepo::stdout(&repo.git(&["remote", "get-url", "origin"]));
            let script = r#"#!/usr/bin/env python3
import json, sys
a = sys.argv[1:]
with open(LOG, 'a') as f: f.write(json.dumps(a)+'\n')
def out(v): print(json.dumps(v))
if a == ['status', '--json']:
 out({'thread':{'environment':{'hostId':'fixture-host','display':{'id':('target-env' if MODE.endswith('current') else 'caller-env')}}}})
elif a[:2] == ['environment', 'list']:
 out([{'id':'target-env','path':TARGET+'/web','projectId':'fixture-project','environmentProviderId':'fixture-provider','managed':True,'isWorktree':True,'status':'ready'}])
elif a[:2] == ['environment', 'providers']:
 out([] if MODE == 'unknown' else [{'id':'fixture-provider'}])
elif a[:2] == ['project', 'show']:
 out({'gitRemoteUrl':REMOTE})
elif a[:2] == ['thread', 'list']:
 out([{'id':'busy-fixture-thread','status':'running'}])
else:
 sys.exit(43)
"#
                .replace("LOG", &serde_json::to_string(&log).unwrap())
                .replace("TARGET", &serde_json::to_string(&target).unwrap())
                .replace("REMOTE", &serde_json::to_string(remote.trim()).unwrap())
                .replace("MODE", &serde_json::to_string(mode).unwrap());
            std::fs::write(&cli, script).unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
            if upstream_only {
                use crate::common::OutputAssertions;
                repo.git(&["push", "origin", "--delete", &branch])
                    .assert_success();
            }
            let refs_before = TestRepo::stdout(&repo.git(&["show-ref"]));
            let head_before =
                TestRepo::stdout(&repo.git(&["-C", target.to_str().unwrap(), "rev-parse", "HEAD"]));
            let mut args = if dry_run {
                vec!["sync", "--dry-run", "--json", "--force"]
            } else {
                vec!["sync", "--json", "--force"]
            };
            if upstream_only {
                args.extend(["--no-delete", "--delete-upstream-gone"]);
            }
            let out = repo.run_stax_with_env(
                &args,
                &[
                    ("BB_CLI", cli.to_str().unwrap()),
                    ("BB_THREAD_ID", "caller-thread"),
                ],
            );
            out.assert_success();
            let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            if dry_run {
                if upstream_only {
                    assert!(
                        json["upstream_gone_protected"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|v| v == &branch),
                        "{mode}: {json}"
                    );
                    assert!(
                        !json["upstream_gone_deletable"]
                            .as_array()
                            .is_some_and(|xs| xs.iter().any(|v| v["name"] == branch)),
                        "{mode}: {json}"
                    );
                } else {
                    let candidate = json["merged_candidates"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|c| c["name"] == branch)
                        .unwrap();
                    assert_eq!(
                        candidate["disposition"], "would_keep_worktree",
                        "{mode}: {json}"
                    );
                    let expected = if mode == "active" {
                        "ActiveAgentWork"
                    } else {
                        "UnknownOwner"
                    };
                    assert!(
                        candidate["keep_reason"]
                            .as_str()
                            .unwrap()
                            .contains(expected),
                        "{mode}: {json}"
                    );
                }
                assert_eq!(receipt_count(&repo), 0);
            }
            assert!(target.is_dir());
            if mode.starts_with("unreadable") {
                std::fs::set_permissions(
                    target.join("web"),
                    std::fs::Permissions::from_mode(0o755),
                )
                .unwrap();
            }
            assert_eq!(
                TestRepo::stdout(&repo.git(&["show-ref"])),
                refs_before,
                "{mode}: {json}"
            );
            assert_eq!(
                TestRepo::stdout(&repo.git(&["-C", target.to_str().unwrap(), "rev-parse", "HEAD"])),
                head_before
            );
            assert_eq!(
                TestRepo::stdout(&repo.git(&[
                    "-C",
                    target.to_str().unwrap(),
                    "branch",
                    "--show-current"
                ]))
                .trim(),
                branch
            );
            assert_eq!(
                std::fs::read_to_string(target.join("target.txt")).unwrap(),
                "merged work\n"
            );
            assert!(!repo.path().join(".git/stax/recovery").exists());
            let calls: Vec<Vec<String>> = std::fs::read_to_string(&log)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert!(
                calls
                    .iter()
                    .any(|a| a.starts_with(&["environment".into(), "list".into()]))
            );
            assert!(
                calls.iter().all(|a| !a
                    .iter()
                    .any(|s| ["delete", "archive", "archive-threads"].contains(&s.as_str()))),
                "unexpected BB effects: {calls:?}"
            );
            if mode == "active" {
                assert!(
                    calls.iter().any(|a| a
                        == &[
                            "thread",
                            "list",
                            "--environment",
                            "target-env",
                            "--limit",
                            "5000",
                            "--json"
                        ]),
                    "selected ID missing: {calls:?}"
                );
            }
        }
    }
}

#[test]
fn dry_run_and_execution_retain_primary_target() {
    use crate::common::OutputAssertions;
    for (dry_run, primary_caller) in [true, false]
        .into_iter()
        .flat_map(|dry| [false, true].map(move |primary| (dry, primary)))
    {
        let repo = TestRepo::new_with_remote();
        repo.run_stax(&["bc", "primary-target"]).assert_success();
        let branch = repo.current_branch();
        repo.create_file("primary.txt", "primary data\n");
        repo.commit("primary feature");
        repo.git(&["push", "-u", "origin", &branch])
            .assert_success();
        repo.git(&["branch", "-f", "main", &branch])
            .assert_success();
        repo.git(&["push", "origin", "main"]).assert_success();
        let dir = tempfile::tempdir().unwrap();
        let caller = dir.path().join("caller");
        repo.git(&["worktree", "add", caller.to_str().unwrap(), "main"])
            .assert_success();
        let refs_before = TestRepo::stdout(&repo.git(&["show-ref"]));
        let head_before = repo.get_commit_sha("HEAD");
        let args = if dry_run {
            vec!["sync", "--dry-run", "--json", "--force"]
        } else {
            vec!["sync", "--json", "--force"]
        };
        let primary = repo.path();
        let out = repo.run_stax_in(if primary_caller { &primary } else { &caller }, &args);
        out.assert_success();
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        if dry_run {
            let candidate = json["merged_candidates"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == branch)
                .unwrap();
            assert_eq!(candidate["disposition"], "would_keep_worktree", "{json}");
            assert!(
                candidate["keep_reason"]
                    .as_str()
                    .unwrap()
                    .contains("PrimaryCheckout")
            );
        }
        assert_eq!(TestRepo::stdout(&repo.git(&["show-ref"])), refs_before);
        assert_eq!(repo.get_commit_sha("HEAD"), head_before);
        assert_eq!(repo.current_branch(), branch);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("primary.txt")).unwrap(),
            "primary data\n"
        );
        assert!(!repo.path().join(".git/stax/recovery").exists());
    }
}

#[tokio::test]
async fn dry_run_and_execution_retain_open_pr_with_missing_remote_and_ancestor_tip() {
    use crate::common::{OutputAssertions, github_mock::write_stax_config};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    for dry_run in [true, false] {
        let server = MockServer::start().await;
        let (repo, _dir, target, branch) = linked_retirement_fixture();
        repo.configure_github_like_submit_remote();
        write_stax_config(std::path::Path::new(&repo.clean_home()), &server.uri(), "");
        let metadata = serde_json::json!({
            "parentBranchName": "main", "parentBranchRevision": repo.get_commit_sha("main"),
            "prInfo": { "number": 991, "state": "OPEN", "isDraft": false }
        });
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), metadata.to_string()).unwrap();
        let hash = repo.git(&["hash-object", "-w", file.path().to_str().unwrap()]);
        hash.assert_success();
        repo.git(&[
            "update-ref",
            &format!("refs/branch-metadata/{branch}"),
            TestRepo::stdout(&hash).trim(),
        ])
        .assert_success();
        repo.git(&["push", "origin", "--delete", &branch])
            .assert_success();
        repo.git(&["merge-base", "--is-ancestor", &branch, "main"])
            .assert_success();
        let head = repo.get_commit_sha(&branch);
        let endpoint = "/repos/test-owner/test-repo/pulls/991";
        Mock::given(method("GET")).and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "url":"https://api.github.com/repos/test-owner/test-repo/pulls/991", "id":991, "number":991,
                "state":"open", "merged":false, "draft":false, "title":"Still open", "body":"",
                "head":{"ref":branch,"sha":head,"label":format!("test-owner:{branch}")},
                "base":{"ref":"main","sha":repo.get_commit_sha("main")},
                "html_url":"https://github.com/test-owner/test-repo/pull/991"
            }))).expect(1).mount(&server).await;
        let refs_before = TestRepo::stdout(&repo.git(&["show-ref"]));
        let args = if dry_run {
            vec![
                "sync",
                "--dry-run",
                "--json",
                "--force",
                "--delete-upstream-gone",
            ]
        } else {
            vec!["sync", "--json", "--force", "--delete-upstream-gone"]
        };
        let out = repo.run_stax_with_env(&args, &[("STAX_GITHUB_TOKEN", "isolated-fixture-token")]);
        out.assert_success();
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        for field in [
            "merged_candidates",
            "upstream_gone_deletable",
            "deleted_branches",
        ] {
            assert!(
                !json[field]
                    .as_array()
                    .is_some_and(|xs| xs.iter().any(|x| x["name"] == branch)),
                "{json}"
            );
        }
        if dry_run {
            assert_eq!(TestRepo::stdout(&repo.git(&["show-ref"])), refs_before);
        }
        assert!(target.is_dir());
        assert_eq!(repo.get_commit_sha(&branch), head);
        assert_eq!(
            TestRepo::stdout(&repo.git(&[
                "-C",
                target.to_str().unwrap(),
                "branch",
                "--show-current"
            ]))
            .trim(),
            branch
        );
        let saved: serde_json::Value = serde_json::from_slice(
            &repo
                .git(&["show", &format!("refs/branch-metadata/{branch}")])
                .stdout,
        )
        .unwrap();
        assert_eq!(saved["prInfo"]["number"], 991);
        assert_eq!(
            saved["prInfo"]["state"]
                .as_str()
                .unwrap()
                .to_ascii_uppercase(),
            "OPEN"
        );
        assert!(!repo.path().join(".git/stax/recovery").exists());
        let requests = server.received_requests().await.unwrap();
        assert!(!requests.is_empty());
        assert!(
            requests
                .iter()
                .all(|r| r.method.as_str() == "GET" && r.url.path() == endpoint),
            "unexpected forge effects: {requests:?}"
        );
        server.verify().await;
    }
}

#[cfg(unix)]
#[test]
fn cycle3_bb_subdirectory_success_removes_exact_environment() {
    use crate::common::OutputAssertions;
    use std::os::unix::fs::PermissionsExt;
    let (repo, dir, target, branch) = linked_retirement_fixture();
    std::fs::create_dir(target.join("web")).unwrap();
    let cli = dir.path().join("bb.py");
    let log = dir.path().join("bb.jsonl");
    let remote = TestRepo::stdout(&repo.git(&["remote", "get-url", "origin"]));
    let script = r#"#!/usr/bin/env python3
import json,sys,os,subprocess
a=sys.argv[1:]
with open(LOG,'a') as f: f.write(json.dumps(a)+'\n')
def out(v): print(json.dumps(v))
if a == ['status','--json']:
 out({'thread':{'environment':{'hostId':'h','display':{'id':'caller'}}}})
elif a[:2] == ['environment','list']:
 out([{'id':'selected','path':TARGET+'/web','projectId':'p','environmentProviderId':'provider','managed':True,'isWorktree':True,'status':'ready'}])
elif a[:2] == ['environment','providers']: out([{'id':'provider'}])
elif a[:2] == ['project','show']: out({'gitRemoteUrl':REMOTE})
elif a[:2] == ['thread','list']: out([{'id':'idle','status':'completed'}])
elif a[:2] == ['thread','show']: out({'thread':{'environmentId':'selected','status':'completed'}})
elif a[:2] == ['thread','archive']:
 assert a == ['thread','archive','idle','--json']; out({})
elif a[:2] == ['environment','show']:
 assert a[2] == 'selected'; out({'path':TARGET+'/web','status':'ready' if os.path.exists(TARGET) else 'destroyed'})
elif a[:2] == ['environment','delete']:
 assert a == ['environment','delete','selected','--json']
 subprocess.run(['git','-C',PRIMARY,'worktree','remove','--force',TARGET],check=True)
 out({})
else: sys.exit(43)
"#.replace("LOG", &serde_json::to_string(&log).unwrap())
 .replace("TARGET", &serde_json::to_string(&target).unwrap())
 .replace("PRIMARY", &serde_json::to_string(&repo.path()).unwrap())
 .replace("REMOTE", &serde_json::to_string(remote.trim()).unwrap());
    std::fs::write(&cli, script).unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = repo.run_stax_with_env(
        &["sync", "--json", "--force"],
        &[
            ("BB_CLI", cli.to_str().unwrap()),
            ("BB_THREAD_ID", "origin-thread"),
        ],
    );
    out.assert_success();
    assert!(!target.exists(), "{}", TestRepo::stdout(&out));
    assert!(!branch_exists(&repo, &branch));
    assert!(
        !repo
            .git(&["config", "--get", &format!("branch.{branch}.remote")])
            .status
            .success(),
        "successful guarded deletion must preserve branch -D config cleanup"
    );
    let calls = std::fs::read_to_string(log).unwrap();
    assert!(calls.contains("[\"thread\", \"archive\", \"idle\", \"--json\"]"));
    assert!(calls.contains("[\"environment\", \"delete\", \"selected\", \"--json\"]"));
}

#[cfg(unix)]
#[test]
fn cycle3_bb_owned_worktree_with_operation_marker_is_retained() {
    use crate::common::OutputAssertions;
    use std::os::unix::fs::PermissionsExt;

    for marker_present in [true, false] {
        let (repo, dir, target, branch) = linked_retirement_fixture();
        if marker_present {
            let marker = TestRepo::stdout(&repo.git(&[
                "-C",
                target.to_str().unwrap(),
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "BISECT_START",
            ]));
            std::fs::write(marker.trim(), "refs/heads/main\n").unwrap();
        }
        let cli = dir.path().join("bb.py");
        let log = dir.path().join("bb.jsonl");
        let remote = TestRepo::stdout(&repo.git(&["remote", "get-url", "origin"]));
        let script = r#"#!/usr/bin/env python3
import json,sys
import os,subprocess
a=sys.argv[1:]
with open(LOG,'a') as f: f.write(json.dumps(a)+'\\n')
def out(v): print(json.dumps(v))
if a == ['status','--json']: out({'thread':{'environment':{'hostId':'h','display':{'id':'caller'}}}})
elif a[:2] == ['environment','list']: out([{'id':'selected','path':TARGET,'projectId':'p','environmentProviderId':'provider','managed':True,'isWorktree':True,'status':'ready'}])
elif a[:2] == ['environment','providers']: out([{'id':'provider'}])
elif a[:2] == ['project','show']: out({'gitRemoteUrl':REMOTE})
elif a[:2] == ['thread','list']: out([{'id':'idle','status':'completed'}])
elif a[:2] == ['thread','show']: out({'thread':{'environmentId':'selected','status':'completed'}})
elif a[:2] == ['thread','archive']:
 assert a == ['thread','archive','idle','--json']; out({})
elif a[:2] == ['environment','show']:
 assert a[2] == 'selected'; out({'path':TARGET,'status':'ready' if os.path.exists(TARGET) else 'destroyed'})
elif a[:2] == ['environment','delete']:
 assert a == ['environment','delete','selected','--json']
 subprocess.run(['git','-C',PRIMARY,'worktree','remove','--force',TARGET],check=True)
 out({})
else: sys.exit(43)
"#
        .replace("LOG", &serde_json::to_string(&log).unwrap())
        .replace("TARGET", &serde_json::to_string(&target).unwrap())
        .replace("PRIMARY", &serde_json::to_string(&repo.path()).unwrap())
        .replace("REMOTE", &serde_json::to_string(remote.trim()).unwrap());
        std::fs::write(&cli, script).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();

        let out = repo.run_stax_with_env(
            &["sync", "--json", "--force"],
            &[
                ("BB_CLI", cli.to_str().unwrap()),
                ("BB_THREAD_ID", "origin-thread"),
            ],
        );
        out.assert_success();
        let calls = std::fs::read_to_string(log).unwrap();
        if marker_present {
            assert!(target.is_dir(), "{}", TestRepo::stdout(&out));
            assert!(branch_exists(&repo, &branch));
            assert!(
                !calls.contains("archive") && !calls.contains("delete"),
                "protected operation must stop before BB lifecycle mutation: {calls}"
            );
            let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            let reason = report["skipped_branches"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["name"] == branch)
                .and_then(|item| item["reason"].as_str())
                .unwrap();
            assert!(reason.contains("UnknownOperation"), "{reason}");
        } else {
            assert!(!target.exists(), "an eligible BB fixture must retire");
            assert!(!branch_exists(&repo, &branch));
            assert!(calls.contains("[\"thread\", \"archive\", \"idle\", \"--json\"]"));
            assert!(calls.contains("[\"environment\", \"delete\", \"selected\", \"--json\"]"));
        }
    }
}

#[test]
fn cycle3_stale_local_trunk_upstream_gone_plan_matches_execution() {
    use crate::common::OutputAssertions;
    let (repo, _dir, target, branch) = linked_retirement_fixture();
    let merged = repo.get_commit_sha("origin/main");
    // Only the local trunk moves back. Remote tracking is already current.
    repo.git(&["reset", "--hard", "main~1"]).assert_success();
    repo.git(&["push", "origin", "--delete", &branch])
        .assert_success();
    repo.git(&["fetch", "--prune", "origin"]).assert_success();
    assert_ne!(repo.get_commit_sha("main"), merged);
    assert_eq!(repo.get_commit_sha("origin/main"), merged);
    let refs = TestRepo::stdout(&repo.git(&["show-ref"]));
    let out = repo.run_stax(&[
        "sync",
        "--dry-run",
        "--json",
        "--force",
        "--no-delete",
        "--delete-upstream-gone",
    ]);
    out.assert_success();
    let plan: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        plan["upstream_gone_deletable"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["name"] == branch),
        "{plan}"
    );
    assert_eq!(TestRepo::stdout(&repo.git(&["show-ref"])), refs);
    let out = repo.run_stax(&[
        "sync",
        "--json",
        "--force",
        "--no-delete",
        "--delete-upstream-gone",
    ]);
    out.assert_success();
    assert!(!branch_exists(&repo, &branch), "{}", TestRepo::stdout(&out));
    assert!(!target.exists());
}

#[test]
fn cycle3_relative_post_remove_hook_leaves_primary_files_intact() {
    use crate::common::OutputAssertions;
    let (repo, dir, target, branch) = linked_retirement_fixture();
    let canonical_target = std::fs::canonicalize(&target).unwrap();
    std::fs::create_dir(repo.path().join("build")).unwrap();
    std::fs::write(repo.path().join("build/keep"), "primary").unwrap();
    let log = dir.path().join("hook-done");
    let command = format!(
        "rm -rf build; printf '%s' \"$STAX_REMOVED_WORKTREE_PATH\" > '{}'",
        log.display()
    );
    let config = format!(
        "[worktree.hooks]\npost_remove = {}\n",
        serde_json::to_string(&command).unwrap()
    );
    std::fs::write(repo.path().join("stax.toml"), config).unwrap();
    // Keep the primary clean so sync reaches the post-removal hook.
    repo.commit("primary hook configuration and protected files");
    let out = repo.run_stax(&["sync", "--json", "--force"]);
    out.assert_success();
    assert!(!target.exists(), "{}", TestRepo::stdout(&out));
    assert!(!branch_exists(&repo, &branch));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !log.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        std::fs::read_to_string(log).unwrap(),
        canonical_target.to_string_lossy()
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("build/keep")).unwrap(),
        "primary"
    );
}

#[cfg(unix)]
#[test]
fn cycle3_binary_changed_confirmation_state_retains_target() {
    use crate::common::{OutputAssertions, run_stax_in_script_with_env};
    use std::os::unix::fs::PermissionsExt;
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\"'\"'"));
    for mutation in [
        "detach-advance",
        "switch",
        "lock",
        "operation",
        "head",
        "owner",
    ] {
        for preserve in [false, true] {
            let (repo, dir, target, branch) = linked_retirement_fixture();
            repo.git(&["branch", "other", &branch]).assert_success();
            let marker = dir.path().join("owner-changed");
            let cli = dir.path().join("bb.py");
            let log = dir.path().join("bb.jsonl");
            let refs = dir.path().join("refs-after-mutation");
            let checkout = dir.path().join("checkout-after-mutation");
            let head = dir.path().join("head-after-mutation");
            let script = r#"#!/usr/bin/env python3
import json,sys,os
a=sys.argv[1:]
with open(LOG,'a') as f: f.write(json.dumps(a)+'\n')
if a == ['status','--json']:
 print(json.dumps({'thread':{'environment':{'hostId':'h','display':{'id':'current'}}}}))
elif a[:2] == ['environment','list']:
 print(json.dumps([{'id':'current','path':TARGET}] if os.path.exists(MARKER) else []))
else: sys.exit(43)
"#
            .replace("LOG", &serde_json::to_string(&log).unwrap())
            .replace("TARGET", &serde_json::to_string(&target).unwrap())
            .replace("MARKER", &serde_json::to_string(&marker).unwrap());
            std::fs::write(&cli, script).unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
            let t = quote(target.to_str().unwrap());
            let b = quote(&branch);
            let change = match mutation {
                "detach-advance" => format!(
                    "git -C {t} checkout --detach; git -C {t} commit --allow-empty -m advanced; git update-ref refs/heads/{b} \"$(git -C {t} rev-parse HEAD)\""
                ),
                "switch" => format!("git -C {t} switch other"),
                "lock" => format!("git worktree lock {t}"),
                "operation" => format!(
                    "git -C {t} rev-parse HEAD > \"$(git -C {t} rev-parse --absolute-git-dir)/MERGE_HEAD\""
                ),
                "head" => format!("git -C {t} commit --allow-empty -m advanced"),
                "owner" => format!("touch {}", quote(marker.to_str().unwrap())),
                _ => unreachable!(),
            };
            let key = if preserve { "\\033[B\\n" } else { "\\n" };
            let input = format!(
                "wait_for_tui_text 'How should sync proceed?'; printf '\\n'; wait_for_tui_text 'What should stax do?'; {{ {change}; }} > /dev/null; git show-ref > {refs}; git -C {t} branch --show-current > {checkout}; git -C {t} rev-parse HEAD > {head}; printf '{key}'",
                refs = quote(refs.to_str().unwrap()),
                checkout = quote(checkout.to_str().unwrap()),
                head = quote(head.to_str().unwrap()),
            );
            let home = repo.clean_home();
            let mut env = vec![("HOME", home.as_str())];
            if mutation == "owner" {
                env.push(("BB_CLI", cli.to_str().unwrap()));
                env.push(("BB_THREAD_ID", "origin"));
            }
            let out = run_stax_in_script_with_env(&repo.path(), &["sync"], &input, &env);
            out.assert_success();
            let output = TestRepo::stdout(&out);
            assert!(
                output.contains("retry") || output.contains("not deleted locally"),
                "{mutation}/{preserve}: {output}"
            );
            assert!(target.is_dir(), "{mutation}/{preserve}: {output}");
            assert!(branch_exists(&repo, &branch));
            let original_refs = std::fs::read_to_string(refs).unwrap();
            let current_refs = TestRepo::stdout(&repo.git(&["show-ref"]));
            // Sync may save backup refs before attempting deletion. Every
            // original ref must still match; only those recovery refs may appear.
            for original in original_refs.lines() {
                assert!(
                    current_refs.lines().any(|line| line == original),
                    "original ref changed: {original}; {mutation}/{preserve}: {output}"
                );
            }
            for current in current_refs.lines() {
                assert!(
                    original_refs.lines().any(|line| line == current)
                        || current
                            .split_whitespace()
                            .nth(1)
                            .unwrap()
                            .starts_with("refs/stax/backups/"),
                    "unexpected ref: {current}; {mutation}/{preserve}: {output}"
                );
            }
            assert_eq!(
                TestRepo::stdout(&repo.git(&[
                    "-C",
                    target.to_str().unwrap(),
                    "branch",
                    "--show-current"
                ])),
                std::fs::read_to_string(checkout).unwrap()
            );
            assert_eq!(
                TestRepo::stdout(&repo.git(&["-C", target.to_str().unwrap(), "rev-parse", "HEAD"])),
                std::fs::read_to_string(head).unwrap()
            );
            assert!(!repo.path().join(".git/stax/recovery").exists());
            if mutation == "owner" {
                let calls = std::fs::read_to_string(log).unwrap();
                assert!(calls.contains("environment\", \"list"));
                assert!(!calls.contains("delete") && !calls.contains("archive"));
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn final_boundary_pre_remove_owner_change_retains_checkout_and_ref() {
    use crate::common::OutputAssertions;
    use std::os::unix::fs::PermissionsExt;
    for owner in ["current", "active", "unknown"] {
        let (repo, dir, target, branch) = linked_retirement_fixture();
        let marker = dir.path().join("registry-changed");
        let cli = dir.path().join("bb.py");
        let log = dir.path().join("mutations.jsonl");
        let reads = dir.path().join("reads.jsonl");
        let remote = TestRepo::stdout(&repo.git(&["remote", "get-url", "origin"]));
        let script = r#"#!/usr/bin/env python3
import json,sys,os
a=sys.argv[1:]
changed=os.path.exists(MARKER)
with open(READS,'a') as f: f.write(json.dumps([changed,a])+'\n')
def out(v): print(json.dumps(v))
if 'delete' in a or 'archive' in a:
 with open(LOG,'a') as f: f.write(json.dumps(a)+'\n')
 sys.exit(44)
if a == ['status','--json']:
 out({'thread':{'environment':{'hostId':'h','display':{'id':'current'}}}})
elif a[:2] == ['environment','list']:
 if changed and OWNER == 'unknown': sys.exit(45)
 out([{'id':OWNER,'path':TARGET,'projectId':'p','environmentProviderId':'provider','managed':True,'isWorktree':True,'status':'ready'}] if changed else [])
elif a[:2] == ['environment','providers']: out([{'id':'provider'}])
elif a[:2] == ['project','show']: out({'gitRemoteUrl':REMOTE})
elif a[:2] == ['thread','list']: out([{'id':'busy','status':'running'}])
else: sys.exit(43)
"#
            .replace("MARKER", &serde_json::to_string(&marker).unwrap())
            .replace("READS", &serde_json::to_string(&reads).unwrap())
            .replace("LOG", &serde_json::to_string(&log).unwrap())
            .replace("OWNER", &serde_json::to_string(owner).unwrap())
            .replace("TARGET", &serde_json::to_string(&target).unwrap())
            .replace("REMOTE", &serde_json::to_string(remote.trim()).unwrap());
        std::fs::write(&cli, script).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hook = format!("touch '{}'", marker.display());
        std::fs::write(
            repo.path().join("stax.toml"),
            format!(
                "[worktree.hooks]\npre_remove = {}\n",
                serde_json::to_string(&hook).unwrap()
            ),
        )
        .unwrap();
        repo.commit("configure registry-only pre-remove hook");
        let refs_before = TestRepo::stdout(&repo.git(&["show-ref"]));
        let head = repo.get_commit_sha(&branch);
        let status_before = repo.git(&["-C", target.to_str().unwrap(), "status", "--porcelain"]);
        status_before.assert_success();
        let out = repo.run_stax_with_env(
            &["sync", "--json", "--force"],
            &[
                ("BB_CLI", cli.to_str().unwrap()),
                ("BB_THREAD_ID", "origin"),
            ],
        );
        out.assert_success();
        assert!(marker.exists(), "hook must run: {}", TestRepo::stdout(&out));
        assert!(target.is_dir());
        assert_eq!(repo.get_commit_sha(&branch), head);
        assert_eq!(
            TestRepo::stdout(&repo.git(&[
                "-C",
                target.to_str().unwrap(),
                "branch",
                "--show-current"
            ]))
            .trim(),
            branch
        );
        assert_eq!(
            repo.git(&["-C", target.to_str().unwrap(), "status", "--porcelain"])
                .stdout,
            status_before.stdout
        );
        let refs_after = TestRepo::stdout(&repo.git(&["show-ref"]));
        for original in refs_before.lines() {
            assert!(
                refs_after.lines().any(|line| line == original),
                "lost original ref: {original}"
            );
        }
        assert!(!log.exists(), "no BB mutation is authorized");
        assert!(
            std::fs::read_to_string(reads)
                .unwrap()
                .lines()
                .any(|line| line.starts_with("[true, [\"environment\", \"list\""))
        );
        assert!(
            TestRepo::stderr(&out).contains("retry"),
            "{}",
            TestRepo::stderr(&out)
        );
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(json.get("deleted_branches").is_none(), "{json}");
    }
}

#[cfg(unix)]
#[test]
fn final_boundary_bb_teardown_changed_ref_is_retained_with_original_recovery() {
    use crate::common::OutputAssertions;
    use std::os::unix::fs::PermissionsExt;
    // Also prove checkout protection when the branch is reused without moving
    // its OID, and while a new checkout has detached HEAD for an operation.
    for mutation in ["advance", "checkout", "rebase"] {
        let (repo, dir, target, branch) = linked_retirement_fixture();
        let config_before =
            repo.git(&["config", "--get-regexp", &format!("^branch\\.{branch}\\.")]);
        config_before.assert_success();
        let original = repo.get_commit_sha(&branch);
        let replacement = dir.path().join("replacement");
        let advanced_file = dir.path().join("advanced-oid");
        let cli = dir.path().join("bb.py");
        let log = dir.path().join("bb.jsonl");
        let remote = TestRepo::stdout(&repo.git(&["remote", "get-url", "origin"]));
        let script = r#"#!/usr/bin/env python3
import json,sys,os,subprocess
a=sys.argv[1:]
with open(LOG,'a') as f: f.write(json.dumps(a)+'\n')
def out(v): print(json.dumps(v))
def git(*args): return subprocess.check_output(['git','-C',PRIMARY,*args],text=True).strip()
if a == ['status','--json']:
 out({'thread':{'environment':{'hostId':'h','display':{'id':'caller'}}}})
elif a[:2] == ['environment','list']:
 out([{'id':'selected','path':TARGET,'projectId':'p','environmentProviderId':'provider','managed':True,'isWorktree':True,'status':'ready'}])
elif a[:2] == ['environment','providers']: out([{'id':'provider'}])
elif a[:2] == ['project','show']: out({'gitRemoteUrl':REMOTE})
elif a[:2] == ['thread','list']: out([])
elif a[:2] == ['environment','show']:
 assert a[2] == 'selected'; out({'path':TARGET,'status':'ready' if os.path.exists(TARGET) else 'destroyed'})
elif a[:2] == ['environment','delete']:
 assert a == ['environment','delete','selected','--json']
 git('worktree','remove','--force',TARGET)
 if MUTATION == 'advance':
  advanced=git('commit-tree',git('rev-parse',BRANCH+'^{tree}'),'-p',ORIGINAL,'-m','new work during teardown')
  git('update-ref','refs/heads/'+BRANCH,advanced,ORIGINAL)
  with open(ADVANCED,'w') as f: f.write(advanced)
 else:
  git('worktree','add',REPLACEMENT,BRANCH)
  if MUTATION == 'rebase':
   git('-C',REPLACEMENT,'checkout','--detach')
   gd=git('-C',REPLACEMENT,'rev-parse','--absolute-git-dir')
   os.mkdir(gd+'/rebase-merge')
   with open(gd+'/rebase-merge/head-name','w') as f: f.write('refs/heads/'+BRANCH+'\n')
 out({})
else: sys.exit(43)
"#
            .replace("LOG", &serde_json::to_string(&log).unwrap())
            .replace("TARGET", &serde_json::to_string(&target).unwrap())
            .replace("PRIMARY", &serde_json::to_string(&repo.path()).unwrap())
            .replace("REMOTE", &serde_json::to_string(remote.trim()).unwrap())
            .replace("MUTATION", &serde_json::to_string(mutation).unwrap())
            .replace("BRANCH", &serde_json::to_string(&branch).unwrap())
            .replace("ORIGINAL", &serde_json::to_string(&original).unwrap())
            .replace("ADVANCED", &serde_json::to_string(&advanced_file).unwrap())
            .replace("REPLACEMENT", &serde_json::to_string(&replacement).unwrap());
        std::fs::write(&cli, script).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = repo.run_stax_with_env(
            &["sync", "--json", "--force"],
            &[
                ("BB_CLI", cli.to_str().unwrap()),
                ("BB_THREAD_ID", "origin-thread"),
            ],
        );
        out.assert_success();
        assert!(!target.exists(), "{}", TestRepo::stdout(&out));
        assert_eq!(
            repo.git(&["config", "--get-regexp", &format!("^branch\\.{branch}\\.")])
                .stdout,
            config_before.stdout,
            "retained refs must retain their branch configuration"
        );
        let retained = repo.get_commit_sha(&branch);
        if mutation == "advance" {
            assert_eq!(retained, std::fs::read_to_string(advanced_file).unwrap());
            assert_ne!(retained, original);
            assert!(TestRepo::stderr(&out).contains("branch head changed"));
        } else {
            assert_eq!(retained, original);
            assert!(replacement.is_dir());
        }
        assert!(
            TestRepo::stderr(&out).contains("ref retained, retry sync"),
            "{}",
            TestRepo::stderr(&out)
        );
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(json.get("deleted_branches").is_none(), "{json}");
        assert!(resolve_ref(&repo, &format!("refs/branch-metadata/{branch}")).is_some());
        let manifest = std::fs::read_dir(repo.path().join(".git/stax/recovery"))
            .unwrap()
            .flat_map(|op| std::fs::read_dir(op.unwrap().path()).unwrap())
            .map(|entry| entry.unwrap().path().join("manifest.json"))
            .find(|p| p.is_file())
            .unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        assert_eq!(saved["observed_head"], original);
        let bundle = manifest.parent().unwrap().join("branch.bundle");
        let heads = repo.git(&["bundle", "list-heads", bundle.to_str().unwrap()]);
        heads.assert_success();
        assert_eq!(
            TestRepo::stdout(&heads).split_whitespace().next(),
            Some(original.as_str())
        );
        if mutation == "advance" {
            assert!(!TestRepo::stdout(&heads).contains(&retained));
        }
        assert!(
            std::fs::read_to_string(log)
                .unwrap()
                .contains("[\"environment\", \"delete\", \"selected\", \"--json\"]")
        );
    }
}
