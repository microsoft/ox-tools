// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Azure DevOps Pipelines backend files: step templates, stages templates,
//! and root pipelines, each an owned file gated on [`Backend::Ado`].
//!
//! Holds the embedded templates, the per-group fan-out, and the registry
//! functions. See [`ado.md`](../../../docs/design/ado.md).

use crate::backend::Backend;
use crate::catalog::Artifact;

/// Embedded body of the shared setup step template.
const SETUP_STEP: &str = include_str!("../../../templates/ado/steps/setup.yml");

/// Embedded body of the cargo-delta impact step template.
const IMPACT_STEP: &str = include_str!("../../../templates/ado/steps/impact.yml");

/// Embedded body of the advisory-comments step template.
const ADVISORY_COMMENTS_STEP: &str = include_str!("../../../templates/ado/steps/advisory-comments.yml");

/// Embedded body of the dirty-file job wrapper.
const JOB_WRAPPER: &str = include_str!("../../../templates/ado/steps/job.yml");

/// Embedded body of the benchmark-history restore step template.
const BENCH_HISTORY_RESTORE_STEP: &str = include_str!("../../../templates/ado/steps/bench-history-restore.yml");

/// Embedded body of the benchmark-findings build-summary step template.
const BENCH_HISTORY_SUMMARY_STEP: &str = include_str!("../../../templates/ado/steps/bench-history-summary.yml");

/// Embedded body of the PR-tier stages template.
const PR_STAGES: &str = include_str!("../../../templates/ado/pr-stages.yml");

/// Embedded body of the scheduled-tier stages template.
const SCHEDULED_STAGES: &str = include_str!("../../../templates/ado/scheduled-stages.yml");

/// Embedded body of the user-owned PR-tier custom-stages extension stub.
const CUSTOM_PR_STAGES: &str = include_str!("../../../templates/ado/custom-pr-stages.yml");

/// Embedded body of the user-owned scheduled-tier custom-stages extension stub.
const CUSTOM_SCHEDULED_STAGES: &str = include_str!("../../../templates/ado/custom-scheduled-stages.yml");

/// Embedded body of the PR root pipeline.
const PR_ROOT_PIPELINE: &str = include_str!("../../../templates/ado/pr-root-pipeline.yml");

/// Embedded body of the scheduled root pipeline.
const SCHEDULED_ROOT_PIPELINE: &str = include_str!("../../../templates/ado/scheduled-root-pipeline.yml");

/// All check groups that get a per-group step template.
///
/// See `github::GROUPS` for the rationale around splitting `pr-slow` into
/// three cloud-workflow-visible sub-stages.
#[cfg(test)]
const GROUPS: &[&str] = &[
    "pr-fast",
    "pr-test",
    "pr-runtime-analysis",
    "pr-mutants",
    "scheduled-test",
    "scheduled-advisories",
    "scheduled-runtime-analysis",
    "scheduled-exhaustive",
    "scheduled-benchmarks",
];

/// Embedded template for one per-group step. `__GROUP__` is substituted with
/// the group name at emit time.
const GROUP_STEP_TEMPLATE: &str = include_str!("../../../templates/ado/steps/group.yml");

/// Placeholder token the per-group template uses for the group name.
const GROUP_PLACEHOLDER: &str = "__GROUP__";

/// Placeholder lines for steps a group needs around the uniform runner.
/// Substituted away entirely for groups that need none.
const PRE_STEPS_PLACEHOLDER: &str = "__PRE_STEPS__\n";
const POST_STEPS_PLACEHOLDER: &str = "__POST_STEPS__\n";

/// Steps that run before the uniform group runner, per group.
///
/// These live in the group's own emitted step template rather than at the
/// call site, so `pr.yml` / `scheduled.yml` stay a plain list of groups. A
/// group absent from the table gets nothing.
const GROUP_PRE_STEPS: &[(&str, &str)] = &[(
    "scheduled-benchmarks",
    // The analysis orders each series by first-parent commit topology and
    // locates the merge-base, so it needs the whole commit graph. LFS
    // matters because benchmark inputs can be LFS-tracked and would
    // otherwise arrive as pointer files.
    //
    // The checkout is explicit rather than a wrapper parameter: job.yml is
    // the file adopters fork, so binding a parameter their copy lacks would
    // fail expansion for the whole pipeline.
    "  - checkout: self\n\
     \x20   fetchDepth: 0\n\
     \x20   lfs: true\n\
     \x20 - template: bench-history-restore.yml\n\
     \x20   parameters:\n\
     \x20     artifact: bench-history-$(Agent.OS)\n",
)];

/// Steps that run after the uniform group runner, per group.
const GROUP_POST_STEPS: &[(&str, &str)] = &[
    (
        "scheduled-test",
        "  - task: PublishCodeCoverageResults@2\n\
         \x20   condition: succeededOrFailed()\n\
         \x20   displayName: Publish coverage\n\
         \x20   inputs:\n\
         \x20     summaryFileLocation: target/coverage/cobertura-*.xml\n\
         \x20     failIfCoverageEmpty: false\n",
    ),
    ("scheduled-benchmarks", "  - template: bench-history-summary.yml\n"),
];

/// The extra steps registered for `group`, or the empty string.
fn extra_steps(table: &'static [(&'static str, &'static str)], group: &str) -> &'static str {
    table
        .iter()
        .find_map(|&(name, steps)| (name == group).then_some(steps))
        .unwrap_or("")
}

/// Render the step template for one group.
#[must_use]
fn render_group_step(group: &str) -> String {
    GROUP_STEP_TEMPLATE
        .replace(GROUP_PLACEHOLDER, group)
        .replace(PRE_STEPS_PLACEHOLDER, extra_steps(GROUP_PRE_STEPS, group))
        .replace(POST_STEPS_PLACEHOLDER, extra_steps(GROUP_POST_STEPS, group))
}

/// Repo-root-relative path for one group's step template.
#[cfg(test)]
#[must_use]
fn group_step_path(group: &str) -> String {
    format!(".pipelines/anvil/steps/{group}.yml")
}

/// `.pipelines/anvil/steps/setup.yml`.
#[must_use]
pub fn setup_step() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil/steps/setup.yml", SETUP_STEP)
}

/// `.pipelines/anvil/steps/impact.yml`.
#[must_use]
pub fn impact_step() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil/steps/impact.yml", IMPACT_STEP)
}

/// `.pipelines/anvil/steps/advisory-comments.yml`.
#[must_use]
pub fn advisory_comments() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil/steps/advisory-comments.yml", ADVISORY_COMMENTS_STEP)
}

/// `.pipelines/anvil/steps/job.yml` — the dirty-file job wrapper.
#[must_use]
pub fn job_wrapper() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil/steps/job.yml", JOB_WRAPPER)
}

/// `.pipelines/anvil/steps/bench-history-restore.yml` — restores the
/// benchmark history the previous scheduled run published.
#[must_use]
pub fn bench_history_restore() -> Artifact {
    Artifact::backend_file(
        Backend::Ado,
        ".pipelines/anvil/steps/bench-history-restore.yml",
        BENCH_HISTORY_RESTORE_STEP,
    )
}

/// `.pipelines/anvil/steps/bench-history-summary.yml` — attaches the
/// benchmark findings to the build summary.
#[must_use]
pub fn bench_history_summary() -> Artifact {
    Artifact::backend_file(
        Backend::Ado,
        ".pipelines/anvil/steps/bench-history-summary.yml",
        BENCH_HISTORY_SUMMARY_STEP,
    )
}

/// `.pipelines/anvil/pr.yml` — the PR-tier stages template.
#[must_use]
pub fn pr_stages() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil/pr.yml", PR_STAGES)
}

/// `.pipelines/anvil/scheduled.yml` — the scheduled-tier stages template.
#[must_use]
pub fn scheduled_stages() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil/scheduled.yml", SCHEDULED_STAGES)
}

/// `.pipelines/anvil/custom-pr-stages.yml` — the repo-owned extension point
/// for PR-tier stages.
///
/// Emitted once as an empty `stages: []` stub. The PR root pipeline
/// references it after the anvil stages, so an adopter can add their own
/// stages here without editing the anvil-owned root or stages template.
/// Once edited it follows the standard dirty-file flow (Propose, don't
/// overwrite), exactly like `steps/job.yml`.
#[must_use]
pub fn custom_pr_stages() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil/custom-pr-stages.yml", CUSTOM_PR_STAGES)
}

/// `.pipelines/anvil/custom-scheduled-stages.yml` — the repo-owned extension
/// point for scheduled-tier stages. See [`custom_pr_stages`].
#[must_use]
pub fn custom_scheduled_stages() -> Artifact {
    Artifact::backend_file(
        Backend::Ado,
        ".pipelines/anvil/custom-scheduled-stages.yml",
        CUSTOM_SCHEDULED_STAGES,
    )
}

/// `.pipelines/anvil-pr.yml` — the PR root pipeline.
#[must_use]
pub fn pr_root_pipeline() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil-pr.yml", PR_ROOT_PIPELINE)
}

/// `.pipelines/anvil-scheduled.yml` — the scheduled root pipeline.
#[must_use]
pub fn scheduled_root_pipeline() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil-scheduled.yml", SCHEDULED_ROOT_PIPELINE)
}

/// The per-group step templates, one concrete owned file per group.
///
/// Each `(group, path)` pair's `path` must equal [`group_step_path`] for its
/// group (asserted in tests); the body is [`render_group_step`].
pub(crate) const GROUP_STEPS: &[(&str, &str)] = &[
    ("pr-fast", ".pipelines/anvil/steps/pr-fast.yml"),
    ("pr-test", ".pipelines/anvil/steps/pr-test.yml"),
    ("pr-runtime-analysis", ".pipelines/anvil/steps/pr-runtime-analysis.yml"),
    ("pr-mutants", ".pipelines/anvil/steps/pr-mutants.yml"),
    ("scheduled-test", ".pipelines/anvil/steps/scheduled-test.yml"),
    ("scheduled-advisories", ".pipelines/anvil/steps/scheduled-advisories.yml"),
    (
        "scheduled-runtime-analysis",
        ".pipelines/anvil/steps/scheduled-runtime-analysis.yml",
    ),
    ("scheduled-exhaustive", ".pipelines/anvil/steps/scheduled-exhaustive.yml"),
    ("scheduled-benchmarks", ".pipelines/anvil/steps/scheduled-benchmarks.yml"),
];

/// All ADO backend artifacts in emission order.
#[must_use]
pub(crate) fn all() -> Vec<Artifact> {
    let mut out = vec![
        setup_step(),
        impact_step(),
        advisory_comments(),
        job_wrapper(),
        bench_history_restore(),
        bench_history_summary(),
    ];
    for (group, path) in GROUP_STEPS {
        out.push(Artifact::backend_file(Backend::Ado, path, render_group_step(group)));
    }
    out.push(pr_stages());
    out.push(scheduled_stages());
    out.push(custom_pr_stages());
    out.push(custom_scheduled_stages());
    out.push(pr_root_pipeline());
    out.push(scheduled_root_pipeline());
    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn setup_and_impact_step_templates_are_non_empty() {
        assert!(SETUP_STEP.contains("just anvil-setup"));
        assert!(IMPACT_STEP.contains("cargo-delta"));
        assert!(IMPACT_STEP.contains("##vso[task.setvariable"));
    }

    #[test]
    fn setup_step_takes_group_parameter_and_dispatches() {
        assert!(SETUP_STEP.contains("name: group"));
        assert!(SETUP_STEP.contains("just anvil-setup"));
        assert!(SETUP_STEP.contains("just anvil-${{ parameters.group }}-setup"));
        assert!(SETUP_STEP.contains("eq(parameters.group, 'none')"));
    }

    #[test]
    fn setup_step_quotes_inline_command_values_containing_colons() {
        // An inline `- bash: echo "x: y"` is a YAML *plain scalar*; the inner
        // `: ` is parsed as a mapping separator ("Mapping values are not
        // allowed in this context"), which ADO rejects at compile time. Such
        // values must be wrapped in quotes. Guard every inline command scalar
        // in the setup step (and catch the specific group=none echo).
        assert!(
            SETUP_STEP.contains(r#"- bash: 'echo "anvil-setup: group=none, skipping tool install"'"#),
            "the group=none echo must be single-quoted so its colon stays literal",
        );
        for line in SETUP_STEP.lines() {
            let trimmed = line.trim_start();
            let Some(value) = trimmed
                .strip_prefix("- bash:")
                .or_else(|| trimmed.strip_prefix("- script:"))
                .or_else(|| trimmed.strip_prefix("- pwsh:"))
                .or_else(|| trimmed.strip_prefix("- powershell:"))
            else {
                continue;
            };
            let value = value.trim();
            // A quoted scalar or a block scalar (`|`/`>`) is safe; a plain
            // scalar must not contain a `: ` mapping-separator sequence.
            if value.starts_with('\'') || value.starts_with('"') || value.starts_with('|') || value.starts_with('>') {
                continue;
            }
            assert!(
                !value.contains(": "),
                "unquoted inline command scalar with a colon will break ADO YAML compilation: {line}",
            );
        }
    }

    #[test]
    fn group_step_passes_group_to_setup() {
        let body = render_group_step("pr-fast");
        assert!(body.contains("template: setup.yml"));
        assert!(body.contains("group: pr-fast"));
    }

    #[test]
    fn impact_step_uses_group_none_and_installs_only_cargo_delta() {
        assert!(IMPACT_STEP.contains("group: none"));
        assert!(IMPACT_STEP.contains("anvil-tool-cargo-delta-install"));
        assert!(!IMPACT_STEP.contains("cargo install --locked cargo-delta"));
    }

    #[test]
    fn job_wrapper_declares_expected_contract() {
        for needle in [
            "name: name",
            "name: pool",
            "name: steps",
            "type: stepList",
            "name: artifacts",
            "PublishPipelineArtifact@1",
        ] {
            assert!(JOB_WRAPPER.contains(needle), "wrapper missing '{needle}'");
        }
    }

    #[test]
    fn render_group_step_has_include_inputs_and_env() {
        let body = render_group_step("pr-fast");
        assert!(body.contains("parameters:"));
        assert!(body.contains("name: include_modified"));
        assert!(body.contains("name: include_affected"));
        assert!(body.contains("name: include_required"));
        assert!(body.contains("just anvil-pr-fast"));
        assert!(body.contains("ANVIL_INCLUDE_MODIFIED"));
        assert!(body.contains("ANVIL_INCLUDE_AFFECTED"));
        assert!(body.contains("ANVIL_INCLUDE_REQUIRED"));
        // PR_TITLE is resolved from the REST API (ADO has no PR-title
        // predefined variable) and threaded via the PR_TITLE pipeline var.
        assert!(body.contains("PR_TITLE: $(PR_TITLE)"));
        assert!(body.contains("setvariable variable=PR_TITLE"));
        assert!(!body.contains("PR_TITLE: $(System.PullRequest.Title)"));
    }

    #[test]
    fn group_step_path_is_under_pipelines() {
        assert_eq!(group_step_path("scheduled-test"), ".pipelines/anvil/steps/scheduled-test.yml");
    }

    #[test]
    fn pr_stages_has_impact_and_group_stages() {
        for needle in [
            "stage: impact\n",
            "stage: pr_fast",
            "stage: pr_test",
            "stage: pr_runtime_analysis",
            "stage: pr_mutants",
        ] {
            assert!(PR_STAGES.contains(needle), "PR stages missing '{needle}'");
        }
        // The impact computation is a single stage with two per-OS jobs, not a
        // stage per OS -- matching how the pr-* stages run per-OS jobs.
        for needle in ["stage: impact_linux", "stage: impact_windows"] {
            assert!(
                !PR_STAGES.contains(needle),
                "impact should be one stage with per-OS jobs, not '{needle}'"
            );
        }
        for needle in ["stage: pr_slow\n", "stage: pr_slow1\n", "stage: pr_slow2\n", "stage: pr_slow3\n"] {
            assert!(
                !PR_STAGES.contains(needle),
                "Stale stage '{needle}' should be gone after the pr-slow rename"
            );
        }
        // Two per-OS jobs in the single impact stage.
        assert!(PR_STAGES.contains("name: compute_linux"));
        assert!(PR_STAGES.contains("name: compute_windows"));
        // Downstream stages consume the per-OS job outputs from the one stage.
        assert!(PR_STAGES.contains("stageDependencies.impact.compute_linux.outputs"));
        assert!(PR_STAGES.contains("stageDependencies.impact.compute_windows.outputs"));
        assert!(PR_STAGES.contains("- template: steps/job.yml"));
        assert!(
            !PR_STAGES.contains("\n      - job: "),
            "PR stages defines a bare `- job:` instead of going through steps/job.yml"
        );
        assert_eq!(
            PR_STAGES.matches("- task: PublishCodeCoverageResults@2").count(),
            2,
            "cobertura publish should appear once per pr_test job (linux + windows)"
        );
        // Every pr-* stage depends on the single impact stage.
        assert_eq!(
            PR_STAGES.matches("dependsOn: [impact]").count(),
            4,
            "each of the four pr-* stages must depend on the single impact stage"
        );
    }

    #[test]
    fn scheduled_stages_has_four_groups() {
        for needle in [
            "stage: scheduled_test",
            "stage: scheduled_advisories",
            "stage: scheduled_runtime_analysis",
            "stage: scheduled_exhaustive",
            "stage: scheduled_benchmarks",
        ] {
            assert!(SCHEDULED_STAGES.contains(needle), "scheduled stages missing '{needle}'");
        }
        // Coverage publication lives in the group's own step template now;
        // the stages file is a plain list of groups.
        assert!(render_group_step("scheduled-test").contains("PublishCodeCoverageResults@2"));
        assert!(SCHEDULED_STAGES.contains("- template: steps/job.yml"));
        assert!(
            !SCHEDULED_STAGES.contains("\n      - job: "),
            "Scheduled stages defines a bare `- job:` instead of going through steps/job.yml"
        );
    }

    #[test]
    fn scheduled_benchmarks_stage_round_trips_the_history_artifact() {
        // The stage is a plain list of groups; the round-trip lives in the
        // group's own step template.
        let group_step = render_group_step("scheduled-benchmarks");
        assert!(
            !JOB_WRAPPER.contains("fetchDepth"),
            "the job wrapper contract must stay frozen; put checkout in the group's step template"
        );
        assert!(group_step.contains("- checkout: self"));
        assert!(group_step.contains("fetchDepth: 0"));
        // Benchmark inputs can be LFS-tracked.
        assert!(group_step.contains("lfs: true"));
        assert!(group_step.contains("template: bench-history-restore.yml"));
        assert!(group_step.contains("template: bench-history-summary.yml"));
        // A group with no registered extras gets none of this.
        assert!(!render_group_step("scheduled-exhaustive").contains("bench-history"));
        assert!(!render_group_step("scheduled-exhaustive").contains("checkout: self"));
        // Coverage publication likewise moved off the call site.
        assert!(render_group_step("scheduled-test").contains("PublishCodeCoverageResults@2"));
        assert!(
            !SCHEDULED_STAGES.contains("PublishCodeCoverageResults@2"),
            "the stages template must not carry per-group steps"
        );
        // The publish stays a job-level output so a forked (1ESPT) wrapper
        // still translates it; the guard rides along as a `condition`.
        assert_eq!(
            SCHEDULED_STAGES
                .matches("condition: and(succeededOrFailed(), ne(variables['ANVIL_BENCH_RESTORE'], ''))")
                .count(),
            2,
            "each leg's artifact entry carries the restore guard"
        );
        assert!(JOB_WRAPPER.contains("${{ if artifact.condition }}"));
        // Take the newest build carrying the artifact whatever its outcome:
        // restoring only from green builds would drop every sample collected
        // while the pipeline was red from a regression.
        assert!(BENCH_HISTORY_RESTORE_STEP.contains("queryOrder=finishTimeDescending"));
        // Absence and operational failure must stay distinguishable, or one
        // transient error publishes an empty store over a good history.
        assert!(BENCH_HISTORY_RESTORE_STEP.contains("if ($status -eq 404) { continue }"));
        assert!(BENCH_HISTORY_RESTORE_STEP.contains("ANVIL_BENCH_RESTORE]restored"));
        assert!(BENCH_HISTORY_RESTORE_STEP.contains("ANVIL_BENCH_RESTORE]cold-start"));
        assert!(
            !BENCH_HISTORY_RESTORE_STEP.contains("continueOnError"),
            "a blanket continueOnError would read every failure as a cold start"
        );
        assert!(BENCH_HISTORY_SUMMARY_STEP.contains("##vso[task.uploadsummary]"));
        assert!(BENCH_HISTORY_SUMMARY_STEP.contains("condition: succeededOrFailed()"));
        // The machine-key escape hatch is an input on this backend too.
        assert!(SCHEDULED_STAGES.contains("name: benchMachineKey"));
        assert!(SCHEDULED_STAGES.contains("ANVIL_BENCH_MACHINE_KEY: ${{ parameters.benchMachineKey }}"));
    }

    #[test]
    fn impact_step_loads_config_for_both_snapshots_and_impact() {
        assert!(IMPACT_STEP.contains("delta_config=\"$(pwd)/.delta.toml\""));
        assert!(!IMPACT_STEP.contains("remote_branch ="));
        assert_eq!(
            IMPACT_STEP.matches("--config \"$delta_config\"").count(),
            3,
            "current snapshot, baseline snapshot, and impact must share the repository config"
        );
        assert!(IMPACT_STEP.contains("cargo delta --config \"$delta_config\" snapshot"));
        assert!(IMPACT_STEP.contains("cargo delta --config \"$delta_config\" impact"));
    }

    #[test]
    fn setup_caches_cargo_install_metadata_without_eager_libclang_install() {
        assert!(SETUP_STEP.contains("$(HOME)/.cargo/.crates.toml"));
        assert!(SETUP_STEP.contains("$(HOME)/.cargo/.crates2.json"));
        assert!(!SETUP_STEP.contains("install libclang"));
        assert!(!SETUP_STEP.contains("install -y clang-devel"));
    }

    #[test]
    fn custom_stages_stubs_are_empty_and_take_pool_parameters() {
        // The extension stubs must emit a valid empty stages list (so the
        // default emit doesn't break the pipeline) and declare the pool
        // parameters the root pipelines pass them.
        for body in [CUSTOM_PR_STAGES, CUSTOM_SCHEDULED_STAGES] {
            assert!(
                body.contains("stages: []"),
                "custom-stages stub must default to an empty stages list"
            );
            assert!(body.contains("name: linuxPool"), "custom-stages stub must declare linuxPool");
            assert!(body.contains("name: windowsPool"), "custom-stages stub must declare windowsPool");
            // It must NOT define any concrete stage by default (the
            // commented-out example doesn't count).
            let defines_stage = body
                .lines()
                .map(str::trim_start)
                .any(|l| !l.starts_with('#') && l.starts_with("- stage:"));
            assert!(!defines_stage, "default custom-stages stub must not define a stage");
        }
    }

    #[test]
    fn custom_stages_artifacts_are_under_pipelines_anvil() {
        match custom_pr_stages() {
            Artifact::OwnedFile(spec) => {
                assert_eq!(spec.path, ".pipelines/anvil/custom-pr-stages.yml");
                assert_eq!(spec.gate, Some(Backend::Ado));
            }
            Artifact::Region(_) => panic!("expected owned file"),
        }
        match custom_scheduled_stages() {
            Artifact::OwnedFile(spec) => {
                assert_eq!(spec.path, ".pipelines/anvil/custom-scheduled-stages.yml");
                assert_eq!(spec.gate, Some(Backend::Ado));
            }
            Artifact::Region(_) => panic!("expected owned file"),
        }
    }

    #[test]
    fn root_pipelines_reference_their_custom_stages_extension() {
        assert!(
            PR_ROOT_PIPELINE.contains("template: anvil/custom-pr-stages.yml"),
            "PR root must reference the custom-pr-stages extension point"
        );
        assert!(
            SCHEDULED_ROOT_PIPELINE.contains("template: anvil/custom-scheduled-stages.yml"),
            "scheduled root must reference the custom-scheduled-stages extension point"
        );
    }

    #[test]
    fn group_step_paths_match_render() {
        assert_eq!(GROUP_STEPS.len(), GROUPS.len());
        for ((group, path), expected_group) in GROUP_STEPS.iter().zip(GROUPS) {
            assert_eq!(group, expected_group, "group order must match GROUPS");
            assert_eq!(
                *path,
                group_step_path(group),
                "registry path must match group_step_path for {group}"
            );
        }
    }
}
