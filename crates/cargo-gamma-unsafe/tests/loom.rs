// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(loom)]

#[cfg(not(unix))]
#[test]
fn concurrency_models() {
    cargo_gamma_unsafe::run_loom_models();
}

#[cfg(unix)]
mod interrupt {
    use cargo_gamma_unsafe::loom_models;

    #[test]
    fn extracted_kills_finish_before_release_reap_and_reuse() {
        loom_models::extracted_kills_finish_before_release_reap_and_reuse();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn extracted_cgroup_writes_finish_before_descriptor_close_and_reuse() {
        loom_models::extracted_cgroup_writes_finish_before_descriptor_close_and_reuse();
    }

    #[test]
    fn only_the_final_cleanup_owner_can_take_fatal_responsibility() {
        loom_models::only_the_final_cleanup_owner_can_take_fatal_responsibility();
    }

    #[test]
    fn claim_versus_sweep_never_orphans_the_claimed_group() {
        loom_models::claim_versus_sweep_never_orphans_the_claimed_group();
    }

    #[test]
    fn a_spawn_window_never_leaves_an_unwatched_child() {
        loom_models::a_spawn_window_never_leaves_an_unwatched_child();
    }

    #[test]
    fn a_terminal_signal_always_has_a_reraise_owner() {
        loom_models::a_terminal_signal_always_has_a_reraise_owner();
    }
}

#[cfg(target_os = "linux")]
mod cgroup {
    use cargo_gamma_unsafe::loom_models;

    #[test]
    fn concurrent_leaf_creations_never_share_a_name() {
        loom_models::concurrent_leaf_creations_never_share_a_name();
    }

    #[test]
    fn only_one_caller_ever_starts_the_reaper() {
        loom_models::only_one_caller_ever_starts_the_reaper();
    }

    #[test]
    fn a_reaper_claim_handed_back_can_be_taken_again() {
        loom_models::a_reaper_claim_handed_back_can_be_taken_again();
    }
}
