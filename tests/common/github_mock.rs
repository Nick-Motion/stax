//! Shared GitHub / WireMock fixtures for integration tests.
//!
//! Individual suites used to hand-roll near-identical helpers; keeping them
//! here lets a new suite mount the standard fixtures in one call.

use std::fs;
use std::path::Path;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Write `~/.config/stax/config.toml` under `home` with the given API base URL
/// and `[submit] stack_links = "off"`. `extra_toml` is appended as-is (empty
/// string for none).
pub(crate) fn write_stax_config(home: &Path, api_base_url: &str, extra_toml: &str) {
    let config_dir = home.join(".config").join("stax");
    fs::create_dir_all(&config_dir).expect("failed to create test config dir");
    let body = format!(
        "[remote]\napi_base_url = \"{api_base_url}\"\n\n[submit]\nstack_links = \"off\"\n{extra_toml}"
    );
    fs::write(config_dir.join("config.toml"), body).expect("failed to write test config");
}

/// Mock `GET /user` to return the given login as the authenticated user.
pub(crate) async fn mount_current_user(mock_server: &MockServer, login: &str) {
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "login": login,
            "id": 1,
            "node_id": "u1",
            "avatar_url": "https://example.com/avatar",
            "gravatar_id": "",
            "url": format!("https://api.github.com/users/{login}"),
            "html_url": format!("https://github.com/{login}"),
            "followers_url": format!("https://api.github.com/users/{login}/followers"),
            "following_url": format!("https://api.github.com/users/{login}/following"),
            "gists_url": format!("https://api.github.com/users/{login}/gists"),
            "starred_url": format!("https://api.github.com/users/{login}/starred"),
            "subscriptions_url": format!("https://api.github.com/users/{login}/subscriptions"),
            "organizations_url": format!("https://api.github.com/users/{login}/orgs"),
            "repos_url": format!("https://api.github.com/users/{login}/repos"),
            "events_url": format!("https://api.github.com/users/{login}/events"),
            "received_events_url": format!("https://api.github.com/users/{login}/received_events"),
            "type": "User",
            "site_admin": false
        })))
        .mount(mock_server)
        .await;
}

/// Mount the four mocks stax needs to create a PR and refresh it once:
/// list-open, create, list-issue-comments, get-by-number. `number`, `head`
/// (branch), and `base` populate the returned PR body. `owner_head_label`
/// controls the `head.label` field, which callers assert against for fork
/// submits (`contributor:branch`) or same-owner submits (`test-owner:branch`).
#[allow(dead_code)]
pub(crate) async fn mount_pr_create_and_refresh(
    mock_server: &MockServer,
    owner: &str,
    repo: &str,
    number: u64,
    head: &str,
    base: &str,
    owner_head_label: &str,
) {
    let pr_body = serde_json::json!({
        "url": format!("https://api.github.com/repos/{owner}/{repo}/pulls/{number}"),
        "id": number,
        "number": number,
        "state": "open",
        "title": head,
        "body": "",
        "draft": false,
        "head": { "ref": head, "sha": "aaaa", "label": format!("{owner_head_label}:{head}") },
        "base": { "ref": base, "sha": "bbbb" },
        "html_url": format!("https://github.com/{owner}/{repo}/pull/{number}")
    });

    Mock::given(method("GET"))
        .and(path(format!("/repos/{owner}/{repo}/pulls")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path(format!("/repos/{owner}/{repo}/pulls")))
        .respond_with(ResponseTemplate::new(201).set_body_json(pr_body.clone()))
        .mount(mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/{owner}/{repo}/issues/{number}/comments"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/repos/{owner}/{repo}/pulls/{number}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(pr_body))
        .mount(mock_server)
        .await;
}

/// Give a synthetic repository fresh merged-PR evidence for its exact branch
/// head. Preserve the existing parent boundary; all Git traffic stays local.
pub(crate) async fn merged_pr_fixture(
    root: &Path,
    branch: &str,
) -> (MockServer, tempfile::TempDir) {
    use super::OutputAssertions;
    let git = |args: &[&str]| {
        let output = super::hermetic_git_command()
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        output.assert_success();
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let head = git(&["rev-parse", branch]);
    let metadata_ref = format!("refs/branch-metadata/{branch}");
    let mut metadata: serde_json::Value =
        serde_json::from_str(&git(&["show", &metadata_ref])).unwrap();
    metadata["prInfo"] = serde_json::json!({"number": 991, "state": "OPEN"});
    let file = tempfile::NamedTempFile::new().unwrap();
    fs::write(file.path(), metadata.to_string()).unwrap();
    let oid = git(&["hash-object", "-w", file.path().to_str().unwrap()]);
    git(&["update-ref", &metadata_ref, &oid]);
    let remote = git(&["remote", "get-url", "origin"]);
    let url = "https://github.com/test-owner/test-repo.git";
    git(&["remote", "set-url", "origin", url]);
    git(&["remote", "set-url", "--push", "origin", &remote]);
    git(&[
        "config",
        "--local",
        &format!("url.file://{}.insteadOf", remote.trim_end_matches('/')),
        url,
    ]);
    let server = MockServer::start().await;
    let home = tempfile::tempdir().unwrap();
    write_stax_config(home.path(), &server.uri(), "");
    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/pulls/991"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "url":"https://api.github.com/repos/test-owner/test-repo/pulls/991",
            "id":991, "number":991, "state":"closed", "merged":true,
            "merged_at":"2026-01-01T00:00:00Z", "draft":false, "title":"Merged fixture", "body":"",
            "head":{"ref":branch,"sha":head,"label":format!("test-owner:{branch}")},
            "base":{"ref":"main","sha":git(&["rev-parse", "main"])},
            "html_url":"https://github.com/test-owner/test-repo/pull/991"
        })))
        .expect(1..)
        .mount(&server)
        .await;
    (server, home)
}
