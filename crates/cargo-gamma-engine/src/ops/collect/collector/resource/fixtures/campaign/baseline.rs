// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn disambiguate_artifact_directories(failure: &mut Error) {
    let mut used = crate::HashSet::default();

    for artifact in failure.artifacts_mut() {
        let folded = artifact.directory.to_ascii_lowercase();
        if used.insert(folded) {
            continue;
        }

        let identity = format!(
            "{}\0{}\0{}\0{}\0{}\0{}",
            artifact.value["package"].as_str().unwrap_or_default(),
            artifact.value["target"].as_str().unwrap_or_default(),
            artifact.value["packageId"].as_str().unwrap_or_default(),
            artifact.value["executable"].as_str().unwrap_or_default(),
            artifact.value["kind"].as_str().unwrap_or_default(),
            artifact.value["test"].as_str().unwrap_or_default(),
        );

        let mut placed = false;
        for length in (INITIAL_DIGEST_LEN
            ..=
            // #[gamma::skip(literal.int_increment, reason = "starting at eight and stepping by four visits the same lengths through 64 whether the inclusive bound is 64 or 65")]
            64)
            .step_by(4)
        {
            let candidate = directory_with_digest(&artifact.directory, &identity, length);
            if used.insert(candidate.to_ascii_lowercase()) {
                artifact.directory = candidate;
                placed = true;
                break;
            }
        }

        if !placed {
            for ordinal in 2_u64.. {
                let candidate = directory_with_suffix(&artifact.directory, &format!("--{ordinal}"));
                if used.insert(candidate.to_ascii_lowercase()) {
                    artifact.directory = candidate;
                    break;
                }
            }
        }
    }
}
