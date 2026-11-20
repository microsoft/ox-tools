// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Repository-wide agent instructions shared by every backend.

use crate::catalog::Artifact;

const CARGO_ANVIL_INSTRUCTIONS: &str = include_str!("../../../templates/instructions/cargo-anvil.instructions.md");
const CARGO_ANVIL_ADOPTION_SKILL: &str = include_str!("../../../templates/skills/cargo-anvil-adoption/SKILL.md");

/// `.github/instructions/cargo-anvil.instructions.md`.
#[must_use]
pub fn cargo_anvil() -> Artifact {
    Artifact::owned_file(".github/instructions/cargo-anvil.instructions.md", CARGO_ANVIL_INSTRUCTIONS)
}

/// `.github/skills/cargo-anvil-adoption/SKILL.md`.
#[must_use]
pub fn adoption_skill() -> Artifact {
    Artifact::owned_file(".github/skills/cargo-anvil-adoption/SKILL.md", CARGO_ANVIL_ADOPTION_SKILL)
}
