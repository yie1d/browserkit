use std::fs;
use std::path::PathBuf;

fn repository_file(path: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(root.join(path))
        .unwrap_or_else(|error| panic!("failed to read repository file '{path}': {error}"))
}

#[test]
fn manifest_uses_the_published_cdpkit_release() {
    let manifest = repository_file("Cargo.toml");

    assert!(
        manifest.contains(r#"cdpkit = "0.6.0""#),
        "browserkit must use the published cdpkit 0.6.0 release"
    );
    assert!(
        !manifest.contains("cdpkit = {"),
        "cdpkit must not use path or git dependency overrides"
    );
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
fn maintained_docs_describe_the_actual_dependency_and_event_policies() {
    let readme = repository_file("README.md");
    assert!(
        !readme.contains("Until cdpkit 0.6.0 is published")
            && !readme.contains("git clone https://github.com/yie1d/cdpkit-rs"),
        "source-build instructions must use the published cdpkit release"
    );

    let redesign = repository_file("docs/REDESIGN.md");
    let roadmap = repository_file("docs/ROADMAP.md");
    for (path, document) in [("docs/REDESIGN.md", redesign), ("docs/ROADMAP.md", roadmap)] {
        assert!(
            document.contains("only `wait networkidle` uses `Bounded(256)`"),
            "{path} must distinguish bounded operations from bounded CDP event channels"
        );
    }
}
