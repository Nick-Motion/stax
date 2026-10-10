use crate::common;

use common::{OutputAssertions, TestRepo};
use serde_json::Value;

fn create_missing_parent_stack(repo: &TestRepo) -> (String, String) {
    repo.run_stax(&["bc", "missing-parent"]).assert_success();
    let parent = repo.current_branch();

    repo.run_stax(&["bc", "missing-child"]).assert_success();
    let child = repo.current_branch();

    repo.run_stax(&["t"]).assert_success();
    let delete_parent = repo.git(&["branch", "-D", &parent]);
    assert!(
        delete_parent.status.success(),
        "Failed to delete parent branch: {}",
        TestRepo::stderr(&delete_parent)
    );

    (parent, child)
}

#[test]
fn status_json_reports_missing_parent_branch_once() {
    let repo = TestRepo::new();
    let (parent, child) = create_missing_parent_stack(&repo);

    let output = repo.run_stax(&["status", "--json"]);
    output.assert_success();

    let status: Value =
        serde_json::from_str(&TestRepo::stdout(&output)).expect("status JSON should parse");
    let branches = status["branches"]
        .as_array()
        .expect("status JSON should include branches");
    let child_entries: Vec<_> = branches
        .iter()
        .filter(|entry| entry["name"].as_str() == Some(child.as_str()))
        .collect();

    assert_eq!(
        child_entries.len(),
        1,
        "missing-parent child should appear once in status JSON: {}",
        TestRepo::stdout(&output)
    );
    assert_eq!(child_entries[0]["missing_parent"], parent);
}

#[test]
fn status_labels_missing_parent_instead_of_needs_restack() {
    let repo = TestRepo::new();
    let (parent, child) = create_missing_parent_stack(&repo);

    let output = repo.run_stax(&["status"]);
    output.assert_success();

    let stdout = TestRepo::stdout(&output);
    let child_lines: Vec<_> = stdout
        .lines()
        .filter(|line| line.contains(&child))
        .collect();

    assert_eq!(
        child_lines.len(),
        1,
        "missing-parent child should appear once in status output: {stdout}"
    );
    assert!(
        child_lines[0].contains("missing parent") && child_lines[0].contains(&parent),
        "expected missing-parent label for child branch, got: {}",
        child_lines[0]
    );
    assert!(
        !child_lines[0].contains("needs restack"),
        "missing-parent branch should not be labeled as ordinary restack work: {}",
        child_lines[0]
    );
    assert!(
        stdout.contains("stax fix --yes"),
        "status should point users at metadata repair, got: {stdout}"
    );
    assert!(
        !stdout.contains("stax rs --restack"),
        "missing-parent-only status should not suggest restack, got: {stdout}"
    );
}

#[test]
fn status_json_reuses_line_diff_cache_and_invalidates_moved_tips() {
    let repo = TestRepo::new();
    repo.run_stax(&["bc", "cached-lines"]).assert_success();
    let branch = repo.current_branch();
    repo.create_file("lines.txt", "one\ntwo\n");
    repo.commit("Add two lines");
    let cold = repo.run_stax(&["status", "--json"]);
    cold.assert_success();
    let cold_json: Value = serde_json::from_slice(&cold.stdout).unwrap();
    let entry = cold_json["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == branch)
        .unwrap();
    assert_eq!(entry["lines_added"], 2);
    assert_eq!(entry["lines_deleted"], 0);
    let cache_path = repo.path().join(".git/stax/line-diff-cache.json");
    let cached = std::fs::read(&cache_path).unwrap();
    let cache: Value = serde_json::from_slice(&cached).unwrap();
    assert_eq!(cache["entries"].as_object().unwrap().len(), 1);
    let timestamp = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1);
    std::fs::File::options()
        .write(true)
        .open(&cache_path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(timestamp))
        .unwrap();
    let warm = repo.run_stax(&["status", "--json"]);
    warm.assert_success();
    assert_eq!(
        serde_json::from_slice::<Value>(&warm.stdout).unwrap(),
        cold_json
    );
    assert_eq!(std::fs::read(&cache_path).unwrap(), cached);
    assert_eq!(
        std::fs::metadata(&cache_path).unwrap().modified().unwrap(),
        timestamp
    );

    repo.create_file("lines.txt", "one\ntwo\nthree\n");
    repo.commit("Add another line");
    let moved = repo.run_stax(&["status", "--json"]);
    moved.assert_success();
    let moved_json: Value = serde_json::from_slice(&moved.stdout).unwrap();
    let entry = moved_json["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == branch)
        .unwrap();
    assert_eq!(entry["lines_added"], 3);
    let moved_cache: Value = serde_json::from_slice(&std::fs::read(&cache_path).unwrap()).unwrap();
    assert_eq!(moved_cache["entries"].as_object().unwrap().len(), 1);
    assert_ne!(moved_cache, cache);

    std::fs::remove_file(&cache_path).unwrap();
    std::fs::create_dir(&cache_path).unwrap();
    let failed_save = repo.run_stax(&["status", "--json"]);
    failed_save.assert_success();
    assert_eq!(
        serde_json::from_slice::<Value>(&failed_save.stdout).unwrap(),
        moved_json
    );
}
