// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))] // miri can't sandbox FS ops these tests do (TempDir, assert_cmd, etc.)
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

//! Snapshot tests for the full emitted file tree.
//!
//! For a small set of representative input combinations, run
//! `cargo anvil` against a bare-workspace tempdir, collect
//! every file anvil produced (sorted by path), and snapshot the
//! whole tree as one string. Snapshots live under `tests/snapshots/`
//! and are reviewed via `cargo insta review`.
//!
//! Coverage rationale: the imperative tests in `src/run.rs` pin the
//! algorithm (which decisions are taken, which paths exist); these
//! snapshot tests pin the *byte-exact emitted content* so template
//! edits surface as reviewable diffs. The two layers are
//! complementary — neither subsumes the other.

#![expect(clippy::unwrap_used, reason = "integration tests favor concise assertions over Result plumbing")]

use std::path::{Path, PathBuf};

use cargo_anvil::test_support::{Cli, MANIFEST_FILE_NAME, run_update};
use tempfile::TempDir;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// Bare workspace fixture: one root manifest + one member crate, with
/// nothing else in the tree. Everything anvil produces is therefore
/// strictly its own output.
fn bare_workspace() -> TempDir {
    let tmp = TempDir::new().unwrap();
    populate_bare_workspace(tmp.path());
    tmp
}

/// Write the bare-workspace fixture into `root`.
fn populate_bare_workspace(root: &Path) {
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n",
    );
    write(
        &root.join("crates/alpha/Cargo.toml"),
        "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(&root.join("crates/alpha/src/lib.rs"), "");
}

/// A bare workspace rooted at a *fixed-name* subdirectory of a tempdir.
///
/// Container rendering embeds the repo directory name (cache-volume prefix,
/// workdir), so snapshots must not depend on the tempdir's random name. The
/// caller passes the returned repo root as the anvil start dir.
fn named_workspace(name: &str) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join(name);
    std::fs::create_dir_all(&root).unwrap();
    populate_bare_workspace(&root);
    (tmp, root)
}

/// Walk the workspace, collect every file produced or modified by
/// anvil, and render them into a single deterministic string.
///
/// The manifest (`.anvil.lock`) is filtered out: it carries the
/// `rendered_by` version which would churn on every crate-version bump,
/// drowning the actual content review in noise. The schema-validation
/// test suite already asserts the manifest is valid TOML.
fn render_tree(root: &Path) -> String {
    render_tree_excluding(root, "")
}

/// Like [`render_tree`] but additionally omits any file whose name equals
/// `extra` (e.g. the user-authored `anvil.toml`, which is not a generated
/// artifact and would otherwise be the sole diff between two trees).
fn render_tree_excluding(root: &Path, extra: &str) -> String {
    let mut paths: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str());
            name != Some(MANIFEST_FILE_NAME) && name != Some(extra)
        })
        .collect();
    paths.sort();

    let mut out = String::new();
    for path in paths {
        let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        let body = std::fs::read_to_string(&path).unwrap();
        out.push_str("=== ");
        out.push_str(&rel);
        out.push_str(" ===\n");
        out.push_str(&body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

fn run(args: &Cli, tmp: &TempDir) {
    run_update(&cargo_anvil::Catalog::anvil(), args, tmp.path()).unwrap();
}

/// The stock public catalog. Container support now ships in the base
/// `Catalog::anvil()` (container-gated), so these tests exercise the real
/// public catalog directly — no fork and no `with_container_artifact` call is
/// needed to opt in, exactly as a public `cargo-anvil` user experiences it.
fn container_catalog() -> cargo_anvil::Catalog {
    cargo_anvil::Catalog::anvil()
}

/// A `Cli` selecting the GitHub backend and applying (not dry-run).
fn github_apply() -> Cli {
    Cli {
        backends: vec!["github".to_owned()],
        no_backends: false,
        dry_run: false,
        force: false,
    }
}

fn ado_apply() -> Cli {
    Cli {
        backends: vec!["ado".to_owned()],
        no_backends: false,
        dry_run: false,
        force: false,
    }
}

#[test]
fn local_only_tree() {
    let tmp = bare_workspace();
    run(
        &Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        },
        &tmp,
    );
    insta::assert_snapshot!("local_only", render_tree(tmp.path()));
}

#[test]
fn github_backend_tree() {
    let tmp = bare_workspace();
    run(
        &Cli {
            backends: vec!["github".to_owned()],
            no_backends: false,
            dry_run: false,
            force: false,
        },
        &tmp,
    );
    insta::assert_snapshot!("github_backend", render_tree(tmp.path()));
}

#[test]
fn ado_backend_tree() {
    let tmp = bare_workspace();
    run(
        &Cli {
            backends: vec!["ado".to_owned()],
            no_backends: false,
            dry_run: false,
            force: false,
        },
        &tmp,
    );
    insta::assert_snapshot!("ado_backend", render_tree(tmp.path()));
}

/// A `[container] enabled = true` anvil.toml (with `devcontainer = true`) must
/// emit the container shim, the devcontainer descriptor, the optional import
/// in `mod.just`, and the re-entry guard on every tier/group recipe. Snapshot
/// the whole tree so those transforms are reviewable byte-for-byte.
#[test]
fn container_enabled_tree() {
    let (_tmp, root) = named_workspace("repo");
    write(
        &root.join("anvil.toml"),
        "[container]\n\
         enabled = true\n\
         image = \"ghcr.io/acme/rust-dev:1.2.3\"\n\
         engine = \"auto\"\n\
         cache-volumes = [\"cargo\", \"rustup\", \"target\"]\n\
         forward-env = [\"CARGO_*\", \"RUST*\"]\n\
         devcontainer = true\n",
    );
    run_update(&container_catalog(), &github_apply(), &root).unwrap();
    insta::assert_snapshot!("container_enabled", render_tree(&root));
}

/// Container gating: enabling emits the shim + devcontainer; the default
/// (absent config) emits neither, and the tier/group bodies stay untouched.
#[test]
fn container_gate_controls_emission() {
    // Enabled: shim + devcontainer present, mod.just imports the shim,
    // tiers.just carries the re-entry guard.
    let (_tmp, root) = named_workspace("repo");
    write(
        &root.join("anvil.toml"),
        "[container]\nenabled = true\nimage = \"img:1\"\ndevcontainer = true\n",
    );
    run_update(&container_catalog(), &github_apply(), &root).unwrap();
    assert!(root.join("justfiles/anvil/container.just").is_file());
    assert!(root.join(".devcontainer/devcontainer.json").is_file());
    let mod_just = std::fs::read_to_string(root.join("justfiles/anvil/mod.just")).unwrap();
    assert!(mod_just.contains("import? 'container.just'"));
    let tiers = std::fs::read_to_string(root.join("justfiles/anvil/tiers.just")).unwrap();
    assert!(tiers.contains("just _anvil-container-run anvil-pr"));

    // Disabled (absent anvil.toml): neither container file appears.
    let (_tmp2, root2) = named_workspace("repo");
    run_update(&container_catalog(), &github_apply(), &root2).unwrap();
    assert!(!root2.join("justfiles/anvil/container.just").exists());
    assert!(!root2.join(".devcontainer/devcontainer.json").exists());
    let mod_just2 = std::fs::read_to_string(root2.join("justfiles/anvil/mod.just")).unwrap();
    assert!(!mod_just2.contains("container.just"));
}

/// `devcontainer` defaults to false: enabling the container without the flag
/// emits the shim but not the descriptor.
#[test]
fn devcontainer_requires_explicit_flag() {
    let (_tmp, root) = named_workspace("repo");
    write(&root.join("anvil.toml"), "[container]\nenabled = true\nimage = \"img:1\"\n");
    run_update(&container_catalog(), &github_apply(), &root).unwrap();
    assert!(root.join("justfiles/anvil/container.just").is_file());
    assert!(!root.join(".devcontainer/devcontainer.json").exists());
}

/// The default exec image: with neither `image` nor `dockerfile` set, anvil
/// must supply the Dockerfile itself, so a repository can adopt container
/// execution without owning any image plumbing. This is what "self-sustained"
/// means in practice — the bottom rung of the ladder exists.
#[test]
fn default_exec_image_is_generated_and_built() {
    let (_tmp, root) = named_workspace("repo");
    write(&root.join("anvil.toml"), "[container]\nenabled = true\n");
    run_update(&container_catalog(), &github_apply(), &root).unwrap();

    let dockerfile = root.join(".anvil/container/Dockerfile");
    assert!(dockerfile.is_file(), "anvil must generate its own exec Dockerfile");
    assert!(root.join(".anvil/container/Dockerfile.dockerignore").is_file());

    // Tools come from the generated catalog, not a second hand-kept list.
    let body = std::fs::read_to_string(&dockerfile).unwrap();
    assert!(body.contains("just anvil-setup"), "the image installs the pinned catalog");
    assert!(
        body.contains("@sha256:"),
        "the base image must be digest-pinned or the content hash means nothing"
    );

    let shim = std::fs::read_to_string(root.join("justfiles/anvil/container.just")).unwrap();
    assert!(shim.contains(r#"anvil_container_image_source := "build""#));
    assert!(shim.contains(r#"anvil_container_dockerfile := ".anvil/container/Dockerfile""#));
    assert!(shim.contains(r#"anvil_container_image_name := "anvil-repo""#));
}

/// A locally-built image exists only on the machine that built it, so a CI
/// job-level container cannot reference it — the runner resolves that before
/// the job starts and has no way to run the generated recipes. Injecting it
/// anyway spliced an empty reference: rejected outright by ADO, and silently
/// non-containerized on GitHub, which is worse than an error.
#[test]
fn build_mode_does_not_inject_a_ci_container() {
    let (_tmp, root) = named_workspace("repo");
    write(&root.join("anvil.toml"), "[container]\nenabled = true\n");
    run_update(&container_catalog(), &ado_apply(), &root).unwrap();

    let job = std::fs::read_to_string(root.join(".pipelines/anvil/steps/job.yml")).unwrap();
    assert!(!job.contains("container:"), "no job container without a resolvable image");
    for name in ["anvil-pr.yml", "anvil-scheduled.yml"] {
        let pipeline = std::fs::read_to_string(root.join(".pipelines").join(name)).unwrap();
        assert!(
            !pipeline.contains("image: \"\""),
            "{name} must never declare an empty container image"
        );
    }

    let (_tmp2, root2) = named_workspace("repo");
    write(&root2.join("anvil.toml"), "[container]\nenabled = true\n");
    run_update(&container_catalog(), &github_apply(), &root2).unwrap();
    for name in ["anvil-pr.yml", "anvil-scheduled.yml"] {
        let workflow = std::fs::read_to_string(root2.join(".github/workflows").join(name)).unwrap();
        assert!(
            !workflow.contains("container_image:"),
            "{name} must not claim a container it cannot obtain"
        );
    }
}

/// The editor resolves the devcontainer descriptor itself and cannot call the
/// generated recipes, so a locally-built image has to be expressed as a build
/// rather than as a reference that does not exist yet.
#[test]
fn devcontainer_describes_a_build_when_the_image_is_built() {
    let (_tmp, root) = named_workspace("repo");
    write(
        &root.join("anvil.toml"),
        "[container]\nenabled = true\ndevcontainer = true\nbuild-args = { RUST_CHANNEL = \"1.93\" }\n",
    );
    run_update(&container_catalog(), &github_apply(), &root).unwrap();

    let descriptor = std::fs::read_to_string(root.join(".devcontainer/devcontainer.json")).unwrap();
    assert!(!descriptor.contains(r#""image": """#), "an empty image is an invalid descriptor");
    // Relative to this file, which lives in `.devcontainer/` -- a repo-relative
    // path would resolve to `.devcontainer/.anvil/...` and name nothing.
    assert!(descriptor.contains(r#""dockerfile": "../.anvil/container/Dockerfile""#));
    assert!(
        root.join(".devcontainer")
            .join("../.anvil/container/Dockerfile")
            .canonicalize()
            .is_ok(),
        "the descriptor's dockerfile path must resolve from .devcontainer/"
    );
    assert!(descriptor.contains(r#""RUST_CHANNEL": "1.93""#), "build args reach the descriptor");
    // Cheap well-formedness guard: the substitution splices a JSON fragment,
    // so an unbalanced brace would be the likely failure mode.
    assert_eq!(
        descriptor.matches('{').count(),
        descriptor.matches('}').count(),
        "descriptor braces must balance: {descriptor}"
    );
}

/// The identity inputs must cover everything that changes the image. The
/// image installs its tools by running `just anvil-setup`, whose dependency
/// chain reaches the tier, group, check and tool recipes — so the recipe tree
/// is hashed too, at run time. Only the files that drive the container from
/// the host are held back, and excluding `container.just` is load-bearing: it
/// is the file that computes the hash.
#[test]
fn exec_image_identity_covers_what_changes_the_image() {
    let (_tmp, root) = named_workspace("repo");
    write(&root.join("anvil.toml"), "[container]\nenabled = true\n");
    run_update(&container_catalog(), &github_apply(), &root).unwrap();

    let shim = std::fs::read_to_string(root.join("justfiles/anvil/container.just")).unwrap();
    let declared = shim
        .lines()
        .find(|l| l.contains("$hashInputs ="))
        .expect("the shim declares its literal hash inputs");

    for expected in [
        ".anvil/container/Dockerfile",
        ".anvil/container/Dockerfile.dockerignore",
        "rust-toolchain.toml",
    ] {
        assert!(declared.contains(expected), "{expected} must define image identity: {declared}");
    }

    // The recipe tree is added by a run-time sweep, minus an explicit denylist.
    assert!(
        shim.contains("Get-ChildItem -LiteralPath $recipeRoot -Recurse -File -Filter '*.just'"),
        "the recipe tree must participate in image identity"
    );
    let excluded = shim
        .lines()
        .find(|l| l.contains("$excluded ="))
        .expect("the shim declares its exclusions");
    assert!(
        excluded.contains("justfiles/anvil/container.just"),
        "the driver must be excluded or the tag would depend on itself: {excluded}"
    );
}

/// The cluster and image-build recipes cannot change what `anvil-setup`
/// installs, so editing them must not force a multi-minute image rebuild.
#[test]
fn cluster_recipes_are_excluded_from_exec_image_identity() {
    let (_tmp, root) = named_workspace("repo");
    write(&root.join("anvil.toml"), image_cluster_toml());
    run_update(&container_catalog(), &github_apply(), &root).unwrap();

    let shim = std::fs::read_to_string(root.join("justfiles/anvil/container.just")).unwrap();
    let excluded = shim.lines().find(|l| l.contains("$excluded =")).unwrap();
    for path in [
        "justfiles/anvil/container.just",
        "justfiles/anvil/container-images.just",
        "justfiles/anvil/cluster.just",
        "justfiles/anvil/cluster-bootstrap.just",
    ] {
        assert!(excluded.contains(path), "{path} must not define image identity: {excluded}");
    }
}

/// A credential must never reach the engine's command line. `-e NAME=VALUE`
/// puts it in host process argv, where endpoint telemetry records and retains
/// it far beyond the life of a short-lived token; `-e NAME` makes the engine
/// copy the value from the environment it already inherits.
#[test]
fn forwarded_env_never_carries_values_on_the_command_line() {
    let (_tmp, root) = named_workspace("repo");
    write(
        &root.join("anvil.toml"),
        "[container]\nenabled = true\nimage = \"img:1\"\nforward-env = [\"MSRUSTUP_*\"]\n",
    );
    run_update(&container_catalog(), &github_apply(), &root).unwrap();

    let shim = std::fs::read_to_string(root.join("justfiles/anvil/container.just")).unwrap();
    assert!(
        shim.contains("$runArgs += @('-e', $entry.Name)"),
        "the name alone must be forwarded"
    );
    assert!(
        !shim.contains("$($entry.Name)=$($entry.Value)"),
        "a forwarded value must never be built into an engine argument"
    );
    // Globs have no deny-list, so the developer needs to see what crossed.
    assert!(shim.contains("anvil: forwarding env:"), "matched names are reported");
    assert!(!shim.contains("$($entry.Value)"), "values must never be printed either");
}

/// BuildKit accepts `--secret id=x,env=UNSET` and mounts an empty file, so a
/// Dockerfile without `required=true` silently builds a degraded image — and
/// tags it with the same content hash a credentialed build would produce,
/// because secret values are deliberately not hashed. The shim has to refuse.
#[test]
fn declared_build_secrets_are_checked_before_the_build() {
    let (_tmp, root) = named_workspace("repo");
    write(
        &root.join("anvil.toml"),
        "[container]\nenabled = true\ndockerfile = \"ci/D\"\n\
         build-secrets = [\"id=tok,env=FEED_TOKEN\"]\n",
    );
    run_update(&container_catalog(), &github_apply(), &root).unwrap();

    let shim = std::fs::read_to_string(root.join("justfiles/anvil/container.just")).unwrap();
    assert!(shim.contains("is unset or empty"), "an env-sourced secret is checked");
    assert!(shim.contains("is missing or empty"), "a file-sourced secret is checked");
    // The check must precede the build, not follow it.
    let check = shim.find("unset or empty").expect("check present");
    let build = shim.find("'build', '--platform'").expect("build present");
    assert!(check < build, "the secret check must run before the engine is invoked");
}

/// A repository that brings its own Dockerfile keeps full control of the
/// image, and its extra build inputs participate in the identity hash. Anvil
/// must not emit its own Dockerfile in that case: a generated file nothing
/// reads is an invitation to edit the wrong one.
#[test]
fn repo_dockerfile_replaces_the_generated_one() {
    let (_tmp, root) = named_workspace("repo");
    write(
        &root.join("anvil.toml"),
        "[container]\n\
         enabled = true\n\
         dockerfile = \"ci/anvil.Dockerfile\"\n\
         build-args = { RUST_CHANNEL = \"ms-prod-1.95\" }\n\
         build-secrets = [\"id=tok,env=TOK\"]\n\
         hash-inputs = [\"ci/install-tools.sh\"]\n",
    );
    run_update(&container_catalog(), &github_apply(), &root).unwrap();

    assert!(!root.join(".anvil/container/Dockerfile").exists());

    let shim = std::fs::read_to_string(root.join("justfiles/anvil/container.just")).unwrap();
    assert!(shim.contains(r#"anvil_container_dockerfile := "ci/anvil.Dockerfile""#));
    assert!(shim.contains(r#"$userBuildArgs = @('RUST_CHANNEL=ms-prod-1.95')"#));
    assert!(shim.contains(r#"$userBuildSecrets = @('id=tok,env=TOK')"#));
    assert!(shim.contains("ci/install-tools.sh"), "declared hash inputs reach the shim");
    // A secret's value must never influence a tag.
    let hash_line = shim.lines().find(|l| l.contains("$hashInputs =")).unwrap();
    assert!(!hash_line.contains("TOK"), "build secrets must stay out of the identity");
}

/// Extending keeps anvil's Dockerfile as the base and layers the repository's
/// on top. The point is that the expensive half stays cached: the repository's
/// own build inputs describe the layer it owns, so a change there must not
/// touch the base's identity.
#[test]
fn extends_layers_on_anvils_image() {
    let (_tmp, root) = named_workspace("repo");
    write(
        &root.join("anvil.toml"),
        "[container]\n\
         enabled = true\n\
         extends = \"ci/substrate.Dockerfile\"\n\
         build-args = { FEED = \"internal\" }\n\
         build-secrets = [\"id=tok,env=TOK\"]\n\
         hash-inputs = [\"ci/install-tools.sh\"]\n",
    );
    run_update(&container_catalog(), &github_apply(), &root).unwrap();

    // The base is anvil's, so it is still generated -- unlike `dockerfile`,
    // which replaces it.
    assert!(root.join(".anvil/container/Dockerfile").is_file());

    let shim = std::fs::read_to_string(root.join("justfiles/anvil/container.just")).unwrap();
    assert!(shim.contains(r#"anvil_container_image_source := "extend""#));
    assert!(shim.contains(r#"anvil_container_dockerfile := ".anvil/container/Dockerfile""#));
    assert!(shim.contains(r#"anvil_container_ext_dockerfile := "ci/substrate.Dockerfile""#));
    assert!(shim.contains(r#"anvil_container_ext_image_name := "anvil-repo-ext""#));

    // The base reference is injected, because its content tag is not knowable
    // until it is resolved.
    assert!(shim.contains("ANVIL_BASE_IMAGE=$image"));

    // The repository's inputs belong to the extension, not the base.
    let base_inputs = shim.lines().find(|l| l.contains("$hashInputs =")).unwrap();
    assert!(
        !base_inputs.contains("ci/install-tools.sh") && !base_inputs.contains("substrate"),
        "the base must not depend on the extension's inputs: {base_inputs}"
    );
    let ext_inputs = shim.lines().find(|l| l.contains("-Inputs @(")).unwrap();
    assert!(ext_inputs.contains("ci/substrate.Dockerfile"));
    assert!(ext_inputs.contains("ci/install-tools.sh"));
}

/// A pre-built image is still just pulled: a repository that opted into an
/// image whose contents it does not describe has nothing for anvil to hash,
/// and must keep working exactly as before.
#[test]
fn configured_image_still_pulls() {
    let (_tmp, root) = named_workspace("repo");
    write(&root.join("anvil.toml"), "[container]\nenabled = true\nimage = \"img:1\"\n");
    run_update(&container_catalog(), &github_apply(), &root).unwrap();

    assert!(!root.join(".anvil/container/Dockerfile").exists());
    let shim = std::fs::read_to_string(root.join("justfiles/anvil/container.just")).unwrap();
    assert!(shim.contains(r#"anvil_container_image_source := "pull""#));
    assert!(shim.contains(r#"$hashInputs = @()"#), "nothing to hash in pull mode");
}

/// After applying with container mode enabled, a second dry-run must report no
/// changes — the enabled transforms are deterministic and the regenerate-check
/// stays green.
#[test]
fn container_enabled_apply_then_dry_run_is_clean() {
    let (_tmp, root) = named_workspace("repo");
    write(
        &root.join("anvil.toml"),
        "[container]\nenabled = true\nimage = \"img:1\"\ndevcontainer = true\n",
    );
    run_update(&container_catalog(), &github_apply(), &root).unwrap();

    let dry = Cli {
        backends: vec!["github".to_owned()],
        no_backends: false,
        dry_run: true,
        force: false,
    };
    let outcome = run_update(&container_catalog(), &dry, &root).unwrap();
    assert!(!outcome.plan.has_changes(), "second dry-run after apply must be clean");
}

/// ADO CI job-level container injection: when container mode is enabled with
/// the ADO backend, the root pipelines declare the `anvil_container` resource
/// and the job wrapper gains the `container` parameter, the conditional
/// `container:` binding, and the `ANVIL_IN_CONTAINER` variable.
#[test]
fn container_enabled_ado_ci() {
    let (_tmp, root) = named_workspace("repo");
    write(
        &root.join("anvil.toml"),
        "[container]\nenabled = true\nimage = \"ghcr.io/acme/rust-dev:1.2.3\"\n",
    );
    run_update(&container_catalog(), &ado_apply(), &root).unwrap();

    let job = std::fs::read_to_string(root.join(".pipelines/anvil/steps/job.yml")).unwrap();
    assert!(job.contains("  - name: container\n    type: string\n    default: anvil_container\n"));
    assert!(job.contains("    ${{ if ne(parameters.container, '') }}:\n      container: ${{ parameters.container }}\n"));
    assert!(job.contains("    variables:\n      ANVIL_IN_CONTAINER: '1'\n"));

    for name in ["anvil-pr.yml", "anvil-scheduled.yml"] {
        let root_pipeline = std::fs::read_to_string(root.join(".pipelines").join(name)).unwrap();
        assert!(
            root_pipeline
                .contains("resources:\n  containers:\n    - container: anvil_container\n      image: \"ghcr.io/acme/rust-dev:1.2.3\"\n"),
            "{name} declares the container resource"
        );
    }
}

/// Acceptance test for the opt-in requirement: the **stock** public
/// `Catalog::anvil()` — no fork, no `with_container_artifact` call — must let a
/// repo opt into container execution purely by writing `[container] enabled =
/// true` in `anvil.toml`, and emit the `container.just` shim. This is the exact
/// scenario that previously failed with "this anvil catalog does not provide
/// container support".
#[test]
fn base_catalog_supports_container_mode() {
    let (_tmp, root) = named_workspace("repo");
    write(&root.join("anvil.toml"), "[container]\nenabled = true\nimage = \"img:1\"\n");

    // The stock catalog, exactly as a public cargo-anvil user has it.
    run_update(&cargo_anvil::Catalog::anvil(), &github_apply(), &root).unwrap();

    assert!(
        root.join("justfiles/anvil/container.just").is_file(),
        "enabling [container] on the stock catalog must emit container.just"
    );
    let mod_just = std::fs::read_to_string(root.join("justfiles/anvil/mod.just")).unwrap();
    assert!(mod_just.contains("container.just"), "mod.just must import the container shim");
}

/// The stock catalog must also drive pillars 2 and 3 with no fork: a config
/// that enables container execution and adds `[[image]]` and `[cluster]`
/// emits the image-build recipes and the cluster harness alongside the shim.
#[test]
fn base_catalog_supports_image_and_cluster() {
    let (_tmp, root) = named_workspace("repo");
    write(
        &root.join("anvil.toml"),
        "[container]\nenabled = true\nimage = \"img:1\"\n\n\
         [[image]]\nname = \"svc\"\ndockerfile = \"D\"\ncontext = \"out/svc\"\n\n\
         [cluster]\nname = \"anvil-kind\"\nload-images = [\"svc\"]\n",
    );

    run_update(&cargo_anvil::Catalog::anvil(), &github_apply(), &root).unwrap();

    assert!(root.join("justfiles/anvil/container.just").is_file());
    assert!(root.join("justfiles/anvil/container-images.just").is_file());
    assert!(root.join("justfiles/anvil/cluster.just").is_file());
    assert!(root.join("justfiles/anvil/cluster-bootstrap.just").is_file());
}

/// Every image is built by identical logic, so the emitted file carries one
/// recipe body and a table of per-image data. Emitting a recipe per image
/// repeated ~35 lines of body per entry and left N copies free to drift.
#[test]
fn images_share_one_recipe_body() {
    let (_tmp, root) = named_workspace("repo");
    write(
        &root.join("anvil.toml"),
        "image-output-dir = \"out\"\n\n\
         [container]\nenabled = true\nimage = \"img:1\"\n\n\
         [[image]]\nname = \"alpha\"\ndockerfile = \"a.Dockerfile\"\ncontext = \"out/a\"\n\n\
         [[image]]\nname = \"beta\"\nrepository = \"acme/beta\"\ndockerfile = \"b.Dockerfile\"\n\
         context = \"out/b\"\ntarget = \"runtime\"\ndepends-on = [\"alpha\"]\n",
    );
    run_update(&container_catalog(), &github_apply(), &root).unwrap();

    let images = std::fs::read_to_string(root.join("justfiles/anvil/container-images.just")).unwrap();

    // One parameterized recipe, not one per image.
    assert_eq!(images.matches("\nanvil-image name=").count(), 1);
    assert!(!images.contains("anvil-image-alpha"), "no per-image recipe should be emitted");
    assert!(!images.contains("anvil-image-beta"), "no per-image recipe should be emitted");
    assert_eq!(
        images.matches("& $engine @cmd").count(),
        1,
        "the build body must appear exactly once"
    );

    // Per-image data still reaches the table, including the repository override
    // and an absent optional field.
    assert!(images.contains("'alpha' = @{"));
    assert!(images.contains("'beta' = @{"));
    assert!(images.contains("repository = 'acme/beta'"));
    assert!(images.contains("target     = 'runtime'"));
    assert!(images.contains("target     = $null"));

    // The aggregate drives the shared recipe in dependency order.
    let alpha = images.find("just anvil-image 'alpha'").expect("alpha is built");
    let beta = images.find("just anvil-image 'beta'").expect("beta is built");
    assert!(alpha < beta, "depends-on must order the aggregate");
}

/// With `[container] enabled = false`, even a container-capable catalog emits a
/// tree byte-identical to the base catalog's — the disabled path is inert.
#[test]
fn disabled_container_is_byte_identical_to_base() {
    let (_tmp_a, root_a) = named_workspace("repo");
    run_update(&cargo_anvil::Catalog::anvil(), &github_apply(), &root_a).unwrap();

    let (_tmp_b, root_b) = named_workspace("repo");
    write(
        &root_b.join("anvil.toml"),
        "[container]\nenabled = false\nimage = \"img:1\"\ndevcontainer = true\n",
    );
    run_update(&container_catalog(), &github_apply(), &root_b).unwrap();

    // The disabled anvil.toml is the only differing file; every generated
    // artifact must match byte-for-byte.
    assert_eq!(
        render_tree_excluding(&root_a, "anvil.toml"),
        render_tree_excluding(&root_b, "anvil.toml"),
        "disabled container mode must not alter the emitted tree"
    );
}

/// A generic anvil.toml exercising all three pillars together — the customer's
/// shape: containerized recipe execution (`[container]`), a multi-image build
/// with deps, staged artifacts, build-args and a target (`[[image]]`), and a
/// Kind cluster harness (`[cluster]`) with a pinned dependency chart, a
/// repo-local chart with CRDs/`--set`/rollout waits, diagnostics, bounded
/// retries and every extension hook. Snapshot the whole tree so the generated
/// image recipes and cluster harness are reviewable byte-for-byte.
fn image_cluster_toml() -> &'static str {
    "image-output-dir = \"out\"\n\
     \n\
     [container]\n\
     enabled = true\n\
     image = \"ghcr.io/acme/rust-dev:1.2.3\"\n\
     engine = \"docker\"\n\
     \n\
     [[image]]\n\
     name = \"base-image\"\n\
     dockerfile = \"containers/base/Dockerfile\"\n\
     context = \"out/base\"\n\
     build-args = { BASE_IMAGE = \"mcr.example/base:3.0\" }\n\
     \n\
     [[image]]\n\
     name = \"my-service\"\n\
     dockerfile = \"containers/svc/Dockerfile\"\n\
     target = \"runtime\"\n\
     context = \"out/svc\"\n\
     depends-on = [\"base-image\"]\n\
     stage-artifacts = [\n\
       { from = \"target/{profile}/my-svc\", to = \"bin/my-svc\" },\n\
     ]\n\
     \n\
     [cluster]\n\
     name = \"anvil-kind\"\n\
     node-image = \"kindest/node:v1.31.0\"\n\
     workers = 2\n\
     load-images = [\"my-service\"]\n\
     \n\
     [[cluster.dependency]]\n\
     name = \"cert-manager\"\n\
     manifest = \"https://example.com/cert-manager.yaml\"\n\
     version = \"v1.16.1\"\n\
     preload-images = [\"quay.io/jetstack/cert-manager-controller:v1.16.1\"]\n\
     wait = [\"deployment/cert-manager-webhook\"]\n\
     \n\
     [[cluster.chart]]\n\
     name = \"svc\"\n\
     path = \"charts/svc\"\n\
     namespace = \"svc-system\"\n\
     crds = \"charts/svc/crds\"\n\
     set = { \"image.tag\" = \"{tag}\" }\n\
     wait = [\"deployment/svc-controller\"]\n\
     \n\
     [cluster.diagnostics]\n\
     resources = [\"pods -A -o wide\", \"events --sort-by=.lastTimestamp\"]\n\
     logs = [\"deployment/svc-controller\"]\n\
     \n\
     [cluster.retry]\n\
     attempts = 2\n\
     delay-seconds = 10\n\
     \n\
     [cluster.hooks]\n\
     pre-install = \"cosmic-native-auth\"\n\
     post-install = \"record-issuer\"\n\
     pre-test = \"seed-data\"\n\
     on-failure = \"collect-support-bundle\"\n"
}

#[test]
fn container_image_and_cluster_tree() {
    let (_tmp, root) = named_workspace("repo");
    write(&root.join("anvil.toml"), image_cluster_toml());
    run_update(&container_catalog(), &github_apply(), &root).unwrap();
    insta::assert_snapshot!("container_image_and_cluster", render_tree(&root));
}

/// The generated image recipes and cluster harness must have every `__TOKEN__`
/// placeholder substituted — no generation marker may survive into the output.
#[test]
fn rendered_container_files_have_no_unsubstituted_tokens() {
    let (_tmp, root) = named_workspace("repo");
    write(&root.join("anvil.toml"), image_cluster_toml());
    run_update(&container_catalog(), &github_apply(), &root).unwrap();

    for rel in [
        "justfiles/anvil/container-images.just",
        "justfiles/anvil/cluster.just",
        "justfiles/anvil/cluster-bootstrap.just",
        "justfiles/anvil/container.just",
    ] {
        let body = std::fs::read_to_string(root.join(rel)).unwrap();
        assert!(!body.contains("__"), "{rel} still contains a `__TOKEN__` placeholder:\n{body}");
    }
}

/// Pillar 2/3 are additive: a container-enabled config that configures neither
/// `[[image]]` nor `[cluster]` must not emit any of the new files, and
/// `mod.just` must not import them — so a plain pillar-1 container build is
/// byte-identical whether or not the new artifacts are registered.
#[test]
fn image_cluster_files_absent_when_unconfigured() {
    let (_tmp, root) = named_workspace("repo");
    write(&root.join("anvil.toml"), "[container]\nenabled = true\nimage = \"img:1\"\n");
    run_update(&container_catalog(), &github_apply(), &root).unwrap();

    assert!(!root.join("justfiles/anvil/container-images.just").exists());
    assert!(!root.join("justfiles/anvil/cluster.just").exists());
    assert!(!root.join("justfiles/anvil/cluster-bootstrap.just").exists());
    let mod_just = std::fs::read_to_string(root.join("justfiles/anvil/mod.just")).unwrap();
    assert!(!mod_just.contains("container-images.just"));
    assert!(!mod_just.contains("cluster.just"));
    assert!(!mod_just.contains("cluster-bootstrap.just"));
}

/// Only the configured pillar emits: images without a cluster emit the image
/// recipes and their import, but neither cluster file nor cluster import.
#[test]
fn images_without_cluster_emit_only_images() {
    let (_tmp, root) = named_workspace("repo");
    write(
        &root.join("anvil.toml"),
        "[container]\nenabled = true\nimage = \"img:1\"\n\n[[image]]\nname = \"svc\"\ndockerfile = \"D\"\ncontext = \"out/svc\"\n",
    );
    run_update(&container_catalog(), &github_apply(), &root).unwrap();

    assert!(root.join("justfiles/anvil/container-images.just").is_file());
    assert!(!root.join("justfiles/anvil/cluster.just").exists());
    let mod_just = std::fs::read_to_string(root.join("justfiles/anvil/mod.just")).unwrap();
    assert!(mod_just.contains("import? 'container-images.just'"));
    assert!(!mod_just.contains("cluster.just"));
}

/// The mirror of the above: a cluster without any `[[image]]` emits the cluster
/// files and their imports, but neither the image recipe file nor its import.
#[test]
fn cluster_without_images_emit_only_cluster() {
    let (_tmp, root) = named_workspace("repo");
    write(
        &root.join("anvil.toml"),
        "[container]\nenabled = true\nimage = \"img:1\"\n\n[cluster]\nname = \"anvil-kind\"\n",
    );
    run_update(&container_catalog(), &github_apply(), &root).unwrap();

    assert!(root.join("justfiles/anvil/cluster.just").is_file());
    assert!(root.join("justfiles/anvil/cluster-bootstrap.just").is_file());
    assert!(!root.join("justfiles/anvil/container-images.just").exists());
    let mod_just = std::fs::read_to_string(root.join("justfiles/anvil/mod.just")).unwrap();
    assert!(mod_just.contains("cluster.just"));
    assert!(!mod_just.contains("container-images.just"));
}

/// After applying an image+cluster config, a second dry-run must report no
/// changes — the generated recipes are deterministic.
#[test]
fn image_cluster_apply_then_dry_run_is_clean() {
    let (_tmp, root) = named_workspace("repo");
    write(&root.join("anvil.toml"), image_cluster_toml());
    run_update(&container_catalog(), &github_apply(), &root).unwrap();

    let dry = Cli {
        backends: vec!["github".to_owned()],
        no_backends: false,
        dry_run: true,
        force: false,
    };
    let outcome = run_update(&container_catalog(), &dry, &root).unwrap();
    assert!(!outcome.plan.has_changes(), "second dry-run after apply must be clean");
}
