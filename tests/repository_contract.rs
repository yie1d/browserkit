use std::fs;
use std::path::PathBuf;

use browserkit::{BrowserError, BrowserRuntime, BrowserSession, CloseReport, Frame, Page};

fn repository_file(path: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(root.join(path))
        .unwrap_or_else(|error| panic!("failed to read repository file '{path}': {error}"))
        .replace("\r\n", "\n")
}

#[test]
fn manifest_uses_the_published_cdpkit_release() {
    let manifest = repository_file("Cargo.toml");

    assert!(
        manifest.contains(r#"cdpkit = "=0.7.1""#),
        "browserkit must use the exact published cdpkit 0.7.1 release"
    );
    assert!(
        !manifest.contains("cdpkit = {"),
        "cdpkit must not use path or git dependency overrides"
    );
    assert!(
        manifest.contains(r#"version = "0.4.3""#),
        "the cdpkit 0.7.1 public-type migration and runtime SDK require a breaking 0.x version bump"
    );
    assert!(
        manifest.contains(r#"exclude = ["docs/REDESIGN.md"]"#),
        "local REDESIGN notes must not be included in the published crate"
    );

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

    let roadmap = repository_file("docs/ROADMAP.md");
    assert!(
        roadmap.contains("Rust 1.88 checks") && !roadmap.contains("Rust 1.75 checks"),
        "roadmap must match the manifest MSRV"
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
fn maintained_docs_describe_the_actual_dependency_and_event_policies() {
    let readme = repository_file("README.md");
    assert!(
        !readme.contains("Until cdpkit 0.6.0 is published")
            && !readme.contains("git clone https://github.com/yie1d/cdpkit-rs"),
        "source-build instructions must use the published cdpkit release"
    );

    let roadmap = repository_file("docs/ROADMAP.md");
    let normalized = roadmap.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        normalized.contains("Event::subscribe(&sender).await")
            && normalized.contains("target/page-scoped events use `&session`")
            && normalized.contains("browser/connection-scoped events use `&cdp`")
            && normalized.contains("independent unbounded queue")
            && normalized.contains(
                "registration is awaited before enabling its domain or triggering an action"
            ),
        "docs/ROADMAP.md must describe cdpkit 0.7.1 registration, scoped senders, and queue semantics"
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

    let tour = repository_file("docs/architecture-tour.html");
    assert!(
        tour.contains("历史 <code>bk</code> CLI/daemon 的架构")
            && tour.contains("cdpkit 0.7.1")
            && !tour.contains("cdpkit 0.6.0"),
        "the architecture tour must identify its legacy scope and current protocol dependency"
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

    for path in ["README.md", "docs/ROADMAP.md"] {
        let document = repository_file(path);
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
            && frame_runtime.contains(
            "runtime.cdp().observe([\"Target.detachedFromTarget\"]).await?",
        )
            && frame_runtime.contains("graph.route_oopif(")
            && frame_runtime.contains("SetAutoAttach::new(true, false)\n            .with_flatten(true)\n            .send(&main_session)"),
        "Target attach observation must use the parent Page Session, detach observation must use the connection scope, and OOPIF routing must stay isolated to the page frame graph"
    );
    let reducer_start = frame_runtime
        .find("tokio::spawn(async move")
        .expect("frame reducer task");
    let auto_attach = frame_runtime
        .find("SetAutoAttach::new(true, false)")
        .expect("Target auto-attach command");
    assert!(
        reducer_start < auto_attach,
        "the frame reducer must be running before Target auto-attach can emit initial OOPIF events"
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
