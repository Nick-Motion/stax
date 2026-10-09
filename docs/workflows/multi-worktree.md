# Multi-worktree behavior

stax is worktree-aware: when a branch in your stack is checked out in another worktree, stax runs rebase, sync, and metadata operations in the right place automatically.

This page covers repo-wide behavior across linked checkouts. For the `st worktree` command surface, see [Worktrees](../worktrees/index.md). For parallel AI lanes, see [AI worktree lanes](agent-worktrees.md).

## What's worktree-aware

- `st restack` and `st sync --restack` run `git rebase` in the target worktree when needed.
- `st cascade` fast-forwards trunk before restacking, even if trunk is checked out elsewhere.
- `st sync` updates trunk in whichever worktree currently has trunk checked out.
- `st sync` removes a linked worktree only after merge proof and a durable recovery archive. A fresh merged PR must match its exact head; branches without PR linkage may use confirmed Git history. A missing remote branch by itself is not merge proof.
- Metadata (`refs/branch-metadata/*`) is shared across all worktrees automatically.

## Sync cleanup

When a confirmed merged branch is checked out in another linked worktree, sync offers removal first for a clean worktree, followed by keeping it or skipping the branch. Sync uses a real Git worktree removal, so warm-slot recycling does not keep the retired directory around. If the branch is checked out in the primary worktree, or the linked worktree is locked or has an operation/conflicts in progress, sync keeps it and reports why.

Dirty tracked, untracked, and ignored files are copied to a versioned recovery directory under the common Git directory before sync removes the worktree. The manifest records the operation, original path, branch head, patches, and file mapping. Archive files and directories are synced before the worktree can be removed. If saving or immediate revalidation fails, sync keeps the worktree and branch for retry. Ownership is checked again after `pre_remove`. If teardown advances or reuses the branch, sync retains the local ref and reports a retry; the archive still covers only the original manifest head. JSON output includes `recovery_artifacts` when an archive was saved. `st sync --force` skips confirmation; it never authorizes discarding dirty work.

`st sync --dry-run --json --force` reports `would_recover_then_delete` for eligible linked checkouts, including dirty ones; `--force` does not hide the recovery requirement. Plans use the same lifecycle observations as execution and retain protected or uncertain BB targets. Dry-run creates no recovery archives and makes no Git or BB mutations.

Run `stax undo <operation-id>` to restore branch refs and metadata. Undo does not recreate the worktree directory or a BB session. To recover local file changes, use the `staged.patch` and `unstaged.patch` in the recorded directory and copy each entry listed in `manifest.json` from `untracked/` to its original path. For the staged patch, apply it both to the index (`git apply --cached --binary`) and worktree (`git apply --binary`) before applying `unstaged.patch`.

BB-managed worktrees are identified by their exact registered environment path, provider, host, and project remote. Sync archives only idle threads associated with that environment and asks BB to delete it; active work, uncertain ownership, a refusal, or a teardown timeout keeps the branch and recovery archive for retry.

For plain Git worktrees, sync runs `pre_remove` from the verified target worktree. Since the directory is gone when `post_remove` runs, that hook starts in a fresh empty temporary directory, so relative effects cannot modify the primary checkout, and receives the removed target's absolute path in `STAX_REMOVED_WORKTREE_PATH`.

## Dirty worktrees

By default, stax fails fast when a target worktree contains uncommitted changes. Use `--auto-stash-pop` to stash before rebase and restore afterward:

```bash
st restack --auto-stash-pop
st upstack restack --auto-stash-pop
st sync --restack --auto-stash-pop
```

`--stash` and `--no-stash` control the **current** working tree before sync starts, not the linked target worktrees. `--stash` auto-stashes the current tree without prompting (works with `--quiet`/`--json`; does NOT auto-confirm branch deletions); `--no-stash` makes sync fail immediately on a dirty current tree, overriding `--force`. In contrast, `--auto-stash-pop` acts during the restack phase on each **linked target** worktree as branches are rebased. The two mechanisms are orthogonal: you can combine `--stash --restack --auto-stash-pop` to handle both the current tree and any linked worktrees in a single `st rs` invocation.

If a conflict occurs, the stash entry is preserved so nothing is lost. When `st sync` auto-stashes your working tree and fails on an unrecoverable error path (for example, when restoring the stash itself conflicts with an updated trunk), it prints a warning to stderr naming the stash "stax auto-stash" with instructions to run `git stash pop` to restore your changes.

## Related

- [Worktrees (`st wt`)](../worktrees/index.md)
- [AI worktree lanes (`st lane`)](agent-worktrees.md)
