use std::fs;
use std::path::PathBuf;
use std::process::Command;

use browserkit::{BrowserError, BrowserRuntime, BrowserSession, CloseReport, Frame, Page};

const RELEASE_FORBIDDEN_PATHS: [&str; 3] = [
    "docs/REDESIGN.md",
    "docs/ROADMAP.md",
    "docs/architecture-tour.html",
];

fn repository_file(path: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(root.join(path))
        .unwrap_or_else(|error| panic!("failed to read repository file '{path}': {error}"))
        .replace("\r\n", "\n")
}

fn dependency_version<'a>(dependency: &'a toml::Value, name: &str) -> Result<&'a str, String> {
    match dependency {
        toml::Value::String(version) => Ok(version),
        toml::Value::Table(table) => table
            .get("version")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| format!("{name} must declare a registry version")),
        _ => Err(format!("{name} must be a string or dependency table")),
    }
}

fn validate_release_manifest(manifest: &str) -> Result<(), String> {
    let manifest = manifest
        .parse::<toml::Value>()
        .map_err(|error| format!("failed to parse Cargo.toml: {error}"))?;
    let root = manifest
        .as_table()
        .ok_or_else(|| "Cargo.toml root must be a table".to_owned())?;
    let package = root
        .get("package")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| "Cargo.toml must contain [package]".to_owned())?;

    if package.get("version").and_then(toml::Value::as_str) != Some("0.4.4") {
        return Err("release metadata must identify browserkit 0.4.4".to_owned());
    }

    let excludes = package
        .get("exclude")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| "package.exclude must be an array".to_owned())?;
    let required_excludes = ["semantic-review/**", "todo/**"];
    if excludes.len() != required_excludes.len()
        || required_excludes.iter().any(|required| {
            !excludes
                .iter()
                .any(|value| value.as_str() == Some(required))
        })
    {
        return Err(format!(
            "package.exclude must contain exactly {required_excludes:?}"
        ));
    }

    let dependencies = root
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| "Cargo.toml must contain [dependencies]".to_owned())?;
    let cdpkit = dependencies
        .get("cdpkit")
        .ok_or_else(|| "Cargo.toml must depend on cdpkit".to_owned())?;
    if dependency_version(cdpkit, "cdpkit")? != "=0.7.2" {
        return Err("cdpkit must use the exact published 0.7.2 release".to_owned());
    }
    if let Some(table) = cdpkit.as_table() {
        for source_override in ["path", "git", "branch", "tag", "rev", "registry"] {
            if table.contains_key(source_override) {
                return Err(format!(
                    "cdpkit must not use the {source_override} dependency override"
                ));
            }
        }
    }

    if let Some(patches) = root.get("patch").and_then(toml::Value::as_table) {
        for source in patches.values().filter_map(toml::Value::as_table) {
            if source.iter().any(|(name, dependency)| {
                name == "cdpkit"
                    || dependency
                        .as_table()
                        .and_then(|table| table.get("package"))
                        .and_then(toml::Value::as_str)
                        == Some("cdpkit")
            }) {
                return Err("cdpkit must not be replaced through [patch]".to_owned());
            }
        }
    }

    let dev_dependencies = root
        .get("dev-dependencies")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| "Cargo.toml must contain [dev-dependencies]".to_owned())?;
    let tokio = dev_dependencies
        .get("tokio")
        .ok_or_else(|| "dev-dependencies must include tokio".to_owned())?;
    if dependency_version(tokio, "dev-dependency tokio")? != "=1.49.0" {
        return Err("the test-util tokio dependency must stay pinned to 1.49.0".to_owned());
    }
    let tokio_features = tokio
        .as_table()
        .and_then(|table| table.get("features"))
        .and_then(toml::Value::as_array)
        .ok_or_else(|| "dev-dependency tokio must declare features".to_owned())?;
    if !tokio_features
        .iter()
        .any(|feature| feature.as_str() == Some("test-util"))
    {
        return Err("dev-dependency tokio must enable test-util".to_owned());
    }

    Ok(())
}

fn manifest_fixture(cdpkit: &str, patch: &str) -> String {
    format!(
        r#"
[package]
name = "browserkit"
version = "0.4.4"
exclude = ["semantic-review/**", "todo/**"]

[dependencies]
{cdpkit}

[dev-dependencies]
tokio = {{ version = "=1.49.0", features = ["test-util"] }}

{patch}
"#
    )
}

#[test]
fn manifest_uses_the_published_cdpkit_release() {
    let manifest = repository_file("Cargo.toml");
    validate_release_manifest(&manifest).unwrap_or_else(|error| panic!("{error}"));

    let changelog = repository_file("CHANGELOG.md");
    for required in [
        "BrowserRuntime",
        "BrowserSession",
        "cdpkit 0.7.1",
        "asynchronous subscription",
        "breaking",
    ] {
        assert!(
            changelog.contains(required),
            "CHANGELOG must document the runtime and cdpkit migration: {required}"
        );
    }
}

#[test]
fn release_manifest_accepts_source_and_normalized_cdpkit_syntax() {
    for cdpkit in [
        r#"cdpkit = "=0.7.2""#,
        "[dependencies.cdpkit]\nversion = \"=0.7.2\"",
    ] {
        validate_release_manifest(&manifest_fixture(cdpkit, ""))
            .unwrap_or_else(|error| panic!("equivalent cdpkit dependency was rejected: {error}"));
    }
}

#[test]
fn release_manifest_rejects_cdpkit_overrides_and_non_exact_versions() {
    for (case, cdpkit, patch) in [
        ("non-exact", r#"cdpkit = "0.7.2""#, ""),
        (
            "path",
            r#"cdpkit = { version = "=0.7.2", path = "../cdpkit" }"#,
            "",
        ),
        (
            "git",
            r#"cdpkit = { version = "=0.7.2", git = "https://example.invalid/cdpkit" }"#,
            "",
        ),
        (
            "patch",
            r#"cdpkit = "=0.7.2""#,
            r#"[patch.crates-io]
cdpkit = { git = "https://example.invalid/cdpkit" }"#,
        ),
    ] {
        assert!(
            validate_release_manifest(&manifest_fixture(cdpkit, patch)).is_err(),
            "{case} cdpkit dependency must be rejected"
        );
    }
}

#[test]
fn release_manifest_requires_the_minimal_internal_excludes() {
    let manifest = manifest_fixture(r#"cdpkit = "=0.7.2""#, "");
    let incomplete_manifests = [
        manifest.replace(r#""semantic-review/**", "#, ""),
        manifest.replace(r#", "todo/**""#, ""),
    ];
    for incomplete in incomplete_manifests {
        assert!(
            validate_release_manifest(&incomplete).is_err(),
            "every required package exclude must be enforced"
        );
    }
    let overly_broad = manifest.replace(r#""todo/**"]"#, r#""todo/**", "tests/**"]"#);
    assert!(
        validate_release_manifest(&overly_broad).is_err(),
        "unreviewed broad package excludes must be rejected"
    );
}

#[test]
fn release_forbidden_paths_are_ignored_and_untracked() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ignore = repository_file(".gitignore");
    let ignore_lines = ignore.lines().collect::<Vec<_>>();
    for path in RELEASE_FORBIDDEN_PATHS {
        assert!(
            ignore_lines.contains(&format!("/{path}").as_str()),
            "{path} must have an exact root-relative .gitignore entry"
        );
    }

    let output = Command::new("git")
        .args(["ls-files", "--"])
        .args(RELEASE_FORBIDDEN_PATHS)
        .current_dir(root)
        .output()
        .expect("git must be available for repository contract checks");
    assert!(output.status.success(), "git ls-files failed");
    assert!(
        output.stdout.is_empty(),
        "release-forbidden paths must not be tracked:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn release_preflight_rejects_forbidden_tracked_and_packaged_paths() {
    let workflow = repository_file(".github/workflows/release.yml");
    assert!(workflow.contains("git ls-files --error-unmatch -- \"$forbidden\""));
    assert!(workflow.contains("grep -Fx \"browserkit-$VERSION/$forbidden\""));
    for path in RELEASE_FORBIDDEN_PATHS {
        assert!(
            workflow.contains(&format!("\"{path}\"")),
            "release preflight must cover {path}"
        );
    }
}

#[test]
fn workflows_resolve_cdpkit_from_the_lockfile() {
    for path in [".github/workflows/ci.yml", ".github/workflows/release.yml"] {
        let workflow = repository_file(path);

        assert!(
            !workflow.contains("yie1d/cdpkit-rs")
                && !workflow.contains("path: cdpkit-rs")
                && !workflow.contains("working-directory: browserkit"),
            "{path} must use the registry dependency from Cargo.lock"
        );
    }
}

#[test]
fn release_and_ci_enforce_the_supported_toolchain_and_strict_audit() {
    let manifest = repository_file("Cargo.toml");
    assert!(
        manifest.contains(r#"rust-version = "1.88""#),
        "Cargo.toml must declare Rust 1.88 as the MSRV"
    );

    let readme = repository_file("README.md");
    assert!(
        readme.contains("Rust 1.88+"),
        "README requirements must match the manifest MSRV"
    );

    for path in [".github/workflows/ci.yml", ".github/workflows/release.yml"] {
        let workflow = repository_file(path);
        assert!(
            workflow.contains("toolchain: 1.88.0"),
            "{path} must validate the declared MSRV"
        );
        assert!(
            workflow.contains("cargo install cargo-audit --version 0.22.2 --locked"),
            "{path} must install the reviewed cargo-audit version"
        );
        assert!(
            workflow.contains("cargo audit --deny warnings"),
            "{path} must reject vulnerabilities and informational warnings"
        );
        assert!(
            !workflow.contains("1.75"),
            "{path} must not retain the superseded MSRV"
        );
    }
}

#[test]
fn release_publishes_and_verifies_the_sdk_before_github_assets() {
    let changelog = repository_file("CHANGELOG.md");
    assert!(
        changelog.contains("## [0.4.0] - 2026-08-19"),
        "the release changelog must contain the dated 0.4.0 section"
    );

    let workflow = repository_file(".github/workflows/release.yml");
    for required in [
        "concurrency:",
        "publish:",
        "id-token: write",
        "rust-lang/crates-io-auth-action@c6f97d42243bad5fab37ca0427f495c86d5b1a18",
        "cargo package --locked",
        "cargo publish --locked",
        "needs: [validate, publish]",
        ".cargo_vcs_info.json",
        "sha256sum --check --strict",
        "git merge-base --is-ancestor",
        "LOCAL_CHECKSUM",
        "SHA256SUMS",
    ] {
        assert!(
            workflow.contains(required),
            "release workflow must preserve SDK publication contract: {required}"
        );
    }
    assert!(
        !workflow.contains("git fetch --force"),
        "release workflow must never rewrite the release tag during provenance checks"
    );
    assert!(
        workflow.matches("--user-agent \"browserkit-release/").count() >= 3,
        "every crates.io API and crate download request must identify the browserkit release client"
    );
    assert!(
        workflow.contains("--silent --show-error --output /dev/null --write-out '%{http_code}'"),
        "the pre-publish existence check must not dirty the release checkout"
    );
    assert!(
        !workflow.contains("] - 2026-08-19\" { capture"),
        "release note extraction must not hard-code one release date"
    );
}

#[test]
fn maintained_docs_use_the_published_dependency() {
    let readme = repository_file("README.md");
    assert!(
        !readme.contains("Until cdpkit 0.6.0 is published")
            && !readme.contains("git clone https://github.com/yie1d/cdpkit-rs"),
        "source-build instructions must use the published cdpkit release"
    );
}

#[test]
fn maintained_docs_separate_the_sdk_from_the_historical_bk_runtime() {
    let connect_guide = repository_file("docs/connect-existing-chrome.md");
    assert!(
        connect_guide.contains("historical `bk` CLI and daemon attach workflow")
            && connect_guide.contains("`BrowserRuntime::launch`"),
        "the existing-browser guide must scope attach-only behavior to bk and acknowledge SDK launch"
    );

    let commands = repository_file("docs/bk-browser/references/commands.md");
    assert!(
        commands.contains("historical `bk` executable and daemon"),
        "the command reference must not describe bk process limits as product-wide SDK limits"
    );

    let skill = repository_file("docs/bk-browser/SKILL.md");
    assert!(skill.contains("version: 0.4.0"));
    assert!(
        skill.contains("运行时命令结果为 JSON")
            && skill.contains("shell completions 输出文本")
            && !skill.contains("输出永远 JSON"),
        "the bundled skill must distinguish runtime JSON from textual help output"
    );
}

#[test]
fn runtime_sdk_contract() {
    fn assert_public_runtime_exports(
        _: Option<(
            BrowserRuntime,
            BrowserSession,
            Page,
            Frame,
            CloseReport,
            BrowserError,
        )>,
    ) {
    }
    assert_public_runtime_exports(None);

    let runtime_module = repository_file("src/runtime/mod.rs");
    for export in [
        "pub use browser::*;",
        "pub use error::*;",
        "pub use frame::*;",
        "pub use page::*;",
        "pub use session::*;",
    ] {
        assert!(
            runtime_module.contains(export),
            "runtime module must publicly re-export {export}"
        );
    }

    for path in ["examples/runtime_connect.rs", "examples/runtime_launch.rs"] {
        let example = repository_file(path);
        assert!(
            example.contains("BrowserRuntime"),
            "{path} must demonstrate the public BrowserRuntime SDK"
        );
        assert!(
            example.contains("CloseReport"),
            "{path} must check explicit close reports"
        );
    }

    for path in ["README.md"] {
        let document = repository_file(path);
        for current_capability in [
            "locators",
            "navigation",
            "network",
            "downloads",
            "storage",
            "snapshots",
            "diagnostics",
            "typed event streams",
        ] {
            assert!(
                document.to_ascii_lowercase().contains(current_capability),
                "{path} must describe the current Runtime capability: {current_capability}"
            );
        }
        assert!(
            !document.contains("remain outside the current lifecycle SDK phase")
                && !document.contains("are later SDK phases"),
            "{path} must not describe implemented Runtime APIs as future work"
        );
        for escape_hatch in ["runtime.cdp()", "page.cdp_session()", "frame.cdp_session()"] {
            assert!(
                document.contains(escape_hatch),
                "{path} must document {escape_hatch}"
            );
        }
        let lower = document.to_ascii_lowercase();
        for forbidden in [
            "capture", "record", "replay", "agent", "ring", "cursor", "watch_",
        ] {
            let found = if forbidden == "watch_" {
                lower.contains(forbidden)
            } else {
                lower
                    .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                    .any(|word| word == forbidden)
            };
            assert!(
                !found,
                "{path} must keep {forbidden} out of the client-neutral runtime SDK"
            );
        }
    }
}

#[test]
fn frame_target_lifecycle_observation_matches_cdp_delivery_scope() {
    let frame_runtime = repository_file("src/runtime/frame.rs");
    assert!(
        frame_runtime.contains(
            "session.observe([\"Target.attachedToTarget\"]).await?",
        ) && frame_runtime.contains("spawn_target_attach_reducer")
            && frame_runtime.contains("drain_initial_attached_targets")
            && frame_runtime.contains(
            "runtime.cdp().observe([\"Target.detachedFromTarget\"]).await?",
        )
            && frame_runtime.contains("graph.route_oopif(")
            && frame_runtime.contains("SetAutoAttach::new(true, configure_every_route)")
            && frame_runtime.contains("has_every_route_configuration"),
        "Target attach observation must use each parent Session, initial configured OOPIFs must drain before attach returns, detach observation must use connection scope, and routing must stay page-local"
    );
    let prepare = frame_runtime
        .find("Self::prepare_frame_session(&main_session)")
        .expect("main route preparation");
    let auto_attach = frame_runtime
        .find("SetAutoAttach::new(true, configure_every_route)")
        .expect("configuration-aware Target auto-attach command");
    let initial_drain = frame_runtime
        .find("Self::drain_initial_attached_targets(&store, &mut main_target_attached)")
        .expect("initial attached-target drain");
    assert!(
        prepare < auto_attach && auto_attach < initial_drain,
        "subscriptions must precede auto-attach and every configured initial target must drain after its response"
    );
}

#[test]
fn frame_store_does_not_retain_a_strong_page_handle() {
    let frame_runtime = repository_file("src/runtime/frame.rs");
    assert!(
        frame_runtime.contains("page: Weak<PageInner>"),
        "FrameStore must not form a PageInner -> FrameStore -> PageInner strong-reference cycle"
    );
    assert!(
        frame_runtime.contains("state: RwLock<FrameState>")
            && !frame_runtime.contains("graph: RwLock<FrameGraph>")
            && !frame_runtime.contains("sessions: RwLock<HashMap")
            && frame_runtime.contains("struct ChildSessionOwnership")
            && frame_runtime.contains("collect_session_subtree")
            && frame_runtime.contains("reroute_session"),
        "frame routes and their CDP Session handles must be updated under one state lock"
    );
    assert!(
        frame_runtime.contains("impl Drop for FrameStore")
            && frame_runtime.contains("Self::prepare_frame_session(&session)")
            && frame_runtime.contains(".send(&session)"),
        "FrameStore must cancel reducers on drop and initialize each OOPIF Session recursively"
    );
}

#[test]
fn runtime_preflight_contracts_preserve_typed_truth_and_ownership_ordering() {
    let session = repository_file("src/runtime/session.rs");
    let new_page = &session[session
        .find("pub async fn new_page")
        .expect("new_page implementation")..];
    let creation = new_page
        .find("PageCreationTransaction::new")
        .expect("new_page creation transaction");
    let route_prepare = new_page
        .find("super::route::prepare_main_route")
        .expect("synchronous route preparation");
    let route_install = new_page
        .find("creation.install_route(rollback)")
        .expect("route ownership transfer into creation transaction");
    let route_apply = new_page
        .find("super::route::apply_main_route")
        .expect("asynchronous route application");
    let navigation_commit = new_page
        .find("super::navigation::commit_page_creation_navigation")
        .expect("new_page navigation commit fence");
    let ownership_handoff = new_page
        .find("creation.finish_success(self, page)")
        .expect("new_page ownership handoff");
    assert!(
        creation < route_prepare
            && route_prepare < route_install
            && route_install < route_apply
            && route_apply < navigation_commit
            && navigation_commit < ownership_handoff,
        "the creation transaction must own admission and target before route configuration can await"
    );

    let success_handoff = &session[session
        .find("fn finish_success")
        .expect("page creation success handoff")..];
    let retain_route = success_handoff
        .find("route.retain();")
        .expect("retained route ownership");
    let retain_target = success_handoff
        .find("page.retain_owned_target(target.retain())")
        .expect("default target ownership transfer");
    let publish = success_handoff
        .find("session.publish_page")
        .expect("new_page publication");
    let release = success_handoff
        .find("drop(admission);")
        .expect("creation admission release");
    assert!(
        retain_route < publish && retain_target < publish && publish < release,
        "route/target ownership and publication must become close-visible before admission is released"
    );

    let navigation = repository_file("src/runtime/navigation.rs");
    let creation_commit = &navigation[navigation
        .find("pub(super) async fn commit_page_creation_navigation")
        .expect("new_page navigation commit helper")..];
    assert!(
        creation_commit.contains("validate_navigation_response(")
            && creation_commit.contains("ActionCompletion::Completed"),
        "new_page navigation commit must reject acknowledged navigation failures as completed"
    );

    let artifact = repository_file("src/runtime/artifact.rs");
    assert!(
        artifact.contains("page.capabilities().status(super::Capability::Pdf)")
            && artifact.contains("ConfigurationFailure::UnsupportedCapability"),
        "PDF preflight must use the owning session capability snapshot and preserve its typed reason"
    );

    let launch = repository_file("src/runtime/launch.rs");
    let validate = launch
        .find("validate_raw_arguments(&options)")
        .expect("raw launch argument preflight");
    let discovery = launch
        .find("BrowserFinder::find()")
        .expect("browser discovery");
    assert!(
        validate < discovery
            && launch.contains("--headless")
            && launch.contains("--proxy-pac-url")
            && launch.contains("--proxy-auto-detect"),
        "typed launch truth must be validated before browser discovery or spawn"
    );
}
