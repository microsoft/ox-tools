// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use camino::{Utf8Path, Utf8PathBuf};

use super::cli::{SelectArgs, SuppressArgs};
use super::dispatch::EXIT_OK;
use super::host::Host;
use super::when::When;
use crate::discover::{Plan, RunRecord, TargetFile};
use crate::elements::Publication;
use crate::error::error;
use crate::fix::Edit;
use crate::model::{Mutant, Outcome};
use crate::report::{Styler, quantity};

/// Implements `suppress`.
///
/// Suppressions are promoted from the latest persisted campaign ledger without rerunning it.
pub(super) fn suppress<H: Host>(host: &mut H, args: &SuppressArgs, _progress_when: When, styler: Styler) -> crate::Result<i32> {
    let eligible = crate::fix::Eligible::parse(&args.eligible)?;

    if eligible.is_empty() {
        return Err(error!("--eligible named no verdicts; nothing could be suppressed").usage());
    }
    if args.run.dry_run {
        return Err(error!("`cargo gamma suppress --dry-run` does not preview source edits; use `--dry-run-suppress` instead").usage());
    }
    reject_selection(&args.run.select)?;

    let selected = crate::paths::physical(&args.run.select.dir)?;
    let (root, base, _lock) = crate::exec::claim_campaign_state(&selected, args.run.measure.cache_dir.as_deref())?;
    let record = RunRecord::load_required(&base)?;
    let (plan, stale) = plan_from_record(&root, &record, &eligible)?;

    for entry in &stale {
        writeln!(host.error(), "{} {entry}", styler.warning())?;
    }

    suppress_plan(host, args, &plan, &eligible, &record, styler)
}

fn reject_selection(args: &SelectArgs) -> crate::Result<()> {
    let unsupported = [
        (args.mutators.is_some(), "--mutators"),
        (!args.files.is_empty(), "--file"),
        (!args.exclude_files.is_empty(), "--exclude-file"),
        (args.shard_count.is_some(), "--shard-count"),
        (args.shard_index.is_some(), "--shard-index"),
        (args.in_diff.is_some(), "--in-diff"),
        (!args.packages.is_empty(), "--package"),
        (args.workspace, "--workspace"),
        (!args.errors.is_empty(), "--error"),
        (!args.features.features.is_empty(), "--features"),
        (args.features.all_features, "--all-features"),
        (args.features.no_default_features, "--no-default-features"),
        (args.config.path.is_some(), "--config"),
        (args.config.no_config, "--no-config"),
    ]
    .into_iter()
    .find_map(|(present, flag)| present.then_some(flag));

    if let Some(flag) = unsupported {
        return Err(error!(
            "`cargo gamma suppress` consumes the completed campaign exactly as recorded; `{flag}` cannot narrow persisted outcomes"
        )
        .usage());
    }

    Ok(())
}

fn suppress_plan<H: Host>(
    host: &mut H,
    args: &SuppressArgs,
    plan: &Plan,
    eligible: &[crate::fix::Eligible],
    record: &RunRecord,
    styler: Styler,
) -> crate::Result<i32> {
    let edits = crate::fix::plan(&plan.mutants, eligible);
    if edits.is_empty() {
        writeln!(
            host.error(),
            "{} nothing to suppress: no mutant had an eligible verdict",
            styler.verb("Finished")
        )?;

        return Ok(EXIT_OK);
    }

    let intended = intended(&plan.mutants, &edits, eligible);
    let date = crate::fix::today();
    let written = apply_all_locked(host, args, plan, &edits, &date)?;

    if args.dry_run_suppress {
        return Ok(EXIT_OK);
    }

    verify_record_or_revert(
        host,
        record,
        eligible,
        plan,
        &intended,
        Applied {
            directives: edits.len(),
            written,
        },
        styler,
    )
}

fn eligible_files(record: &RunRecord, eligible: &[crate::fix::Eligible]) -> crate::Result<crate::HashMap<Utf8PathBuf, usize>> {
    let mut affected = crate::HashMap::default();

    for outcome in record.outcomes() {
        if eligible.iter().any(|entry| entry.outcome() == outcome.outcome) {
            if outcome.site.is_none() {
                return Err(error!(
                    "campaign outcome `{}` in `{}` predates persisted source identities; run a new campaign before suppressing it",
                    outcome.id, outcome.file
                ));
            }

            *affected.entry(outcome.file).or_default() += 1;
        }
    }

    Ok(affected)
}

struct CurrentSources {
    contents: crate::HashMap<Utf8PathBuf, String>,
    generated: crate::HashMap<Utf8PathBuf, Vec<Mutant>>,
    stale: Vec<String>,
}

fn current_sources(root: &Utf8Path, record: &RunRecord, affected: &crate::HashMap<Utf8PathBuf, usize>) -> crate::Result<CurrentSources> {
    let outcomes = record.outcomes();
    let mut contents = crate::HashMap::default();
    let mut generated = crate::HashMap::default();
    let mut stale = Vec::new();

    for (file, eligible_count) in affected {
        let path = root.join(file);
        let source = match fs::read_to_string(&path) {
            Ok(source) => crate::parse::strip_bom(&source).to_owned(),
            Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {
                stale.push(format!(
                    "{file}: source file is missing; no edit was planned for {}",
                    quantity(*eligible_count, "eligible outcome")
                ));
                continue;
            }
            Err(cause) => return Err(error!("could not read `{path}`").caused_by(cause)),
        };
        let parsed = crate::parse::SourceFile::parse(file.clone(), source.clone())?;
        let selectors: Vec<String> = outcomes
            .iter()
            .filter(|outcome| &outcome.file == file)
            .filter_map(|outcome| outcome.site.as_ref().map(|site| site.mutator.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let selection = crate::ops::Selection::parse(&selectors.join(","))?;
        let package = outcomes
            .iter()
            .find(|outcome| &outcome.file == file)
            .map_or("", |outcome| outcome.package.as_str());
        let candidates = crate::ops::collect_candidates(&parsed, &selection);
        let current = crate::ops::into_mutants(&parsed, package, candidates);
        let _previous = generated.insert(file.clone(), current);
        let _previous = contents.insert(file.clone(), source);
    }

    Ok(CurrentSources {
        contents,
        generated,
        stale,
    })
}

fn plan_from_record(root: &Utf8Path, record: &RunRecord, eligible: &[crate::fix::Eligible]) -> crate::Result<(Plan, Vec<String>)> {
    let outcomes = record.outcomes();
    let mut files = Vec::new();
    let mut digests = crate::HashMap::default();
    let mut mutants = Vec::new();
    let affected = eligible_files(record, eligible)?;
    let CurrentSources {
        contents,
        generated,
        mut stale,
    } = current_sources(root, record, &affected)?;

    for outcome in outcomes {
        if !affected.contains_key(&outcome.file) {
            continue;
        }
        let is_eligible = eligible.iter().any(|entry| entry.outcome() == outcome.outcome);
        let Some(site) = outcome.site else { continue };
        let Some(source) = contents.get(&outcome.file) else { continue };

        let identity = format!(
            "{:032x}",
            crate::model::site_key(&site.item, &site.mutator, &crate::model::normalize_site_text(&site.original))
        );
        if identity != site.digest {
            if is_eligible {
                stale.push(format!(
                    "{}:{}: persisted source identity for mutant {} is invalid; no edit was planned",
                    outcome.file, site.line, outcome.id
                ));
            }
            continue;
        }

        let current_digest = crate::discover::digest(source.as_bytes());
        let expected = crate::discover::SiteIdentity {
            file: outcome.file.clone(),
            item: site.item.clone(),
            mutator: site.mutator.clone(),
            normalized_text: site.digest.clone(),
            occurrence: site.occurrence,
        };
        let matches: Vec<&Mutant> = generated
            .get(&outcome.file)
            .into_iter()
            .flatten()
            .filter(|candidate| {
                crate::discover::SiteIdentity::from_mutant(candidate) == expected
                    && candidate.id == outcome.id
                    && candidate.replacement_index == site.replacement_index
                    && candidate.replacement.as_str() == site.replacement
                    && candidate.shape == site.shape
                    && (current_digest != outcome.file_digest
                        || (candidate.span.start == site.span_start
                            && candidate.span.end == site.span_end
                            && candidate.original.as_str() == site.original))
            })
            .collect();

        let [candidate] = matches.as_slice() else {
            if is_eligible {
                stale.push(format!(
                    "{}:{}: source site for mutant {} changed, disappeared, or became ambiguous; no edit was planned",
                    outcome.file, site.line, outcome.id
                ));
            }
            continue;
        };

        let mut mutant = (*candidate).clone();
        mutant.outcome = outcome.outcome;
        mutant.suppression = outcome.suppression;
        mutant.elapsed_ms = 0;
        mutant.killed_by = None;
        mutant.note = None;
        mutants.push(mutant);
    }

    for (path, source) in contents {
        let package = mutants
            .iter()
            .find(|mutant| mutant.file.as_ref() == path)
            .map_or_else(String::new, |mutant| mutant.package.to_string());
        let _previous = digests.insert(path.clone(), crate::discover::digest(source.as_bytes()));
        files.push(TargetFile {
            absolute: root.join(&path),
            path,
            package,
            source: Some(source),
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));

    let mut plan = Plan {
        root: root.to_owned(),
        files,
        mutants,
        suppressed: 0,
        idle: Vec::new(),
        sharded_out: 0,
        settled_out: 0,
        digests,
        skipped: Vec::new(),
        reach: crate::HashMap::default(),
        specs: crate::HashMap::default(),
    };
    apply_source_policy(&mut plan)?;

    Ok((plan, stale))
}

fn apply_source_policy(plan: &mut Plan) -> crate::Result<()> {
    for file in &plan.files {
        let Some(source) = file.source.as_ref() else { continue };
        let parsed = crate::parse::SourceFile::parse(file.path.clone(), source.clone())?;
        let directives = crate::suppress::directives(&parsed)?;
        let mutants: Vec<&mut Mutant> = plan.mutants.iter_mut().filter(|mutant| mutant.file.as_ref() == file.path).collect();

        for mutant in mutants {
            if mutant.outcome != Outcome::Ignored {
                mutant.suppression = None;
            }
            let _count = crate::suppress::suppress(core::slice::from_mut(mutant), &directives);
        }
    }

    Ok(())
}

/// The mutants the edits were written for, which the verification is then allowed to find suppressed.
///
/// The same three conditions `fix::plan` inserted on, restated rather than widened: the line is one
/// an edit was written for, the directive on that line names this mutant's mutator, and this
/// mutant's own verdict was eligible.
///
/// Restating them is the whole point. Deriving the set from the edit *sites* alone — every mutant on
/// a line an edit touched — admits the other mutants sharing that line, and two mutants at one site
/// with the same mutator is ordinary rather than exotic: `x + y + w` yields two `arith.add_to_sub`
/// mutants. Let one time out and the other survive, and the directive written for the timeout
/// suppresses both, because directives match by mutator name and not by occurrence. Told that the
/// survivor was intended, the verification exempts it from the collateral check and reports clean —
/// a survivor out of the denominator with nothing said about it, which is precisely what this
/// command exists to make impossible. The narrower set lets it land in `collateral` instead, where
/// it takes the whole edit back and says so.
///
/// A map rather than a scan of the edits per mutant: both grow with the workspace, and the pairing
/// has no business being quadratic in it.
fn intended(mutants: &[crate::model::Mutant], edits: &[Edit], eligible: &[crate::fix::Eligible]) -> BTreeSet<String> {
    let mut touched: crate::HashMap<(&Utf8Path, usize), BTreeSet<&str>> = crate::HashMap::default();

    for edit in edits {
        touched
            .entry((edit.file.as_path(), edit.line))
            .or_default()
            .extend(edit.mutators.iter().map(String::as_str));
    }

    mutants
        .iter()
        .filter(|mutant| {
            touched
                .get(&(&*mutant.file, mutant.line))
                .is_some_and(|mutators| mutators.contains(&*mutant.mutator))
                && eligible.iter().any(|entry| entry.outcome() == mutant.outcome)
        })
        .map(|mutant| mutant.id.to_string())
        .collect()
}

/// One file a command has rewritten, with both generations the command observed.
///
/// Rollback may restore `before` only when the path still holds `after`. Recording that published
/// generation turns compensation into a compare-and-replace, so a save made after this command's
/// edit remains the editor's save rather than becoming stale rollback input.
#[derive(Debug)]
pub(super) struct WrittenFile {
    path: Utf8PathBuf,
    before: String,
    after: String,
}

impl WrittenFile {
    #[must_use]
    pub(super) const fn new(path: Utf8PathBuf, before: String, after: String) -> Self {
        Self { path, before, after }
    }
}

/// Ordered as they were written, so that putting the tree back is the exact reverse of what was
/// done to it.
pub(super) type Written = Vec<WrittenFile>;

/// Writes every file the plan has an edit for, taking back whatever it wrote if any of them fails.
///
/// The compensation is the point. A read, a parse or a write can fail on file N with files 1 to N-1
/// already rewritten, and returning that error on its own would leave a tree nobody asked for and
/// nothing recorded — for `suppress`, directives standing over mutants that the next run will
/// therefore skip, which is the one thing this command must never do quietly.
#[cfg(test)]
fn apply_all<H: Host>(host: &mut H, args: &SuppressArgs, plan: &Plan, edits: &[Edit], date: &str) -> crate::Result<Written> {
    apply_all_with_lock(host, args, plan, edits, date, false)
}

fn apply_all_locked<H: Host>(host: &mut H, args: &SuppressArgs, plan: &Plan, edits: &[Edit], date: &str) -> crate::Result<Written> {
    apply_all_with_lock(host, args, plan, edits, date, true)
}

fn apply_all_with_lock<H: Host>(
    host: &mut H,
    args: &SuppressArgs,
    plan: &Plan,
    edits: &[Edit],
    date: &str,
    workspace_locked: bool,
) -> crate::Result<Written> {
    if !args.dry_run_suppress {
        let paths: Vec<&Utf8Path> = edits.iter().map(|edit| edit.file.as_path()).collect();

        reject_external_sources(&plan.root, &paths)?;
        recoverable(&plan.root, &paths, args.allow_dirty)?;
    }

    let mut written = Written::new();

    match edit_files(host, args, plan, edits, date, &mut written, workspace_locked) {
        Ok(()) => Ok(written),

        // `written` is handed over rather than borrowed so that the compensation owns what it puts
        // back: whatever this returns, those files are no longer this command's to revert.
        Err(cause) => Err(reverted_with_lock(&plan.root, written, cause, workspace_locked)),
    }
}

/// Refuses every source edit that would leave the workspace before anything is published.
///
/// Checking the whole batch first keeps a later external link from turning a refusal into an
/// edit-and-rollback transaction. `recoverable` also inspects these paths, so it must not reach an
/// external referent first either.
pub(super) fn reject_external_sources(root: &Utf8Path, paths: &[&Utf8Path]) -> crate::Result<()> {
    for path in paths {
        let absolute = root.join(path);
        let _destination = crate::paths::require_within(&absolute, root, "a source edit")?;
    }

    Ok(())
}

/// Refuses to edit a file that nothing on disk could put back.
///
/// The compensation set below lives in process memory and nowhere else. Every failure the loop can
/// see is answered by it, but the interrupt handler kills the process where it stands, and an
/// interrupt after file three of seven leaves four untouched files, three rewritten ones, and no
/// record anywhere of which were which. For `unsuppress` what is left in those three is *deleted*
/// directives, hand-written reasons included, which nothing can reconstruct.
///
/// Rather than keep a journal of its own, this command requires the one every user of it already
/// has: a committed file is recoverable by `git checkout`, and the failure mode above costs a
/// command rather than an afternoon. A file with uncommitted changes has nothing behind it, so it
/// is refused — and the refusal names the flag, because someone working outside version control
/// deliberately is entitled to say so once rather than be stopped forever.
///
/// A tree that is not a repository at all, or a host with no git, gets no opinion: demanding
/// version control from someone who is not using it would be a different command.
///
/// Shared with `unsuppress`, whose edit loop has the same shape and the same hazard.
pub(super) fn recoverable(root: &Utf8Path, paths: &[&Utf8Path], allow_dirty: bool) -> crate::Result<()> {
    if allow_dirty {
        return Ok(());
    }

    // A status with no pathspec is a status of the whole repository, which is a different and much
    // ruder question than the one being asked here.
    if paths.is_empty() {
        return Ok(());
    }

    let Some(dirty) = uncommitted(root, paths) else {
        return Ok(());
    };

    if dirty.is_empty() {
        return Ok(());
    }

    Err(error!(
        "{} about to be edited {} uncommitted changes: {}. This command's rollback lives in this process only, so an interrupt part-way through the edit would leave a tree nothing on disk records — version control is the journal. Commit or stash first, or pass `--allow-dirty` to edit anyway",
        quantity(dirty.len(), "file"),
        if dirty.len() == 1 { "has" } else { "have" },
        dirty.join(", ")
    )
    .usage())
}

/// The paths among `paths` that git reports as changed, or `None` when there is no repository.
///
/// Asked of git rather than derived from the index directly, for the reason the copy path already
/// asks it: a worktree, a submodule and an index format newer than any library understands all
/// answer correctly, and a directory that is not a repository answers by failing.
fn uncommitted(root: &Utf8Path, paths: &[&Utf8Path]) -> Option<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root.as_std_path())
        .args(["status", "--porcelain", "-z", "--"])
        .args(paths.iter().map(|path| path.as_std_path()))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    Some(
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .filter_map(|entry| core::str::from_utf8(entry).ok())
            // Each entry is two status characters, a space, and the path they are about.
            .map(|entry| entry.get(3..).unwrap_or(entry).to_owned())
            .collect(),
    )
}

/// Rewrites each file the plan has an edit for, recording what it held before in `written`.
///
/// Every step is fallible and every step uses `?`, which is what makes the caller's compensation
/// necessary: this stops where it fails and says nothing about the files it already changed beyond
/// what it has put in `written`.
fn edit_files<H: Host>(
    host: &mut H,
    args: &SuppressArgs,
    plan: &Plan,
    edits: &[Edit],
    date: &str,
    written: &mut Written,
    workspace_locked: bool,
) -> crate::Result<()> {
    for file in &plan.files {
        let for_file: Vec<&Edit> = edits.iter().filter(|edit| edit.file == file.path).collect();

        if for_file.is_empty() {
            continue;
        }

        let path = plan.root.join(&file.path);
        let before = fs::read_to_string(&path).map_err(|cause| error!("could not read `{path}`").caused_by(cause))?;
        let source = crate::parse::strip_bom(&before);

        // The line numbers in `for_file` were decided by a discovery that ran before the measured
        // run, which on a real workspace is hours ago, and nothing has held the tree still since.
        // A line number means nothing against text it was not computed from: applied to a file the
        // author has edited, the directive lands on whichever line moved into that position and
        // suppresses a mutant nobody chose, or annotates a statement. Refusing names the one thing
        // the user can act on — re-run — where the alternative is a source edit that looks
        // deliberate.
        //
        // Checked even in a dry run: a diff computed against text that is no longer there is a
        // diff of an edit that will not be made, which is worse than no diff at all.
        if let Some(recorded) = plan.digests.get(&file.path)
            && crate::discover::digest(source.as_bytes()) != *recorded
        {
            return Err(error!(
                "`{path}` changed since the run that planned this edit; nothing was written to it. Re-run to plan against the file as it is now"
            ));
        }

        let mut after = crate::fix::apply(source, &for_file, date);

        if before.len() != source.len() {
            after.insert(0, crate::parse::BOM);
        }

        // Parsing before writing, not after: a patch that does not parse must never reach the disk,
        // because the revert path is only as good as the copy it holds.
        let _ = syn::parse_file(&after).map_err(|cause| error!("the generated directive would not parse in {path}").caused_by(cause))?;

        if args.dry_run_suppress {
            write!(host.results(), "{}", crate::fix::diff(&file.path, &before, &after))?;
        } else {
            // The final comparison happens after the replacement has been staged. The earlier
            // digest protects the long run; this check catches changes through that comparison.
            // The publication API deliberately makes no claim about a non-cooperating replacement
            // in the syscall interval between comparison and rename.
            let publication = if workspace_locked {
                crate::elements::write_if_unchanged_locked(&path, Some(&before), &after)
            } else {
                crate::elements::write_if_unchanged(&plan.root, &path, Some(&before), &after)
            }?;

            match publication {
                Publication::Conflict => {
                    return Err(error!(
                        "`{path}` changed while this command was preparing to publish its edit; the editor's bytes were left alone. Re-run to plan against the file as it is now"
                    ));
                }
                Publication::Published => written.push(WrittenFile::new(path, before, after)),
                Publication::PublishedUndurable(cause) => {
                    // Rename made the source edit visible even though syncing its directory did
                    // not finish. Record it before returning so `apply_all` can compensate it.
                    written.push(WrittenFile::new(path, before, after));
                    return Err(cause);
                }
            }
        }
    }

    Ok(())
}

/// Puts every file in `written` back the way it was, and folds what happened into `cause`.
///
/// Both failures are reported, rather than the second replacing the first. What went wrong first is
/// why the command stopped; whether the tree went back is what the user has to act on, and a
/// message carrying only one of the two sends them to the wrong place — either hunting for an edit
/// that was taken back, or trusting a rollback that did not happen.
///
/// Shared with `unsuppress`, whose edit loop has the same shape and the same hazard.
pub(super) fn reverted(root: &Utf8Path, written: Written, cause: crate::error::Error) -> crate::error::Error {
    reverted_with_lock(root, written, cause, false)
}

fn reverted_with_lock(root: &Utf8Path, written: Written, cause: crate::error::Error, workspace_locked: bool) -> crate::error::Error {
    if written.is_empty() {
        return cause;
    }

    // Backwards, because the compensation set is ordered as it was written and two entries can name
    // one file: a symlinked source directory, or one file reached under two of the plan's paths.
    // Replaying forwards would restore the pristine text and then restore the text that already
    // held the first edit, leaving the file at the intermediate state under a message saying every
    // edit had been taken back.
    let mut stranded = Vec::new();
    let mut undurable = Vec::new();

    for entry in written.into_iter().rev() {
        let publication = if workspace_locked {
            crate::elements::write_if_unchanged_locked(&entry.path, Some(&entry.after), &entry.before)
        } else {
            crate::elements::write_if_unchanged(root, &entry.path, Some(&entry.after), &entry.before)
        };

        match publication {
            Ok(Publication::Conflict) => {
                stranded.push(format!("{} (changed after this command wrote it and was left alone)", entry.path));
            }
            Ok(Publication::Published) => {}
            Ok(Publication::PublishedUndurable(failure)) => {
                undurable.push(format!("{} ({failure})", entry.path));
            }
            Err(failure) => stranded.push(format!("{} ({failure})", entry.path)),
        }
    }

    if stranded.is_empty() {
        if undurable.is_empty() {
            return error!("{cause}; every edit has been reverted");
        }

        return error!(
            "{cause}; every edit has been reverted, but {} could not be made durable because their directories could not be synced: {}",
            quantity(undurable.len(), "file"),
            undurable.join(", ")
        );
    }

    let failure = error!(
        "{cause}; every edit has been reverted except {}, which changed after this command wrote them and were left alone: {}",
        quantity(stranded.len(), "file"),
        stranded.join(", ")
    );

    if undurable.is_empty() {
        return failure;
    }

    error!(
        "{failure}; {} reverted {} could not be made durable because their directories could not be synced: {}",
        quantity(undurable.len(), "file"),
        if undurable.len() == 1 { "edit" } else { "edits" },
        undurable.join(", ")
    )
}

/// Re-runs discovery over the edited tree and reverts unless the suppressed set is exactly right.
///
/// Over-suppression is the hazard: a directive attached to a multi-line construct silently takes out
/// everything inside it, which can include survivors. Checking both directions is what makes an
/// automated source edit something a reviewer can trust without reading every line of it.
struct Applied {
    directives: usize,
    written: Written,
}

fn verify_record_or_revert<H: Host>(
    host: &mut H,
    record: &RunRecord,
    eligible: &[crate::fix::Eligible],
    before: &Plan,
    intended: &BTreeSet<String>,
    applied: Applied,
    styler: Styler,
) -> crate::Result<i32> {
    let verified = plan_from_record(&before.root, record, eligible)
        .map(|(after, _stale)| crate::fix::verify(&before.mutants, &after.mutants, intended));
    finish_verification(host, before, applied.directives, applied.written, styler, verified)
}

#[cfg(test)]
fn verify_or_revert<H: Host>(
    host: &mut H,
    _args: &SuppressArgs,
    before: &Plan,
    intended: &BTreeSet<String>,
    directives: usize,
    written: Written,
    styler: Styler,
) -> crate::Result<i32> {
    let verified = Ok(crate::fix::verify(&before.mutants, &before.mutants, intended));
    finish_verification(host, before, directives, written, styler, verified)
}

fn finish_verification<H: Host>(
    host: &mut H,
    before: &Plan,
    directives: usize,
    written: Written,
    styler: Styler,
    verified: crate::Result<crate::fix::Verification>,
) -> crate::Result<i32> {
    let result = match verified {
        Ok(result) => result,
        Err(cause) => return Err(reverted_with_lock(&before.root, written, cause, true)),
    };

    if result.is_clean() {
        let mut stream = host.error();

        // The directives written, not the mutants they cover: one comment naming three mutators
        // over a line carrying three mutants is one directive, and reporting three would tell the
        // reader to look for two comments that are not there.
        writeln!(
            stream,
            "{} {} in {}",
            styler.verb("Suppressed"),
            quantity(directives, "directive"),
            quantity(written.len(), "file")
        )?;

        writeln!(
            stream,
            "{} every generated directive is tagged; grep for `cargo gamma suppress` to audit them",
            styler.verb("Note")
        )?;

        return Ok(EXIT_OK);
    }

    Err(reverted_with_lock(&before.root, written, unclean(&result, before), true))
}

/// Says which half of the verification failed, and names what it was about.
///
/// All three vectors, because [`crate::fix::Verification::is_clean`] requires all three to be
/// empty: a rollback caused by `released` alone would otherwise print two zeroes and a message about
/// directives missing their target, which is a failure the reader can neither explain nor act on.
/// Stable IDs remain the comparison key, but the diagnostic translates them back to source
/// descriptions so the reader can understand the rejected edit without consulting a report.
fn unclean(result: &crate::fix::Verification, before: &Plan) -> crate::error::Error {
    const SHOWN: usize = 3;

    let by_id: crate::HashMap<&str, &Mutant> = before.mutants.iter().map(|mutant| (&*mutant.id, mutant)).collect();
    let describe = |ids: &[String]| -> String {
        let mut lines: Vec<String> = ids
            .iter()
            .filter_map(|id| by_id.get(id.as_str()))
            .take(SHOWN)
            .map(|mutant| format!("  {} (previous verdict: {})", mutant.describe(), mutant.outcome))
            .collect();

        if let Some(rest) = ids.len().checked_sub(lines.len()).filter(|rest| *rest > 0) {
            lines.push(format!("  and {rest} more"));
        }

        lines.join("\n")
    };
    let mut findings = Vec::new();

    if !result.missing.is_empty() {
        findings.push(format!(
            "{} intended {} not suppressed:\n{}",
            result.missing.len(),
            if result.missing.len() == 1 { "mutant was" } else { "mutants were" },
            describe(&result.missing)
        ));
    }
    if !result.collateral.is_empty() {
        findings.push(format!(
            "{} additional {} suppressed even though the previous verdict was not eligible:\n{}",
            result.collateral.len(),
            if result.collateral.len() == 1 {
                "mutant would be"
            } else {
                "mutants would be"
            },
            describe(&result.collateral)
        ));
    }
    if !result.released.is_empty() {
        findings.push(format!(
            "{} existing {} no longer apply:\n{}",
            result.released.len(),
            if result.released.len() == 1 {
                "suppression would"
            } else {
                "suppressions would"
            },
            describe(&result.released)
        ));
    }

    let scope = if result.collateral.is_empty() {
        ""
    } else {
        "\nSuppression directives apply to syntactic scopes, so one generated directive can cover matching mutants beyond its target site"
    };

    error!(
        "cargo-gamma refused the generated directives because verification found:\n- {}{scope}",
        findings.join("\n- ")
    )
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::fmt::Write as _;

    use super::*;
    use crate::commands::RunArgs;
    #[cfg(unix)]
    use crate::testing::workdir;
    use crate::testing::{Sink, fails_at_every_line};

    fn crate_dir(name: &str) -> tempfile::TempDir {
        crate::fixtures::crate_dir(name, "pub fn answer() -> i32 { 42 }\n").0
    }

    /// Builds a state-only `suppress` invocation against this fixture's completed ledger.
    fn dry_args(root: &Utf8PathBuf, eligible: &str) -> SuppressArgs {
        crate::exec::mark_cache_owned_for_test(root, root);

        SuppressArgs {
            run: RunArgs {
                select: crate::commands::SelectArgs {
                    dir: root.clone(),
                    ..crate::commands::SelectArgs::default()
                },
                measure: crate::commands::MeasureArgs {
                    cache_dir: Some(root.clone()),
                    ..crate::commands::MeasureArgs::default()
                },
                ..RunArgs::default()
            },
            dry_run_suppress: false,
            allow_dirty: false,
            eligible: eligible.to_owned(),
        }
    }

    fn recorded_mutant(root: &Utf8Path, id: &str, outcome: Outcome) -> Mutant {
        recorded_mutant_in(root, "src/lib.rs", "42", id, outcome)
    }

    fn collected_mutants_in(root: &Utf8Path, file: &str, mutator: &str) -> Vec<Mutant> {
        let source = fs::read_to_string(root.join(file)).expect("source");
        let parsed = crate::parse::SourceFile::parse(file, source).expect("parsed source");
        let selection = crate::ops::Selection::parse(mutator).expect("known mutator");
        let candidates = crate::ops::collect_candidates(&parsed, &selection);
        crate::ops::into_mutants(&parsed, "subject", candidates)
    }

    fn recorded_mutant_in(root: &Utf8Path, file: &str, original: &str, _id: &str, outcome: Outcome) -> Mutant {
        let mut mutant = collected_mutants_in(root, file, "literal.int_to_zero")
            .into_iter()
            .find(|mutant| mutant.original.as_str() == original)
            .expect("fixture contains the recorded site");
        mutant.outcome = outcome;
        mutant
    }

    fn store_record(root: &Utf8Path, mutants: &[Mutant]) {
        let context = crate::discover::record_context(&crate::discover::RecordContext {
            toolchain: Some("test"),
            ..crate::discover::RecordContext::default()
        })
        .expect("context");
        RunRecord::from_run(root, mutants, &context, &[root.join("src")]).store(root, root);
    }

    /// A plan over the named files, which is all the edit loop reads out of one.
    fn plan_over(root: &Utf8PathBuf, files: &[&str]) -> Plan {
        Plan {
            skipped: Vec::new(),
            digests: crate::HashMap::default(),
            root: root.clone(),
            files: files
                .iter()
                .map(|name| TargetFile {
                    path: Utf8PathBuf::from(name),
                    absolute: root.join(name),
                    package: "subject".to_owned(),
                    source: None,
                })
                .collect(),
            mutants: Vec::new(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        }
    }

    /// One directive over the first line of `file`.
    fn edit_for(file: &str) -> Edit {
        Edit {
            file: Utf8PathBuf::from(file),
            line: 1,
            mutators: core::iter::once("arith.add_to_sub".to_owned()).collect(),
            tag: "timeout",
        }
    }

    #[test]
    fn empty_eligibility_is_a_usage_error() {
        let mut host = Sink::default();
        let args = SuppressArgs {
            run: RunArgs::default(),
            dry_run_suppress: false,
            allow_dirty: false,
            eligible: String::new(),
        };

        let err = suppress(&mut host, &args, When::Never, Styler::new(false)).unwrap_err();

        assert!(err.is_usage());
    }

    #[test]
    fn clean_verification_reports_what_was_suppressed() {
        let dir = crate_dir("suppress-verify-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let args = SuppressArgs {
            run: RunArgs {
                select: crate::commands::SelectArgs {
                    dir: root.clone(),
                    ..crate::commands::SelectArgs::default()
                },
                ..RunArgs::default()
            },
            dry_run_suppress: false,
            allow_dirty: false,
            eligible: "timeout".to_owned(),
        };
        let before = Plan {
            skipped: Vec::new(),
            digests: crate::HashMap::default(),
            root: root.clone(),
            files: vec![TargetFile {
                path: Utf8PathBuf::from("src/lib.rs"),
                absolute: root.join("src/lib.rs"),
                package: "subject".to_owned(),
                source: None,
            }],
            mutants: Vec::new(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        };
        let mut host = Sink::default();

        let code = verify_or_revert(&mut host, &args, &before, &BTreeSet::new(), 0, Vec::new(), Styler::new(false)).expect("verify");
        let err = String::from_utf8(host.err).expect("utf-8");

        assert_eq!(code, EXIT_OK);
        assert!(err.contains("Suppressed 0 directives"), "{err}");
        assert!(err.contains("grep for `cargo gamma suppress`"), "{err}");
        assert!(host.out.is_empty());
    }

    #[test]
    fn verification_keeps_the_configuration_generation_merged_before_the_edit() {
        let dir = crate_dir("suppress-config-generation-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let config = root.join("gamma.toml");

        fs::create_dir_all(config.parent().expect("configuration parent")).expect("configuration parent");
        fs::write(&config, "cargo-args = [\"--cfg\", \"recorded_configuration\"]\n").expect("configuration");
        let mut args = dry_args(&root, "timeout");
        crate::config::Config::resolve(&args.run.select)
            .expect("initial configuration")
            .apply(&mut args.run)
            .expect("configuration merges");

        // This is the interval between argument merging and verification after a suppression
        // edit. Discovery must use the already merged cargo options rather than re-read this
        // unrelated generation, which is malformed and would previously be swallowed as defaults.
        fs::write(&config, "cargo-args = [\n").expect("broken later configuration");

        let mut host = Sink::default();

        let code = verify_or_revert(
            &mut host,
            &args,
            &plan_over(&root, &["src/lib.rs"]),
            &BTreeSet::new(),
            0,
            Vec::new(),
            Styler::new(false),
        )
        .expect("verification uses its resolved configuration generation");

        assert_eq!(code, EXIT_OK);
    }

    #[test]
    fn a_verification_error_reverts_every_written_file() {
        let dir = crate_dir("suppress-verify-error-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("src/lib.rs");
        let original = fs::read_to_string(&path).expect("original");
        let generated = "// #[gamma::skip]\npub fn answer() -> i32 { 42 }\n";
        fs::write(&path, generated).expect("edited");
        let mut host = Sink::default();

        let failure = finish_verification(
            &mut host,
            &plan_over(&root, &["src/lib.rs"]),
            1,
            vec![WrittenFile::new(path.clone(), original.clone(), generated.to_owned())],
            Styler::new(false),
            Err(error!("injected source verification failure")),
        )
        .expect_err("verification must fail");

        assert!(failure.to_string().contains("every edit has been reverted"), "{failure}");
        assert_eq!(fs::read_to_string(path).expect("restored"), original);
    }

    /// A run that produced no mutants leaves nothing to suppress and is not a failure.
    #[test]
    fn a_run_with_no_mutants_at_all_succeeds_quietly() {
        let dir = crate_dir("suppress-none-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::write(root.join("src/lib.rs"), "pub struct Empty;\n").expect("lib");
        store_record(&root, &[]);

        let mut host = Sink::default();

        let code = suppress(&mut host, &dry_args(&root, "timeout"), When::Never, Styler::new(false)).expect("suppress");

        assert_eq!(code, EXIT_OK);
        assert!(host.err().contains("nothing to suppress"), "{}", host.err());
    }

    /// Mutants that were never run have no eligible verdict, so nothing gets edited.
    #[test]
    fn a_population_with_no_eligible_verdict_edits_nothing() {
        // Determining an `unviable` or `timeout` verdict means building and running the
        let dir = crate_dir("suppress-ineligible-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let source = fs::read_to_string(root.join("src/lib.rs")).expect("read");
        store_record(&root, &[recorded_mutant(&root, "killed", Outcome::Killed)]);

        let mut host = Sink::default();

        let code = suppress(&mut host, &dry_args(&root, "timeout"), When::Never, Styler::new(false)).expect("suppress");

        assert_eq!(code, EXIT_OK);
        assert!(host.err().contains("nothing to suppress"), "{}", host.err());
        assert_eq!(fs::read_to_string(root.join("src/lib.rs")).expect("read"), source);

        fails_at_every_line(1, |host| {
            suppress(host, &dry_args(&root, "timeout"), When::Never, Styler::new(false)).map(|_| ())
        });
    }

    /// Directives that missed their target take the whole edit back rather than leaving it half done.
    #[test]
    fn a_verification_that_fails_reverts_every_file_it_wrote() {
        let dir = crate_dir("suppress-revert-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("src/lib.rs");
        let original = fs::read_to_string(&path).expect("read");

        fs::write(&path, "pub fn answer() -> i32 { 0 }\n").expect("edited");

        let before = Plan {
            skipped: Vec::new(),
            digests: crate::HashMap::default(),
            root: root.clone(),
            files: vec![TargetFile {
                path: Utf8PathBuf::from("src/lib.rs"),
                absolute: path.clone(),
                package: "subject".to_owned(),
                source: None,
            }],
            mutants: Vec::new(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        };

        // Naming an id that no mutant carries makes the intended set impossible to satisfy, which
        // is exactly the shape of an over- or under-reaching directive.
        let intended: BTreeSet<String> = core::iter::once("never-generated".to_owned()).collect();
        let mut host = Sink::default();

        let error = verify_or_revert(
            &mut host,
            &dry_args(&root, "timeout"),
            &before,
            &intended,
            1,
            vec![WrittenFile::new(
                path.clone(),
                original.clone(),
                "pub fn answer() -> i32 { 0 }\n".to_owned(),
            )],
            Styler::new(false),
        )
        .expect_err("verification should fail");

        assert!(error.to_string().contains("every edit has been reverted"), "{error}");
        assert_eq!(fs::read_to_string(&path).expect("read"), original);
    }

    /// A closed stream has to surface from the success report.
    #[test]
    fn a_closed_stream_is_reported_by_the_success_report() {
        let dir = crate_dir("suppress-broken-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let before = Plan {
            skipped: Vec::new(),
            digests: crate::HashMap::default(),
            root: root.clone(),
            files: Vec::new(),
            mutants: Vec::new(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        };

        fails_at_every_line(2, |host| {
            verify_or_revert(
                host,
                &dry_args(&root, "timeout"),
                &before,
                &BTreeSet::new(),
                0,
                Vec::new(),
                Styler::new(false),
            )
            .map(|_| ())
        });
    }

    /// A real, non-dry-run suppress writes the directive to the file that earned one and leaves
    /// every other file in the workspace untouched; touching a file that had nothing eligible in
    /// it would be an edit nobody asked for and nobody could explain from the run's own report.
    #[test]
    fn a_multi_file_run_edits_only_the_file_with_an_eligible_mutant() {
        let dir = crate_dir("suppress-multi-file-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::write(root.join("src/other.rs"), "pub struct Marker;\n").expect("other");
        store_record(&root, &[recorded_mutant(&root, "timeout", Outcome::Timeout)]);
        let args = dry_args(&root, "timeout");
        let mut host = Sink::default();

        let code = suppress(&mut host, &args, When::Never, Styler::new(false)).expect("suppress");

        assert_eq!(code, EXIT_OK);
        assert!(host.err().contains("Suppressed"), "{}", host.err());

        let lib = fs::read_to_string(root.join("src/lib.rs")).expect("lib after");
        let other = fs::read_to_string(root.join("src/other.rs")).expect("other after");

        assert!(lib.contains("gamma::skip"), "{lib}");
        assert!(!other.contains("gamma::skip"), "{other}");
    }

    #[test]
    fn a_workspace_member_resolves_campaign_state_and_sources_from_the_workspace_root() {
        let dir = crate_dir("suppress-workspace-member-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let member = root.join("member");
        fs::create_dir_all(member.join("src")).expect("member source directory");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"root\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\
             [workspace]\nmembers = [\"member\"]\n",
        )
        .expect("workspace manifest");
        fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"member\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .expect("member manifest");
        fs::write(member.join("src/lib.rs"), "pub fn member_answer() -> i32 { 42 }\n").expect("member source");
        let mutant = recorded_mutant_in(&root, "member/src/lib.rs", "42", "timeout", Outcome::Timeout);
        store_record(&root, &[mutant]);
        let mut args = dry_args(&member, "timeout");
        crate::exec::mark_cache_owned_for_test(&root, &root);
        args.run.measure.cache_dir = Some(root);
        args.allow_dirty = true;
        let mut host = Sink::default();

        let code = suppress(&mut host, &args, When::Never, Styler::new(false)).expect("member suppression");

        assert_eq!(code, EXIT_OK);
        assert!(
            fs::read_to_string(member.join("src/lib.rs"))
                .expect("member source after")
                .contains("gamma::skip"),
            "{}",
            host.err()
        );
        assert!(!member.join("gamma-hints.yaml").exists());
    }

    #[test]
    fn persisted_suppression_requires_current_workspace_identity_without_rewriting_evidence() {
        let dir = crate_dir("suppress-persisted-only-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        store_record(&root, &[recorded_mutant(&root, "timeout", Outcome::Timeout)]);
        let source_path = root.join("src/lib.rs");
        let record_path = root.join("last-gamma-run.json");
        let progress_path = root.join("gamma-progress.log");
        let source_before = fs::read(&source_path).expect("source");
        let record_before = fs::read(&record_path).expect("record");
        fs::write(&progress_path, "completed timeout evidence\n").expect("progress");
        let progress_before = fs::read(&progress_path).expect("progress bytes");
        fs::write(root.join("Cargo.toml"), "this is deliberately not Cargo metadata").expect("invalid manifest");
        let mut args = dry_args(&root, "timeout");
        drop(crate::exec::claim_workspace(&root).expect("workspace cache"));
        assert!(crate::exec::remember_campaign_base(&root, &root));
        args.run.measure.cache_dir = None;
        args.allow_dirty = true;
        let mut host = Sink::default();

        let failure = suppress(&mut host, &args, When::Never, Styler::new(false)).expect_err("invalid workspace metadata must fail closed");

        assert!(
            failure.to_string().contains("could not resolve the selected workspace"),
            "{failure}"
        );
        assert_eq!(fs::read(source_path).expect("source after"), source_before);
        assert_eq!(fs::read(record_path).expect("record after"), record_before);
        assert_eq!(fs::read(progress_path).expect("progress after"), progress_before);
    }

    #[test]
    fn suppression_consumes_the_completed_campaign_publication_without_a_synthetic_record() {
        let dir = crate_dir("suppress-completed-publication-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mutant = recorded_mutant(&root, "timeout", Outcome::Timeout);
        let mut plan = plan_over(&root, &["src/lib.rs"]);
        plan.mutants = vec![mutant];
        let inputs = RunRecord::snapshot_with_external(&root, &root.join("cache"), &[], true);
        let context = crate::discover::record_context(&crate::discover::RecordContext {
            toolchain: Some("test"),
            ..crate::discover::RecordContext::default()
        })
        .expect("context");
        let record = RunRecord::from_completed_plan_snapshot(&plan, &context, inputs, &crate::discover::Killers::default())
            .expect("captured campaign source");
        record.store_completed(&root).expect("completed campaign publication");
        let mut args = dry_args(&root, "timeout");
        drop(crate::exec::claim_workspace(&root).expect("workspace cache"));
        assert!(crate::exec::remember_campaign_base(&root, &root));
        args.run.measure.cache_dir = None;
        args.allow_dirty = true;
        let mut host = Sink::default();

        let code = suppress(&mut host, &args, When::Never, Styler::new(false)).expect("persisted suppression");

        assert_eq!(code, EXIT_OK);
        assert!(fs::read_to_string(root.join("src/lib.rs")).expect("source").contains("gamma::skip"));
    }

    #[test]
    fn a_uniquely_moved_recorded_site_is_suppressed_at_its_current_location() {
        let dir = crate_dir("suppress-moved-recorded-site-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        store_record(&root, &[recorded_mutant(&root, "timeout", Outcome::Timeout)]);
        let path = root.join("src/lib.rs");
        let source = fs::read_to_string(&path).expect("source");
        fs::write(&path, format!("// moved after the campaign\n{source}")).expect("moved source");
        let mut args = dry_args(&root, "timeout");
        args.allow_dirty = true;
        let mut host = Sink::default();

        let code = suppress(&mut host, &args, When::Never, Styler::new(false)).expect("moved suppression");

        assert_eq!(code, EXIT_OK);
        let edited = fs::read_to_string(path).expect("edited source");
        assert!(edited.starts_with("// moved after the campaign\n"), "{edited}");
        assert!(edited.contains("gamma::skip"), "{edited}");
    }

    #[test]
    fn a_changed_recorded_site_is_reported_stale_and_not_guessed() {
        let dir = crate_dir("suppress-stale-recorded-site-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        store_record(&root, &[recorded_mutant(&root, "timeout", Outcome::Timeout)]);
        let path = root.join("src/lib.rs");
        let changed = fs::read_to_string(&path).expect("source").replace("42", "43");
        fs::write(&path, &changed).expect("changed source");
        let mut host = Sink::default();

        let code = suppress(&mut host, &dry_args(&root, "timeout"), When::Never, Styler::new(false)).expect("stale outcome");

        assert_eq!(code, EXIT_OK);
        assert!(host.err().contains("changed, disappeared, or became ambiguous"), "{}", host.err());
        assert_eq!(fs::read_to_string(path).expect("source after"), changed);
    }

    #[test]
    fn repeated_operator_text_relocates_by_stable_item_identity() {
        let dir = crate_dir("suppress-repeated-operator-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("src/lib.rs");
        let source = "pub fn first(a: i32, b: i32) -> i32 {\n    a + b\n}\n\
                      pub fn target(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        fs::write(&path, source).expect("source");
        let mut recorded = collected_mutants_in(&root, "src/lib.rs", "arith.add_to_sub")
            .into_iter()
            .find(|mutant| mutant.item_path.contains("target"))
            .expect("target mutant");
        recorded.outcome = Outcome::Timeout;
        store_record(&root, &[recorded]);
        let inserted = "pub fn inserted(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        fs::write(&path, format!("{inserted}{source}")).expect("moved source");
        let mut args = dry_args(&root, "timeout");
        args.allow_dirty = true;
        let mut host = Sink::default();

        let code = suppress(&mut host, &args, When::Never, Styler::new(false)).expect("stable relocation");

        assert_eq!(code, EXIT_OK);
        let edited = fs::read_to_string(path).expect("edited source");
        let target = edited.find("pub fn target").expect("target remains");
        assert!(!edited[..target].contains("gamma::skip"), "{edited}");
        assert!(edited[target..].contains("gamma::skip(arith.add_to_sub"), "{edited}");
    }

    #[test]
    fn repeated_operator_text_does_not_rescue_a_changed_recorded_item() {
        let dir = crate_dir("suppress-repeated-operator-stale-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("src/lib.rs");
        let source = "pub fn first(a: i32, b: i32) -> i32 {\n    a + b\n}\n\
                      pub fn target(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        fs::write(&path, source).expect("source");
        let mut recorded = collected_mutants_in(&root, "src/lib.rs", "arith.add_to_sub")
            .into_iter()
            .find(|mutant| mutant.item_path.contains("target"))
            .expect("target mutant");
        recorded.outcome = Outcome::Timeout;
        store_record(&root, &[recorded]);
        let changed = "pub fn first(a: i32, b: i32) -> i32 {\n    a + b\n}\n\
                       pub fn target(a: i32, b: i32) -> i32 {\n    a - b\n}\n";
        fs::write(&path, changed).expect("changed target");
        let record = RunRecord::load_required(&root).expect("record");

        let (plan, stale) =
            plan_from_record(&root, &record, &crate::fix::Eligible::parse("timeout").expect("eligible")).expect("source-only relocation");

        assert!(plan.mutants.is_empty());
        assert_eq!(stale.len(), 1);
        assert!(stale[0].contains("changed, disappeared, or became ambiguous"), "{}", stale[0]);
    }

    #[test]
    fn files_with_only_ineligible_outcomes_are_not_read() {
        let dir = crate_dir("suppress-affected-files-only-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::write(root.join("src/other.rs"), "pub fn other() -> i32 { 99 }\n").expect("other source");
        let outcomes = [
            recorded_mutant(&root, "timeout", Outcome::Timeout),
            recorded_mutant_in(&root, "src/other.rs", "99", "killed", Outcome::Killed),
        ];
        store_record(&root, &outcomes);
        fs::remove_file(root.join("src/other.rs")).expect("remove ineligible source");
        let mut args = dry_args(&root, "timeout");
        args.allow_dirty = true;
        let mut host = Sink::default();

        let code = suppress(&mut host, &args, When::Never, Styler::new(false)).expect("affected source only");

        assert_eq!(code, EXIT_OK);
        assert!(
            fs::read_to_string(root.join("src/lib.rs"))
                .expect("edited source")
                .contains("gamma::skip")
        );
    }

    #[test]
    fn a_missing_eligible_source_file_is_reported_as_stale() {
        let dir = crate_dir("suppress-missing-source-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        store_record(&root, &[recorded_mutant(&root, "timeout", Outcome::Timeout)]);
        let mut args = dry_args(&root, "timeout");
        drop(crate::exec::claim_workspace(&root).expect("workspace cache"));
        assert!(crate::exec::remember_campaign_base(&root, &root));
        args.run.measure.cache_dir = None;
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"subject\"\nversion = \"0.0.0\"\nedition = \"2024\"\nautolib = false\n\n[workspace]\n",
        )
        .expect("manifest without implicit library");
        fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("remaining package target");
        fs::remove_file(root.join("src/lib.rs")).expect("remove eligible source");
        let mut host = Sink::default();

        let code = suppress(&mut host, &args, When::Never, Styler::new(false)).expect("stale missing source");

        assert_eq!(code, EXIT_OK);
        assert!(host.err().contains("source file is missing"), "{}", host.err());
        assert!(host.err().contains("nothing to suppress"), "{}", host.err());
    }

    #[test]
    fn a_large_persisted_ledger_is_reconstructed_deterministically() {
        let dir = crate_dir("suppress-large-ledger-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut source = String::new();
        for index in 0..2_000 {
            writeln!(source, "pub fn f{index:04}(a: i32) -> i32 {{ a + 1 }}").expect("writing formatted text to a String cannot fail");
        }
        fs::write(root.join("src/lib.rs"), source).expect("large source");
        let mut mutants = collected_mutants_in(&root, "src/lib.rs", "arith.add_to_sub");
        for mutant in &mut mutants {
            mutant.outcome = Outcome::Timeout;
        }
        store_record(&root, &mutants);
        let record = RunRecord::load_required(&root).expect("record");

        let (plan, stale) =
            plan_from_record(&root, &record, &crate::fix::Eligible::parse("timeout").expect("eligible")).expect("ledger reconstruction");

        assert!(stale.is_empty());
        assert_eq!(plan.mutants.len(), 2_000);
        assert_eq!(
            plan.mutants.iter().map(|mutant| mutant.id.clone()).collect::<BTreeSet<_>>().len(),
            2_000
        );
    }

    /// `--dry-run-suppress` writes the diff to stdout instead of touching the source; a caller
    /// previewing a suppression run needs the file to still hold the mutant afterwards, or the
    /// preview would be lying about what it is a preview of.
    #[test]
    fn a_dry_run_suppress_prints_the_diff_and_leaves_the_file_alone() {
        let dir = crate_dir("suppress-dry-run-write-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let source = fs::read_to_string(root.join("src/lib.rs")).expect("lib");
        store_record(&root, &[recorded_mutant(&root, "timeout", Outcome::Timeout)]);
        let mut args = dry_args(&root, "timeout");
        args.dry_run_suppress = true;
        let mut host = Sink::default();

        let code = suppress(&mut host, &args, When::Never, Styler::new(false)).expect("suppress");

        assert_eq!(code, EXIT_OK);
        assert!(host.out().contains("gamma::skip"), "{}", host.out());
        assert_eq!(fs::read_to_string(root.join("src/lib.rs")).expect("lib after"), source);
    }

    /// The two-file case the compensation exists for: the first file is rewritten, the second one
    /// cannot be read, and the command must not leave the first one holding a directive nobody
    /// asked for. A directive left behind that way is worse than a failed command — it silently
    /// removes mutants from the next run, so the score goes up because the tool broke.
    #[test]
    fn a_failure_on_the_second_file_puts_the_first_one_back_byte_for_byte() {
        let dir = crate_dir("suppress-rollback-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let first = root.join("src/lib.rs");
        let original = fs::read(first.as_std_path()).expect("the original bytes");

        // Named so it sorts after the first, and absent so that reading it fails on every platform
        // and for every user, root included.
        let plan = plan_over(&root, &["src/lib.rs", "src/zzz_gone.rs"]);
        let edits = vec![edit_for("src/lib.rs"), edit_for("src/zzz_gone.rs")];
        let mut host = Sink::default();

        let error = apply_all(&mut host, &dry_args(&root, "timeout"), &plan, &edits, "2026-01-01").expect_err("the second file");

        assert!(error.to_string().contains("zzz_gone.rs"), "{error}");
        assert!(error.to_string().contains("every edit has been reverted"), "{error}");
        assert_eq!(
            fs::read(first.as_std_path()).expect("the bytes afterwards"),
            original,
            "the first file was left edited"
        );
    }

    /// The same loop, with nothing in its way, does rewrite both files — so the test above is
    /// asserting that a rollback happened rather than that the loop never got started.
    #[test]
    fn both_files_are_rewritten_when_nothing_fails() {
        let dir = crate_dir("suppress-both-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");

        fs::write(root.join("src/other.rs"), "pub fn other() -> i32 { 7 }\n").expect("other");

        let plan = plan_over(&root, &["src/lib.rs", "src/other.rs"]);
        let edits = vec![edit_for("src/lib.rs"), edit_for("src/other.rs")];
        let mut host = Sink::default();

        let written = apply_all(&mut host, &dry_args(&root, "timeout"), &plan, &edits, "2026-01-01").expect("both files");

        assert_eq!(written.len(), 2);
        assert!(fs::read_to_string(root.join("src/lib.rs")).expect("lib").contains("gamma::skip"));
        assert!(
            fs::read_to_string(root.join("src/other.rs"))
                .expect("other")
                .contains("gamma::skip")
        );
    }

    /// A file edited since the plan was made is refused, not written to by its old line numbers.
    ///
    /// The window here is the whole measured run — hours on a real workspace — and nothing holds
    /// the tree still across it. The line numbers were decided against the text discovery read;
    /// applied to text somebody has since edited they land on whichever line moved into that
    /// position, so a directive appears over a function nobody chose and the mutants it suppresses
    /// go unmeasured with a comment that says the author meant it. Content-addressed mutant ids
    /// catch the *consequence* afterwards and revert, but the message they produce says the edit
    /// "did not suppress what it was meant to", which sends the reader looking for a bug in their
    /// eligibility selection rather than at the file they saved.
    #[test]
    fn a_file_edited_since_the_plan_was_made_is_refused_rather_than_edited_by_stale_line_numbers() {
        let dir = crate_dir("suppress-moved-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("src/lib.rs");
        let planned = fs::read_to_string(path.as_std_path()).expect("the text the plan saw");

        let mut plan = plan_over(&root, &["src/lib.rs"]);
        let _recorded = plan
            .digests
            .insert(Utf8PathBuf::from("src/lib.rs"), crate::discover::digest(planned.as_bytes()));

        // The user saves the file while the run is measuring, which is all it takes: every line
        // number the plan holds is now one line out.
        fs::write(path.as_std_path(), format!("// a note added while the run was going\n{planned}")).expect("the edit");

        let after_edit = fs::read(path.as_std_path()).expect("the bytes as the user left them");
        let edits = vec![edit_for("src/lib.rs")];
        let mut host = Sink::default();

        let error = apply_all(&mut host, &dry_args(&root, "timeout"), &plan, &edits, "2026-01-01").expect_err("the file moved");

        assert!(
            error.to_string().contains("changed since the run that planned this edit"),
            "{error}"
        );
        assert_eq!(
            fs::read(path.as_std_path()).expect("the bytes afterwards"),
            after_edit,
            "the refusal did not leave the user's own edit alone"
        );
    }

    /// The same loop writes the file when it is the one the plan was made against, so the test
    /// above is asserting that the digest refused it rather than that nothing was ever attempted.
    #[test]
    fn a_file_that_still_matches_the_plan_is_edited_as_planned() {
        let dir = crate_dir("suppress-unmoved-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("src/lib.rs");
        let planned = fs::read_to_string(path.as_std_path()).expect("the text the plan saw");

        let mut plan = plan_over(&root, &["src/lib.rs"]);
        let _recorded = plan
            .digests
            .insert(Utf8PathBuf::from("src/lib.rs"), crate::discover::digest(planned.as_bytes()));

        let edits = vec![edit_for("src/lib.rs")];
        let mut host = Sink::default();

        let written = apply_all(&mut host, &dry_args(&root, "timeout"), &plan, &edits, "2026-01-01").expect("the file is unchanged");

        assert_eq!(written.len(), 1);
        assert!(fs::read_to_string(path.as_std_path()).expect("lib").contains("gamma::skip"));
    }

    /// Discovery strips a leading BOM before hashing and locating lines. Suppression must compare
    /// that same normalized source, then put the mark back before the first generated directive so
    /// it remains a file marker rather than becoming an in-source character.
    #[test]
    fn bom_prefixed_files_match_the_plan_and_keep_the_bom_for_first_and_later_edits() {
        for (name, source, line, expected) in [
            (
                "first",
                "\u{feff}pub fn first() -> i32 { 1 }\n",
                1,
                "\u{feff}// #[gamma::skip(arith.add_to_sub,",
            ),
            (
                "later",
                "\u{feff}pub fn first() -> i32 { 1 }\npub fn later() -> i32 { 2 }\n",
                2,
                "\u{feff}pub fn first() -> i32 { 1 }\n// #[gamma::skip(arith.add_to_sub,",
            ),
        ] {
            let dir = crate_dir(&format!("suppress-bom-{name}-"));
            let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
            let path = root.join("src/lib.rs");
            fs::write(&path, source).expect("bom source");

            let mut plan = plan_over(&root, &["src/lib.rs"]);
            let _recorded = plan.digests.insert(
                Utf8PathBuf::from("src/lib.rs"),
                crate::discover::digest(crate::parse::strip_bom(source).as_bytes()),
            );
            let edit = Edit {
                line,
                ..edit_for("src/lib.rs")
            };

            let written =
                apply_all(&mut Sink::default(), &dry_args(&root, "timeout"), &plan, &[edit], "2026-01-01").expect("apply bom edit");
            let after = fs::read_to_string(path).expect("edited source");

            assert_eq!(written.len(), 1, "{name}");
            assert!(after.starts_with(expected), "{name}: {after:?}");
            assert_eq!(after.chars().next(), Some(crate::parse::BOM), "{name}: {after:?}");
        }
    }

    /// The final publication check, unlike the discovery digest, guards the short interval after
    /// the replacement has been prepared. An editor save there is a conflict, not a generated
    /// directive over text the command did not inspect.
    #[test]
    fn a_save_after_validation_and_before_suppress_publication_is_left_alone() {
        let dir = crate_dir("suppress-publication-conflict-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("src/lib.rs");
        let editor = "// saved by the editor\npub fn answer() -> i32 { 43 }\n".to_owned();
        let editor_path = path.clone();

        crate::elements::before_next_publication(move |_| {
            fs::write(editor_path, &editor).expect("the editor save");
        });

        let error = apply_all(
            &mut Sink::default(),
            &dry_args(&root, "timeout"),
            &plan_over(&root, &["src/lib.rs"]),
            &[edit_for("src/lib.rs")],
            "2026-01-01",
        )
        .expect_err("the generation changed after validation");

        assert!(
            error.to_string().contains("changed while this command was preparing to publish"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(path).expect("the editor's bytes"),
            "// saved by the editor\npub fn answer() -> i32 { 43 }\n"
        );
    }

    /// A directory sync comes after the rename, so this is the failure where the source was
    /// changed even though the writer returned an error. The entry must reach `written` before
    /// that error escapes, or compensation has no way to put the directive back.
    #[test]
    fn a_post_rename_suppress_sync_failure_is_reverted() {
        let dir = crate_dir("suppress-sync-failure-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("src/lib.rs");
        let original = fs::read_to_string(&path).expect("original");

        crate::elements::fail_next_directory_sync();

        let error = apply_all(
            &mut Sink::default(),
            &dry_args(&root, "timeout"),
            &plan_over(&root, &["src/lib.rs"]),
            &[edit_for("src/lib.rs")],
            "2026-01-01",
        )
        .expect_err("the post-rename sync fails");

        assert!(error.to_string().contains("injected directory sync failure"), "{error}");
        assert!(error.to_string().contains("every edit has been reverted"), "{error}");
        assert_eq!(fs::read_to_string(path).expect("source restored"), original);
    }

    /// A write that cannot be staged leaves the source exactly as it was, rather than truncated.
    ///
    /// This is the injected partial write: blocking the staging file stands in for the disk filling
    /// up or the process being killed between the truncate and the replacement bytes, which is what
    /// writing straight into the file left no answer for.
    #[test]
    fn a_write_that_cannot_be_staged_leaves_the_source_untouched() {
        let dir = crate_dir("suppress-partial-write-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("src/lib.rs");
        let original = fs::read(path.as_std_path()).expect("the original bytes");

        let scratch = root.join(".blocked-stage");
        fs::create_dir(scratch.as_std_path()).expect("block the staging file");
        crate::elements::next_scratch_path(scratch);

        let plan = plan_over(&root, &["src/lib.rs"]);
        let edits = vec![edit_for("src/lib.rs")];
        let mut host = Sink::default();

        let error = apply_all(&mut host, &dry_args(&root, "timeout"), &plan, &edits, "2026-01-01").expect_err("the write");

        assert!(error.to_string().contains("lib.rs"), "{error}");
        assert_eq!(
            fs::read(path.as_std_path()).expect("the bytes afterwards"),
            original,
            "a failed write did not leave the source alone"
        );
    }

    /// When the rollback itself cannot be done, both failures are reported: the one that stopped
    /// the command, and the fact that the tree is not as it was found. Reporting only the first
    /// would tell the user their source is back when it is not.
    #[test]
    fn a_rollback_that_fails_is_reported_with_what_went_wrong_first() {
        let dir = crate_dir("suppress-rollback-failure-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("src");

        // A directory cannot be replaced by a file on any platform, so restoring this one fails
        // for a reason no permission model can talk the test out of.
        let written = vec![WrittenFile::new(
            path,
            "pub fn answer() -> i32 { 42 }\n".to_owned(),
            "// #[gamma::skip]\npub fn answer() -> i32 { 42 }\n".to_owned(),
        )];
        let error = reverted(&root, written, error!("could not read `src/zzz_gone.rs`"));
        let text = error.to_string();

        assert!(text.contains("could not read `src/zzz_gone.rs`"), "{text}");
        assert!(text.contains("changed after this command wrote them"), "{text}");
        assert!(text.contains("src"), "{text}");
    }

    /// Nothing written means nothing to revert, and no claim that anything was.
    #[test]
    fn a_failure_before_the_first_write_makes_no_claim_about_reverting() {
        let error = reverted(Utf8Path::new("."), Written::new(), error!("could not read `src/lib.rs`"));

        assert_eq!(error.to_string(), "could not read `src/lib.rs`");
    }

    /// A failed verification must not put stale source back over a save made after this command
    /// published its directive. The rollback has the same generation guard as publication.
    #[test]
    fn a_save_before_suppress_rollback_is_left_alone() {
        let dir = crate_dir("suppress-rollback-conflict-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("src/lib.rs");
        let original = "pub fn answer() -> i32 { 42 }\n";
        let generated = "// #[gamma::skip(arith.add_to_sub)]\npub fn answer() -> i32 { 42 }\n";
        let editor = "// saved by the editor\npub fn answer() -> i32 { 43 }\n".to_owned();
        let editor_path = path.clone();

        fs::write(&path, generated).expect("the generated edit");
        crate::elements::before_next_publication(move |_| {
            fs::write(editor_path, &editor).expect("the editor save");
        });

        let error = reverted(
            &root,
            vec![WrittenFile::new(path.clone(), original.to_owned(), generated.to_owned())],
            error!("verification failed"),
        );

        assert!(error.to_string().contains("changed after this command wrote it"), "{error}");
        assert_eq!(
            fs::read_to_string(path).expect("the editor's bytes"),
            "// saved by the editor\npub fn answer() -> i32 { 43 }\n"
        );
    }

    /// The compensating rename can reach the original bytes before its directory sync fails. The
    /// error must retain that durability failure without claiming the source was left edited.
    #[test]
    fn a_post_rename_rollback_sync_failure_is_reported_as_restored() {
        let dir = crate_dir("suppress-rollback-sync-failure-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("src/lib.rs");
        let original = "pub fn answer() -> i32 { 42 }\n";
        let generated = "// #[gamma::skip(arith.add_to_sub)]\npub fn answer() -> i32 { 42 }\n";

        fs::write(&path, generated).expect("the generated edit");
        crate::elements::fail_next_directory_sync();

        let error = reverted(
            &root,
            vec![WrittenFile::new(path.clone(), original.to_owned(), generated.to_owned())],
            error!("verification failed"),
        );

        assert!(error.to_string().contains("injected directory sync failure"), "{error}");
        assert!(error.to_string().contains("every edit has been reverted, but 1 file"), "{error}");
        assert_eq!(fs::read_to_string(path).expect("source restored"), original);
    }

    /// One file can reach the compensation set twice, and only the bytes found first are the
    /// user's. Replaying forwards restores those and then overwrites them with the intermediate
    /// text, which is worse than not reverting at all: the message says the tree is as it was.
    #[test]
    fn a_file_written_twice_is_restored_to_the_bytes_that_were_there_first() {
        let dir = crate_dir("suppress-rollback-order-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("src/lib.rs");

        let original = "pub fn answer() -> i32 { 42 }\n";
        let first = "// #[gamma::skip(arith.add_to_sub)]\npub fn answer() -> i32 { 42 }\n";
        let second = "// #[gamma::skip(arith.add_to_sub)]\n// #[gamma::skip(arith.add_to_sub)]\npub fn answer() -> i32 { 42 }\n";
        fs::write(&path, second).expect("the second generated version");
        let written = vec![
            WrittenFile::new(path.clone(), original.to_owned(), first.to_owned()),
            WrittenFile::new(path.clone(), first.to_owned(), second.to_owned()),
        ];
        let error = reverted(&root, written, error!("could not read `src/other.rs`"));

        assert!(error.to_string().contains("every edit has been reverted"), "{error}");
        assert_eq!(
            fs::read_to_string(&path).expect("the file afterwards"),
            "pub fn answer() -> i32 { 42 }\n",
            "the rollback left the file holding an edit this command made"
        );
    }

    /// Builds a mutant at a named site with a named verdict.
    fn mutant(id: &str, line: usize, mutator: &str, outcome: crate::model::Outcome) -> crate::model::Mutant {
        crate::model::Mutant {
            id: id.to_owned().into(),
            line,
            mutator: (mutator.to_owned()).into(),
            outcome,
            ..crate::fixtures::mutant()
        }
    }

    /// Marks a mutant as suppressed, the way the apply pass does once a directive names it.
    fn suppressed(mutant: &crate::model::Mutant) -> crate::model::Mutant {
        crate::model::Mutant {
            suppression: Some(crate::model::Suppression {
                channel: crate::model::Channel::Comment,
                reason: None,
                tag: Some("timeout".to_owned()),
                line: Some(mutant.line),
            }),
            ..mutant.clone()
        }
    }

    /// The critical one. Two mutants of the same mutator at one site is ordinary — `x + y + w` has
    /// two `arith.add_to_sub` occurrences — and if one times out and the other survives, the
    /// directive written for the timeout suppresses both, because matching is by mutator name.
    /// Exempting the survivor from the collateral check would take a survivor out of the
    /// denominator and report success, which is the one thing this command may never do.
    #[test]
    fn a_survivor_sharing_a_site_with_a_timeout_is_caught_as_collateral() {
        let eligible = crate::fix::Eligible::parse("timeout").expect("eligibility");
        let before = vec![
            mutant("timed-out", 2, "arith.add_to_sub", crate::model::Outcome::Timeout),
            mutant("survivor", 2, "arith.add_to_sub", crate::model::Outcome::Survived),
        ];
        let edits = crate::fix::plan(&before, &eligible);

        let intended = intended(&before, &edits, &eligible);

        assert_eq!(intended, core::iter::once("timed-out".to_owned()).collect::<BTreeSet<String>>());

        // What the directive actually does to the file: both occurrences carry that mutator name.
        let after: Vec<crate::model::Mutant> = before.iter().map(suppressed).collect();
        let result = crate::fix::verify(&before, &after, &intended);

        assert!(!result.is_clean(), "a suppressed survivor was reported as a clean edit");
        assert_eq!(result.collateral, vec!["survivor".to_owned()]);
    }

    /// The other half of the same mistake, in the harmless-looking direction: a killed mutant
    /// sharing a line with a timeout is not named by the directive and is not meant to be
    /// suppressed, so requiring it to be suppressed reverts a perfectly good edit and blames a
    /// directive placement problem that does not exist.
    #[test]
    fn a_killed_mutant_sharing_a_line_is_not_required_to_be_suppressed() {
        let eligible = crate::fix::Eligible::parse("timeout").expect("eligibility");
        let before = vec![
            mutant("timed-out", 2, "arith.add_to_sub", crate::model::Outcome::Timeout),
            mutant("killed", 2, "stmt.delete", crate::model::Outcome::Killed),
        ];
        let edits = crate::fix::plan(&before, &eligible);
        let intended = intended(&before, &edits, &eligible);

        // Only the timeout's mutator is named, so only the timeout is suppressed.
        let after = vec![suppressed(&before[0]), before[1].clone()];
        let result = crate::fix::verify(&before, &after, &intended);

        assert!(result.is_clean(), "{result:?}");
        assert!(result.missing.is_empty(), "{:?}", result.missing);
    }

    /// Timeout and memory exhaustion get different tags and therefore different edits. Both edits
    /// can still land on one statement, and intent tracking must retain both selector sets rather
    /// than letting the later edit replace the earlier one.
    #[test]
    fn separately_tagged_directives_on_one_line_are_all_intended() {
        let eligible = crate::fix::Eligible::parse("timeout,outofmem").expect("eligibility");
        let before = vec![
            mutant("timeout", 2, "loop.break_to_continue", crate::model::Outcome::Timeout),
            mutant("oom", 2, "loop.delete_break", crate::model::Outcome::OutOfMemory),
        ];
        let edits = crate::fix::plan(&before, &eligible);

        assert_eq!(edits.len(), 2, "{edits:?}");
        assert_eq!(
            intended(&before, &edits, &eligible),
            ["oom".to_owned(), "timeout".to_owned()].into_iter().collect()
        );
    }

    /// The count belongs to the comments written, not to the mutants they cover: three mutants at
    /// one site are one directive, and reporting three sends the reader looking for two comments
    /// that were never written.
    #[test]
    fn the_success_line_counts_directives_rather_than_mutants() {
        let dir = crate_dir("suppress-count-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut host = Sink::default();
        let before = plan_over(&root, &["src/lib.rs"]);

        let code = verify_or_revert(
            &mut host,
            &dry_args(&root, "timeout"),
            &before,
            &["a".to_owned(), "b".to_owned(), "c".to_owned()].into_iter().collect(),
            1,
            Vec::new(),
            Styler::new(false),
        );

        // The intended set is unsatisfiable here, so the message is the failure's; what matters is
        // that the clean path below counts what it was told rather than what it verified.
        let _ = code.expect_err("an unsatisfiable intended set");

        let mut host = Sink::default();
        let code = verify_or_revert(
            &mut host,
            &dry_args(&root, "timeout"),
            &before,
            &BTreeSet::new(),
            1,
            Vec::new(),
            Styler::new(false),
        )
        .expect("verify");

        assert_eq!(code, EXIT_OK);
        assert!(host.err().contains("Suppressed 1 directive in"), "{}", host.err());
    }

    /// A rollback caused by a directive that stopped suppressing something must not print two
    /// zeroes and a sentence about directives missing their target, which is a failure the reader
    /// can neither explain nor act on.
    #[test]
    fn a_verification_failure_names_which_half_failed_and_what_it_was_about() {
        let dir = crate_dir("suppress-verification-message-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut before = plan_over(&root, &["src/lib.rs"]);
        before.mutants = vec![
            mutant("one", 2, "arith.add_to_sub", crate::model::Outcome::Killed),
            mutant("two", 3, "stmt.delete", crate::model::Outcome::Survived),
            mutant("three", 4, "cond.negate", crate::model::Outcome::Timeout),
            mutant("four", 5, "loop.delete_break", crate::model::Outcome::OutOfMemory),
        ];
        let released = crate::fix::Verification {
            missing: Vec::new(),
            collateral: Vec::new(),
            released: vec!["one".to_owned(), "two".to_owned(), "three".to_owned(), "four".to_owned()],
        };
        let text = unclean(&released, &before).to_string();

        assert!(text.contains("4 existing suppressions would no longer apply"), "{text}");
        assert!(text.contains("src/lib.rs:2:"), "{text}");
        assert!(text.contains("previous verdict: killed"), "{text}");
        assert!(text.contains("and 1 more"), "{text}");
        for id in ["one", "two", "three", "four"] {
            assert!(!text.contains(id), "internal mutant ID `{id}` leaked into:\n{text}");
        }
    }

    #[test]
    fn a_collateral_suppression_error_explains_scope_and_names_source_not_ids() {
        let dir = crate_dir("suppress-collateral-message-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut before = plan_over(&root, &["src/lib.rs"]);
        before.mutants = vec![mutant("opaque-id", 2, "arith.add_to_sub", crate::model::Outcome::Killed)];
        let collateral = crate::fix::Verification {
            missing: Vec::new(),
            collateral: vec!["opaque-id".to_owned()],
            released: Vec::new(),
        };

        let text = unclean(&collateral, &before).to_string();

        assert!(text.contains("1 additional mutant would be suppressed"), "{text}");
        assert!(text.contains("src/lib.rs:2:"), "{text}");
        assert!(text.contains("[arith.add_to_sub]"), "{text}");
        assert!(text.contains("previous verdict: killed"), "{text}");
        assert!(text.contains("directives apply to syntactic scopes"), "{text}");
        assert!(!text.contains("opaque-id"), "{text}");
    }

    /// Version control is the only journal this command has, and an interrupt part-way through the
    /// edit loop leaves a tree nothing on disk records. A file with uncommitted changes has nothing
    /// behind it, so it is refused before the first write rather than after the third.
    #[test]
    fn an_edit_over_a_file_with_uncommitted_changes_is_refused_before_anything_is_written() {
        let dir = crate_dir("suppress-dirty-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("src/lib.rs");
        let original = fs::read_to_string(&path).expect("the original");

        if !git(&root, &["init", "--quiet"]) {
            return;
        }

        let plan = plan_over(&root, &["src/lib.rs"]);
        let edits = vec![edit_for("src/lib.rs")];
        let mut args = dry_args(&root, "timeout");
        let mut host = Sink::default();

        let error = apply_all(&mut host, &args, &plan, &edits, "2026-01-01").expect_err("a dirty tree");

        assert!(error.is_usage(), "{error}");
        assert!(error.to_string().contains("src/lib.rs"), "{error}");
        assert!(error.to_string().contains("--allow-dirty"), "{error}");
        assert_eq!(
            fs::read_to_string(&path).expect("afterwards"),
            original,
            "the refusal still wrote the file"
        );

        // Someone working outside version control deliberately is entitled to say so.
        args.allow_dirty = true;

        let _ = apply_all(&mut host, &args, &plan, &edits, "2026-01-01").expect("the override");

        assert_ne!(fs::read_to_string(&path).expect("afterwards"), original);
    }

    #[cfg(unix)]
    #[test]
    fn applying_refuses_the_whole_batch_when_a_source_symlink_leaves_the_workspace() {
        let dir = crate_dir("suppress-external-link-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let first = root.join("src/lib.rs");
        let first_before = fs::read_to_string(&first).expect("first source");
        let second = root.join("src/other.rs");
        let external = workdir("suppress-external-referent-");
        let outside = Utf8PathBuf::from_path_buf(external.path().join("outside.rs")).expect("utf8");
        let outside_before = "pub fn other() -> i32 { 7 }\n";

        fs::write(&outside, outside_before).expect("external source");
        std::os::unix::fs::symlink(outside.as_std_path(), second.as_std_path()).expect("source link");

        let plan = plan_over(&root, &["src/lib.rs", "src/other.rs"]);
        let edits = vec![edit_for("src/lib.rs"), edit_for("src/other.rs")];
        let mut args = dry_args(&root, "timeout");

        args.allow_dirty = true;

        let error = apply_all(&mut Sink::default(), &args, &plan, &edits, "2026-01-01")
            .expect_err("an external source referent must refuse the batch");

        assert!(error.to_string().contains("outside"), "{error}");
        assert_eq!(fs::read_to_string(first).expect("first source"), first_before);
        assert_eq!(fs::read_to_string(outside).expect("external source"), outside_before);
    }

    /// Demanding version control from someone who is not using it would be a different command,
    /// so a tree that is not a repository gets no opinion.
    #[test]
    fn a_tree_that_is_not_a_repository_is_edited_without_complaint() {
        let dir = crate_dir("suppress-no-repo-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");

        recoverable(&root, &[Utf8Path::new("src/lib.rs")], false).expect("no repository, no opinion");
    }

    /// State-consuming suppression does not manufacture a replacement campaign when none exists.
    #[test]
    fn a_missing_campaign_record_is_reported_without_running_the_workspace() {
        let dir = crate_dir("suppress-missing-record-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let source = fs::read_to_string(root.join("src/lib.rs")).expect("the fixture");
        let mut host = Sink::default();

        let error = suppress(&mut host, &dry_args(&root, "timeout"), When::Never, Styler::new(false)).expect_err("missing persisted state");

        assert!(error.to_string().contains("could not read campaign state"), "{error}");
        assert_eq!(fs::read_to_string(root.join("src/lib.rs")).expect("afterwards"), source);
    }

    /// Runs git in `root`, reporting whether it could be run at all.
    fn git(root: &Utf8Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(root.as_std_path())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}
