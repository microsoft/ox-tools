// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use camino::Utf8PathBuf;

const VERSION: u32 = 4;
type MutantId = String;

#[derive(Clone)]
struct HintContext {
    repo_sha: String,
    generated_on: String,
}

#[derive(Clone)]
struct GeneralizedHints;

#[derive(Clone, PartialEq)]
struct Killer {
    package: String,
    target: String,
    test: String,
}

enum GeneralizedSection {
    Supported(GeneralizedHints),
}

pub struct Hints {
    /// The format this was written in.
    version: u32,

    /// What wrote it, which is the provenance a reviewer asks for first.
    tool: String,

    /// The commit and date at which this artifact generation was published.
    context: HintContext,

    /// One entry per mutant with something to say about it, ordered by file and then by id.
    ///
    /// This is the semantic form used in memory. The version-4 wire form groups these entries by
    /// file and interns killers within each group.
    mutants: Vec<Hint>,

    /// Optional score-neutral tiers shared verbatim with the run record.
    generalized: GeneralizedHints,
}

struct Hint {
    /// The workspace-relative file the mutant lives in, which is the artifact's primary sort key.
    ///
    /// Carried even though nothing reads it at run time — the run keys by mutant id — because a
    /// diff of this file is read by people, and a list of content hashes with no file beside them
    /// tells a reviewer nothing about what changed.
    file: Utf8PathBuf,

    /// The mutant's content-addressed id.
    id: MutantId,

    /// The test that caught it, when one did.
    killer: Option<Killer>,

    /// Whether it failed to compile for the run that was promoted.
    ///
    /// A hint about *order*, never a filter. See [`Tier::Ordering`]: the mutant is built and judged
    /// exactly as it would have been, and all this decides is that it is offered to the compiler
    /// early, where a mutant that does turn out to be unviable costs one round instead of hiding
    /// behind another one for several.
    unviable: bool,
}

struct GroupedHints {
    version: u32,
    tool: String,
    context: HintContext,
    files: Vec<FileHints>,
    #[serde(default)]
    generalized: GeneralizedSection,
}

struct FileHints {
    path: Utf8PathBuf,
    killers: Vec<Killer>,
    mutants: Vec<GroupedHint>,
}

struct GroupedHint {
    id: MutantId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    killer: Option<u32>,
    #[serde(default, skip_serializing_if = "is_not_set")]
    unviable: bool,
}

impl From<&Hints> for GroupedHints {
    fn from(hints: &Hints) -> Self {
        let mut files = Vec::new();
        let mut start = 0;

        while start < hints.mutants.len() {
            let path = hints.mutants[start].file.clone();
            let end = hints.mutants[start..]
                .iter()
                // #[gamma::skip(relational.ne_to_eq, tag = "outofmemory", reason = "matching the first equal path produces an empty group and repeatedly allocates without advancing start")]
                .position(|hint| hint.file != path)
                .map_or(hints.mutants.len(), |offset| start + offset);
            let group = &hints.mutants[start..end];
            let mut killers: Vec<Killer> = group.iter().filter_map(|hint| hint.killer.clone()).collect();
            killers.sort_by(|left, right| killer_key(left).cmp(&killer_key(right)));
            killers.dedup();
            let mutants = group
                .iter()
                .map(|hint| GroupedHint {
                    id: hint.id.clone(),
                    killer: hint.killer.as_ref().map(|killer| {
                        let index = killers
                            .iter()
                            .position(|known| known == killer)
                            .expect("the killer pool was built from every killer in this file group");
                        u32::try_from(index).expect("a killer index cannot exceed the number of mutants retained in the bounded artifact")
                    }),
                    unviable: hint.unviable,
                })
                .collect();
            files.push(FileHints { path, killers, mutants });
            start = end;
        }

        Self {
            version: VERSION,
            tool: hints.tool.clone(),
            context: hints.context.clone(),
            files,
            generalized: GeneralizedSection::Supported(hints.generalized.clone()),
        }
    }
}

fn killer_key(killer: &Killer) -> (&str, &str, &str) {
    (killer.package.as_str(), killer.target.as_str(), killer.test.as_str())
}
