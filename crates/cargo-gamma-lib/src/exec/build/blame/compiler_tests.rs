// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use cargo_gamma_engine::ops::collect::Confidence;

use super::*;
use crate::ops::collect::{Candidate, Shape, into_mutants};
use crate::parse::SourceFile;

const SOURCE: &str = r"
        use std::io::{self, Read};
        struct Reader;
        impl Read for Reader {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> { Ok(0) }
        }
        struct Archive<R>(R);
        struct Entries<R>(std::marker::PhantomData<R>);
        struct Entry<R>(std::marker::PhantomData<R>);
        impl<R: Read> Archive<R> {
            fn entries(self) -> io::Result<Entries<R>> { Ok(Entries(std::marker::PhantomData)) }
        }
        impl<R: Read> Iterator for Entries<R> {
            type Item = io::Result<Entry<R>>;
            fn next(&mut self) -> Option<Self::Item> { None }
        }
        impl<R: Read> Entry<R> { fn size(&self) -> u64 { 4 } }
        fn unpack() -> io::Result<u64> {
            let decoder = Reader.take(4);
            let mut size = 0;
            for entry in Archive(decoder).entries()? {
                let entry = entry?;
                size += 100 - entry.size();
            }
            Ok(size)
        }
    ";

#[test]
fn guarded_reader_failure_does_not_blame_a_downstream_never_type_cascade() {
    let source = SOURCE;
    let file = SourceFile::parse("subject.rs", source.into()).unwrap();
    let candidates = [
        ("iter.take_to_skip", "Reader.take(4)", "Reader.skip(4)", Shape::IterExpr),
        ("arith.sub_to_add", "100 - entry.size()", "(100) + (entry.size())", Shape::Expr),
    ]
    .map(|(mutator, original, replacement, shape)| {
        let start = source.find(original).unwrap();
        Candidate {
            mutator,
            span: start..start + original.len(),
            replacement: replacement.into(),
            replacement_index: 0,
            identity_occurrence: None,
            item_path: "unpack".into(),
            trait_impl: None,
            shape,
            confidence: Confidence::default(),
        }
    });
    let mut mutants = into_mutants(&file, "subject", candidates.into());
    for (index, mutant) in mutants.iter_mut().enumerate() {
        mutant.ordinal = u32::try_from(index + 1).unwrap();
    }
    let selected = mutants.iter().collect::<Vec<_>>();
    let (instrumented, guards) = crate::schema::instrument_with_guards(source, &selected).unwrap();
    let stdout = [
        (
            "Reader.skip(4)",
            "the method `skip` exists for struct `Reader`, but its trait bounds were not satisfied",
        ),
        ("(100) + (entry.size())", "no method named `size` found for type `!`"),
    ]
    .map(|(replacement, message)| {
        let start = instrumented.find(replacement).unwrap();
        let preceding = &instrumented[..start];
        let line = preceding.split('\n').count();
        let column = preceding.rsplit('\n').next().unwrap().len() + 1;
        serde_json::json!({
            "reason": "compiler-message",
            "message": {
                "level": "error",
                "code": {"code": "E0599"},
                "message": message,
                "spans": [{
                    "file_name": "subject.rs",
                    "line_start": line, "line_end": line,
                    "column_start": column, "column_end": column + replacement.len(),
                    "is_primary": true
                }]
            }
        })
        .to_string()
    })
    .join("\n");
    let guards = guards
        .into_iter()
        .map(|(ordinal, guard)| (ordinal, ("subject.rs".into(), guard)))
        .collect();
    let blamed = blame(&stdout, Utf8Path::new("workspace"), &guards);
    assert_eq!(
        blamed,
        HashMap::from_iter([(
            1,
            CompilerReason {
                code: "E0599".into(),
                category: "the method <value> exists for struct <value>, but its trait bounds were not satisfied".into(),
                evidence: AttributionEvidence::ReplacementPrimary,
            },
        )])
    );
    let independent = stdout.replace("for type `!`", "for struct `Entry`");
    let blamed = blame(&independent, Utf8Path::new("workspace"), &guards);
    assert_eq!(blamed.keys().copied().collect::<HashSet<_>>(), HashSet::from_iter([1, 2]));
}
