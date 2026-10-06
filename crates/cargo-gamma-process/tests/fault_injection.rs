// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(feature = "fault-injection")]

use cargo_gamma_process::faults::Fault;

#[test]
fn dependency_builds_expose_the_complete_fault_vocabulary() {
    let _ = [
        Fault::StderrReader,
        Fault::OutputWait,
        Fault::Sweep,
        Fault::AbandonCleanup,
        Fault::TryWait,
        Fault::Kill,
    ];

    #[cfg(windows)]
    let _ = [Fault::JobCreate, Fault::JobAssign, Fault::JobResume];
}
