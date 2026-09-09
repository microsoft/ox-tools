// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Driver for a single managed region.
//!
//! Given the host file's current text, a region id, and the rendered
//! region body, this module locates the region (if present), consults the
//! manifest, computes the decision, and returns a [`PlanItem`] ready to be
//! applied.
//!
//! The host text is supplied by the caller rather than read here, so that
//! multiple regions targeting the same host file compose: the caller
//! threads an accumulating in-memory host text (seeded from disk) through
//! every region, and each region splices on top of the previous one's
//! result instead of re-reading the original disk state. See
//! [`crate::run`]'s `HostTextCache` and
//! [`updates.md`](../../../docs/design/updates.md).

use std::collections::BTreeSet;

use ohno::{AppError, app_err};
use toml_edit::DocumentMut;

use crate::checksum::checksum_str;
#[cfg(test)]
use crate::decision::Decision;
use crate::manifest::{Manifest, RegionKey};
use crate::plan::{PlanItem, Target};
use crate::region::{
    CommentSyntax, RegionPlacement, TomlAdoption, adopt_unmanaged_toml_tables, find_region, insert_after_region,
    mask_retiring_managed_regions, start_region_offset, text_newline, upsert_region_with_newline,
};

/// Inputs that identify and render one managed region.
#[derive(Clone, Copy)]
pub struct ManagedRegionRequest<'a> {
    /// Repo-root-relative forward-slash path of the host file.
    pub host_relpath: &'a str,
    /// Stable identifier written into the region sentinels.
    pub region_id: &'a str,
    /// Template content rendered between the sentinels using the host's line endings.
    pub rendered_body: &'a str,
    /// Comment flavor used by the host file.
    pub syntax: CommentSyntax,
    /// Required position of the region within the host file.
    pub placement: RegionPlacement,
    /// Original host style, captured before marker cleanup or adoption.
    pub newline: Option<&'a str>,
}

impl ManagedRegionRequest<'_> {
    #[cfg(test)]
    fn at_end<'a>(host_relpath: &'a str, region_id: &'a str, rendered_body: &'a str, syntax: CommentSyntax) -> ManagedRegionRequest<'a> {
        ManagedRegionRequest {
            host_relpath,
            region_id,
            rendered_body,
            syntax,
            placement: RegionPlacement::End,
            newline: None,
        }
    }
}

/// Compute the [`PlanItem`] for a managed region.
///
/// `host_text` is the host file's current content — `None`
/// when the host file does not (yet) exist — which for the second and
/// later regions in one host is the in-memory result of splicing the
/// earlier regions, not the original disk state. `request` identifies
/// the host and region and carries its rendered body, comment flavor,
/// and placement.
///
/// If the host text is `None`, the region is treated as a `Write` and
/// the spliced output will be just the rendered region (sentinels + body).
///
/// # Errors
///
/// Returns an error if the region in the host is malformed.
pub fn plan_managed_region(manifest: &Manifest, host_text: Option<&str>, request: ManagedRegionRequest<'_>) -> Result<PlanItem, AppError> {
    let ManagedRegionRequest {
        host_relpath,
        region_id,
        rendered_body,
        syntax,
        placement,
        newline,
    } = request;
    let template_checksum = checksum_str(rendered_body);
    let key = RegionKey {
        host: host_relpath.to_owned(),
        id: region_id.to_owned(),
    };
    let last_rendered = manifest.regions.get(&key).map(String::as_str);

    let disk_region = match host_text {
        None => None,
        Some(text) => find_region(text, region_id, syntax)?,
    };
    let disk_checksum = disk_region.as_ref().map(|region| checksum_str(region.body_str()));
    let needs_reposition = placement == RegionPlacement::Start
        && disk_region
            .as_ref()
            .is_some_and(|region| region.start_line.start != start_region_offset(host_text.unwrap_or(""), syntax));

    let target = Target::Region {
        host: host_relpath.to_owned(),
        id: region_id.to_owned(),
    };
    if disk_checksum.as_deref() == Some(&template_checksum) && !needs_reposition {
        return Ok(PlanItem::insync(target, template_checksum));
    }
    if disk_region.as_ref().is_some_and(|region| !region.is_empty())
        && disk_checksum.as_deref() != last_rendered
        && disk_checksum.as_deref() != Some(&template_checksum)
    {
        return Err(app_err!(
            "the managed region contains edits that do not match its last render or the current template. \
             Restore its generated content, or empty its body to regenerate it; keep user settings outside the sentinels"
        ));
    }
    let spliced = splice(host_relpath, host_text, region_id, rendered_body, syntax, placement, newline)?;
    Ok(PlanItem::write_region(
        host_relpath,
        region_id,
        rendered_body.to_owned(),
        spliced,
        template_checksum,
    ))
}

/// Why writing `request`'s region into its TOML host would produce a file
/// TOML cannot read, if it would.
///
/// This is the backstop for the whole class of failure behind issue #148:
/// splicing a region that declares a whole table beside a hand-written copy of
/// that table yields two identical headers, which TOML rejects outright — and
/// the generator had already rewritten the file and recorded the region by the
/// time anything noticed. Adoption resolves the cases it can model; this
/// catches whatever is left by asking the parser, rather than by enumerating
/// shapes.
///
/// Both introducing a region and updating one are checked. An update used to
/// be assumed safe, on the grounds that replacing a region where it stands
/// cannot add a header — but the *body* can: a template that gains a table the
/// host already declares by hand collides on the next run, from a host that
/// was perfectly valid before it.
///
/// `retiring` names the managed regions this pass removes from the host. They
/// are blanked and everything else is judged as written, so a migration that is
/// about to become valid is not refused while a sibling that is *staying* is
/// still seen. The alternative — masking every region but this one — is what
/// let two regions of the catalog compose into a duplicate header with neither
/// able to see it.
///
/// Returns `None` for a host that is not TOML and for a splice whose result
/// parses.
#[must_use]
pub fn toml_introduction_refusal(
    host_text: Option<&str>,
    request: ManagedRegionRequest<'_>,
    retiring: &BTreeSet<String>,
) -> Option<String> {
    let ManagedRegionRequest {
        host_relpath,
        region_id,
        rendered_body,
        syntax,
        placement,
        newline,
    } = request;
    if !is_toml_host(host_relpath) {
        return None;
    }
    let base = host_text.unwrap_or("");
    // A malformed region is a separate diagnosis, raised by the planner.
    if find_region(base, region_id, syntax).is_err() {
        return None;
    }

    let spliced = match splice(host_relpath, host_text, region_id, rendered_body, syntax, placement, newline) {
        Err(error) => return Some(error.to_string()),
        Ok(spliced) => spliced,
    };
    let error = mask_retiring_managed_regions(&spliced, syntax, retiring)
        .parse::<DocumentMut>()
        .err()?;
    Some(format!(
        "splicing the region would leave {host_relpath} unparsable as TOML: {error}"
    ))
}

fn is_toml_host(host_relpath: &str) -> bool {
    std::path::Path::new(host_relpath)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"))
}

fn splice(
    host_relpath: &str,
    host_text: Option<&str>,
    region_id: &str,
    rendered_body: &str,
    syntax: CommentSyntax,
    placement: RegionPlacement,
    newline: Option<&str>,
) -> Result<String, AppError> {
    let base = host_text.unwrap_or("");
    let newline = newline.unwrap_or_else(|| text_newline(base));

    // Writing a region into a TOML host: adopt any hand-written copy of the
    // tables the body declares, rather than appending a duplicate that TOML
    // will refuse to parse.
    //
    // This runs on updates as well as introductions. It was once scoped to
    // introductions, on the reasoning that replacing an existing region in
    // place cannot add a header — true of the splice, but not of the body: a
    // template that gains a table the host declares by hand collides on the
    // next run. Reconciling it here is what spares the user an edit they would
    // have to reverse-engineer, since the correct one (drop the header, move
    // the extras below the closing sentinel) is not something a diagnostic can
    // usefully describe. On an ordinary update there is nothing to adopt:
    // adoption masks every managed region before parsing, so tables the region
    // already owns are invisible and it returns `Unchanged`.
    let adopted;
    let mut residue = String::new();
    let base = if is_toml_host(host_relpath) {
        match adopt_unmanaged_toml_tables(base, rendered_body, syntax) {
            TomlAdoption::Unchanged => base,
            TomlAdoption::Adopted { text, residue: kept } => {
                residue = kept;
                adopted = text;
                adopted.as_str()
            }
            TomlAdoption::Conflict {
                table,
                key,
                managed,
                hand_written,
            } => {
                return Err(app_err!(
                    "{host_relpath} declares `{key}` in `[{table}]` as {hand_written}, but the managed \
                     region '{region_id}' declares it as {managed}. Adopting the table would discard \
                     one of them and keeping both would repeat the key, which TOML rejects."
                ));
            }
            TomlAdoption::Unrelocatable { table, tail_table } => {
                return Err(app_err!(
                    "{host_relpath} declares settings in `[{table}]` that the managed region \
                     '{region_id}' does not, and the region's body ends in `[{tail_table}]`, so \
                     re-emitting them after the region would make them settings of `[{tail_table}]` \
                     instead. Remove them from `[{table}]` and re-run."
                ));
            }
        }
    } else {
        base
    };

    let spliced = upsert_region_with_newline(base, region_id, rendered_body, syntax, placement, newline)?;
    insert_after_region(&spliced, region_id, &residue, syntax)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    const SYN: CommentSyntax = CommentSyntax::Hash;

    fn request<'a>(host_relpath: &'a str, region_id: &'a str, rendered_body: &'a str) -> ManagedRegionRequest<'a> {
        ManagedRegionRequest::at_end(host_relpath, region_id, rendered_body, SYN)
    }

    /// The backstop with nothing retiring, which is every case but a migration.
    fn refusal(host_text: Option<&str>, request: ManagedRegionRequest<'_>) -> Option<String> {
        toml_introduction_refusal(host_text, request, &BTreeSet::new())
    }

    /// The generic parse backstop sees every region that is staying.
    #[test]
    fn a_duplicate_table_fails_the_parse_backstop() {
        let host = "# >>> anvil-managed: other\n[licenses]\nallow = [\"MIT\"]\n# <<< anvil-managed: other\n";
        let body = "[licenses]\nconfidence-threshold = 0.9\n";

        let verdict = refusal(Some(host), request("deny.toml", "r", body)).expect("two regions cannot both declare [licenses]");

        assert!(verdict.contains("unparsable as TOML"));
    }

    /// The same collision against a sibling this pass is *removing* is a
    /// migration, not a fault: the old region is blanked, so the splice is
    /// judged against the file as it will be left.
    #[test]
    fn a_retiring_sibling_does_not_refuse_the_region_replacing_it() {
        let host = "# >>> anvil-managed: old\n[licenses]\nallow = [\"MIT\"]\n# <<< anvil-managed: old\n";
        let body = "[licenses]\nconfidence-threshold = 0.9\n";
        let retiring = BTreeSet::from(["old".to_owned()]);

        assert_eq!(
            toml_introduction_refusal(Some(host), request("deny.toml", "r", body), &retiring),
            None,
            "the region being removed must not block the one replacing it"
        );
    }

    /// A dotted assignment declares its table just as a header does, so a
    /// sibling writing `[a]` beside a region writing `a.b = 1` is the same
    /// collision. Nothing enumerates table *headers* any more — the parser is
    /// asked, and it rejects declaring `a` twice.
    #[test]
    fn a_dotted_key_collides_with_a_siblings_header() {
        let host = "# >>> anvil-managed: other\nlints.rust.unsafe_code = \"deny\"\n# <<< anvil-managed: other\n";
        let body = "[lints]\nworkspace = true\n";

        let verdict = refusal(Some(host), request("Cargo.toml", "r", body)).expect("a dotted key declares the table too");

        assert!(verdict.contains("unparsable as TOML"));
    }

    /// Issue #148, end to end. A `deny.toml` whose `[advisories]` carries the
    /// repository's own accepted advisory used to receive a second
    /// `[advisories]` header — a file `cargo deny` cannot read, written to disk
    /// and recorded in the manifest before anything noticed, because the
    /// fixtures only ever asserted on fragments of its text.
    #[test]
    fn splicing_beside_a_hand_written_table_produces_parsable_toml() {
        let host = "[advisories]\n# waiting on upstream\nignore = [\"RUSTSEC-9999-0001\"]\n";
        let body = "[advisories]\nyanked = \"deny\"\nunmaintained = \"all\"\n";

        let item = plan_managed_region(
            &Manifest::default(),
            Some(host),
            request("deny.toml", "anvil-deny-advisories", body),
        )
        .unwrap();
        let spliced = item.spliced_host.as_deref().unwrap();

        let document = spliced
            .parse::<DocumentMut>()
            .unwrap_or_else(|error| panic!("spliced deny.toml must parse: {error}\n---\n{spliced}\n---"));
        assert_eq!(spliced.matches("[advisories]").count(), 1, "no duplicate header:\n{spliced}");
        // The kept entry has to stay an `[advisories]` setting: relocated under
        // the wrong header it is a different setting that cargo-deny ignores.
        assert_eq!(
            document["advisories"]["ignore"].as_array().unwrap().len(),
            1,
            "the accepted advisory is still an [advisories] entry:\n{spliced}"
        );
        assert_eq!(
            document["advisories"]["yanked"].as_str(),
            Some("deny"),
            "the managed keys are present"
        );
        assert!(
            spliced.contains("# waiting on upstream"),
            "the user's reasoning travels with it:\n{spliced}"
        );
    }

    /// A key both sides declare with different values has no safe output: TOML
    /// forbids repeating it, and choosing either value discards a decision
    /// somebody made. The run refuses the region and leaves the host alone.
    #[test]
    fn a_conflicting_key_is_refused_rather_than_written() {
        let host = "[advisories]\nyanked = \"warn\"\n";
        let body = "[advisories]\nyanked = \"deny\"\n";

        let reason =
            refusal(Some(host), request("deny.toml", "anvil-deny-advisories", body)).expect("a disagreement over `yanked` must be refused");

        assert!(reason.contains("yanked"), "the refusal names the key: {reason}");
    }

    /// A hand-written setting the body does not declare, in a table the body
    /// does not open last, has nowhere to go: re-emitted after the region it
    /// becomes a setting of the body's trailing table. The run refuses the
    /// region and names both tables, so the user can see what would have moved
    /// where.
    #[test]
    fn a_setting_that_would_change_table_is_refused_rather_than_written() {
        let host = "[Hunspell]\nlang = \"en_US\"\ntransform_regex = [\"^'\"]\n";
        let body = "[Hunspell]\nlang = \"en_US\"\n\n[Hunspell.quirks]\nallow_concatenation = true\n";

        let reason = refusal(Some(host), request("spellcheck.toml", "anvil-spellcheck", body))
            .expect("a setting that cannot keep its table must be refused");

        assert!(
            reason.contains("[Hunspell]") && reason.contains("[Hunspell.quirks]"),
            "the refusal names the table and where residue would land: {reason}"
        );
    }

    /// The refusal is a backstop, not a gate. An ordinary introduction — and an
    /// adoption that keeps residue — has to pass it, or onboarding stops for
    /// every repository that ever hand-wrote one of these tables.
    #[test]
    fn an_adoptable_host_is_not_refused() {
        let host = "[advisories]\nignore = [\"RUSTSEC-9999-0001\"]\n";
        let body = "[advisories]\nyanked = \"deny\"\n";

        assert_eq!(refusal(Some(host), request("deny.toml", "anvil-deny-advisories", body)), None);
        assert_eq!(refusal(None, request("deny.toml", "anvil-deny-advisories", body)), None);
        assert_eq!(refusal(Some("recipe:\n"), request("Justfile", "r", "body\n")), None);
    }

    /// A region already on disk beside a hand-written copy of its own table is
    /// the duplicate-header file adoption exists to repair — so an update
    /// repairs it rather than walking past it. The host below does not parse as
    /// written: two `[advisories]` headers. Adoption masks the region, sees the
    /// hand-written copy, and takes it over.
    #[test]
    fn an_existing_region_beside_a_hand_written_copy_is_repaired() {
        let host = "# >>> anvil-managed: r\n[advisories]\nyanked = \"warn\"\n# <<< anvil-managed: r\n\n[advisories]\nignore = []\n";
        assert!(host.parse::<DocumentMut>().is_err(), "the host starts out broken");

        let request = request("deny.toml", "r", "[advisories]\nyanked = \"deny\"\n");
        assert_eq!(refusal(Some(host), request), None, "repairable, not refused");

        let mut manifest = Manifest::default();
        manifest.set_region("deny.toml", "r", checksum_str("[advisories]\nyanked = \"warn\"\n"));
        let item = plan_managed_region(&manifest, Some(host), request).unwrap();
        let spliced = item.spliced_host.as_deref().expect("the region is written");

        let document = spliced
            .parse::<DocumentMut>()
            .unwrap_or_else(|error| panic!("the repaired host must parse: {error}\n---\n{spliced}\n---"));
        assert_eq!(spliced.matches("[advisories]").count(), 1, "one header survives:\n{spliced}");
        assert!(
            document["advisories"]["ignore"].as_array().is_some(),
            "and the hand-written entry is kept:\n{spliced}"
        );
    }

    /// A new independent table gets its own region, preserving the older one.
    #[test]
    fn a_new_table_region_adopts_the_hand_written_copy() {
        let host = "\
[licenses]
unused-allowed-license = \"allow\"

# >>> anvil-managed: r
[advisories]
yanked = \"deny\"
# <<< anvil-managed: r
";
        assert!(host.parse::<DocumentMut>().is_ok(), "the host is valid before the bump");

        let old_body = "[advisories]\nyanked = \"deny\"\n";
        let new_body = "[licenses]\nallow = [\"MIT\"]\n";
        let mut manifest = Manifest::default();
        manifest.set_region("deny.toml", "r", checksum_str(old_body));

        let request = request("deny.toml", "licenses", new_body);
        assert_eq!(refusal(Some(host), request), None, "adoption resolves it");

        let item = plan_managed_region(&manifest, Some(host), request).unwrap();
        assert_eq!(item.decision, Decision::Write);
        let spliced = item.spliced_host.as_deref().expect("the region is written");

        let document = spliced
            .parse::<DocumentMut>()
            .unwrap_or_else(|error| panic!("the updated host must parse: {error}\n---\n{spliced}\n---"));
        assert_eq!(spliced.matches("[licenses]").count(), 1, "no duplicate header:\n{spliced}");
        assert_eq!(
            document["licenses"]["unused-allowed-license"].as_str(),
            Some("allow"),
            "the user's own setting is never dropped, and still configures `[licenses]`:\n{spliced}"
        );
        assert!(document["licenses"]["allow"].as_array().is_some(), "alongside the managed keys");
        assert_eq!(
            find_region(spliced, "r", CommentSyntax::Hash).unwrap().unwrap().body_str(),
            old_body
        );
    }

    /// Adoption on the update path does not reach for anything it did not
    /// reach for before: with nothing hand-written outside a region, every
    /// table the body declares is already the region's own and invisible behind
    /// the mask, so an ordinary template bump is byte-identical to what it was.
    #[test]
    fn an_ordinary_update_is_untouched_by_adoption() {
        let host = "# >>> anvil-managed: r\n[advisories]\nyanked = \"warn\"\n# <<< anvil-managed: r\n";
        let new_body = "[advisories]\nyanked = \"deny\"\n";
        let mut manifest = Manifest::default();
        manifest.set_region("deny.toml", "r", checksum_str("[advisories]\nyanked = \"warn\"\n"));

        let item = plan_managed_region(&manifest, Some(host), request("deny.toml", "r", new_body)).unwrap();

        assert_eq!(item.decision, Decision::Write);
        assert_eq!(
            item.spliced_host.as_deref(),
            Some("# >>> anvil-managed: r\n[advisories]\nyanked = \"deny\"\n# <<< anvil-managed: r\n"),
            "the region is replaced in place, with nothing relocated"
        );
    }

    /// An update whose body disagrees with a hand-written value still has no
    /// safe output, so it refuses exactly as an introduction does — and the
    /// remedy the diagnostic names is one the user can actually carry out.
    #[test]
    fn an_update_that_conflicts_with_a_hand_written_value_is_refused() {
        let host =
            "[licenses]\nconfidence-threshold = 0.8\n\n# >>> anvil-managed: r\n[advisories]\nyanked = \"deny\"\n# <<< anvil-managed: r\n";
        let new_body = "[licenses]\nconfidence-threshold = 0.93\n";

        let reason = refusal(Some(host), request("deny.toml", "licenses", new_body))
            .expect("a disagreement over `confidence-threshold` must be refused");

        assert!(reason.contains("confidence-threshold"), "the refusal names the key: {reason}");
    }

    #[test]
    fn missing_host_writes_new_file() {
        let item = plan_managed_region(&Manifest::default(), None, request("Justfile", "r", "body line\n")).unwrap();
        assert_eq!(item.decision, Decision::Write);
        let spliced = item.spliced_host.as_deref().unwrap();
        assert!(spliced.contains("# >>> anvil-managed: r"));
        assert!(spliced.contains("body line"));
    }

    #[test]
    fn existing_host_without_region_appends_region() {
        let item = plan_managed_region(&Manifest::default(), Some("user content\n"), request("Justfile", "r", "body\n")).unwrap();
        assert_eq!(item.decision, Decision::Write);
        let spliced = item.spliced_host.as_deref().unwrap();
        assert!(spliced.starts_with("user content\n"));
        assert!(spliced.contains("# >>> anvil-managed: r"));
    }

    #[test]
    fn adoption_keeps_the_original_line_ending_when_it_removes_the_whole_host() {
        for newline in ["\n", "\r\n"] {
            for residue in ["", "ignore = []"] {
                let host = format!("[advisories]{newline}yanked = \"deny\"{newline}{residue}{newline}");
                let body = "[advisories]\nyanked = \"deny\"\n";
                let item = plan_managed_region(&Manifest::default(), Some(&host), request("deny.toml", "r", body)).unwrap();
                let spliced = item.spliced_host.as_deref().unwrap();
                let expected = format!(
                    "# >>> anvil-managed: r{newline}[advisories]{newline}yanked = \"deny\"{newline}# <<< anvil-managed: r{newline}"
                );
                let expected = if residue.is_empty() {
                    expected
                } else {
                    format!("{expected}{residue}{newline}")
                };
                assert_eq!(spliced, expected);
                let document = spliced.parse::<DocumentMut>().unwrap();
                assert_eq!(document["advisories"]["yanked"].as_str(), Some("deny"));
                if !residue.is_empty() {
                    assert!(document["advisories"]["ignore"].is_array());
                }

                let mut manifest = Manifest::default();
                manifest.set_region("deny.toml", "r", checksum_str(body));
                let repeated = plan_managed_region(&manifest, Some(spliced), request("deny.toml", "r", body)).unwrap();
                assert_eq!(repeated.decision, Decision::InSync);
            }
        }
    }

    #[test]
    fn crlf_region_updates_keep_normalized_checksum_decisions() {
        let old_body = "old\n";
        let host = "# user\r\n\r\n# >>> anvil-managed: r\r\nold\r\n# <<< anvil-managed: r\r\n";
        for last_body in [old_body, "old\r\n"] {
            let mut manifest = Manifest::default();
            manifest.set_region("Justfile", "r", checksum_str(last_body));
            let item = plan_managed_region(&manifest, Some(host), request("Justfile", "r", "new\n")).unwrap();
            assert_eq!(item.decision, Decision::Write);
            assert_eq!(
                item.spliced_host.as_deref().unwrap(),
                "# user\r\n\r\n# >>> anvil-managed: r\r\nnew\r\n# <<< anvil-managed: r\r\n"
            );
        }
    }

    #[test]
    fn adopting_an_unterminated_crlf_host_keeps_the_relocated_entry_crlf() {
        let host = "[advisories]\r\nignore = []";
        let item = plan_managed_region(
            &Manifest::default(),
            Some(host),
            request("deny.toml", "r", "[advisories]\nyanked = \"deny\"\n"),
        )
        .unwrap();
        assert_eq!(
            item.spliced_host.as_deref().unwrap(),
            "# >>> anvil-managed: r\r\n[advisories]\r\nyanked = \"deny\"\r\n# <<< anvil-managed: r\r\nignore = []\r\n"
        );
    }

    /// A member manifest that already declares `[lints] workspace = true` by
    /// hand must not gain a second `[lints]` table when the managed region is
    /// first introduced. TOML rejects a duplicate table outright, so appending
    /// blindly does not merely produce redundant text — it makes the manifest
    /// unparseable and takes the whole workspace down with it.
    #[test]
    fn an_unmanaged_lints_table_is_adopted_rather_than_duplicated() {
        let host = "[package]\nname = \"demo\"\n\n[lints]\nworkspace = true\n";
        let item = plan_managed_region(
            &Manifest::default(),
            Some(host),
            request("crates/demo/Cargo.toml", "anvil-lints", "[lints]\nworkspace = true\n"),
        )
        .unwrap();

        let spliced = item.spliced_host.as_deref().unwrap();
        assert_eq!(
            spliced.matches("\n[lints]").count() + usize::from(spliced.starts_with("[lints]")),
            1,
            "exactly one [lints] table survives:\n{spliced}"
        );
        assert!(
            spliced.contains("# >>> anvil-managed: anvil-lints"),
            "the surviving table is the managed one"
        );
        assert!(spliced.contains("[package]"), "unrelated content is preserved");
    }

    #[test]
    fn a_table_header_with_a_trailing_comment_is_adopted() {
        let host = "[package]\nname = \"demo\"\n\n[lints] # configured by hand\nworkspace = true\n";
        let item = plan_managed_region(
            &Manifest::default(),
            Some(host),
            request("crates/demo/Cargo.toml", "anvil-lints", "[lints]\nworkspace = true\n"),
        )
        .unwrap();

        let spliced = item.spliced_host.as_deref().unwrap();
        assert_eq!(
            spliced.matches("[lints]").count(),
            1,
            "exactly one [lints] table survives:\n{spliced}"
        );
        assert!(
            !spliced.contains("configured by hand"),
            "the adopted table's comment is removed:\n{spliced}"
        );
    }

    /// Adoption must drop the duplicated table and nothing else: a table that
    /// merely follows the adopted one is unrelated and must survive intact.
    #[test]
    fn adoption_stops_at_the_next_table_header() {
        let host = "[package]\nname = \"demo\"\n\n[lints]\nworkspace = true\n\n[dependencies]\nserde = \"1\"\n";
        let item = plan_managed_region(
            &Manifest::default(),
            Some(host),
            request("crates/demo/Cargo.toml", "anvil-lints", "[lints]\nworkspace = true\n"),
        )
        .unwrap();

        let spliced = item.spliced_host.as_deref().unwrap();
        assert!(spliced.contains("[dependencies]"), "the following table survives:\n{spliced}");
        assert!(spliced.contains("serde = \"1\""), "its keys survive too:\n{spliced}");
        assert_eq!(spliced.matches("\n[lints]").count(), 1, "still exactly one [lints]:\n{spliced}");
    }

    /// A non-TOML host is untouched by adoption — a bracketed line in a
    /// Justfile or YAML file is not a table header and must not be dropped.
    /// The fixture is deliberately one that adoption *would* claim in a TOML
    /// host: the bracketed block matches the body exactly, so only the
    /// host-type restriction keeps it, and weakening that restriction fails
    /// this test rather than passing it by accident.
    #[test]
    fn a_non_toml_host_is_not_subject_to_table_adoption() {
        let host = "[not-a-table]\nbody\n";
        let item = plan_managed_region(&Manifest::default(), Some(host), request("Justfile", "r", "[not-a-table]\nbody\n")).unwrap();

        let spliced = item.spliced_host.as_deref().unwrap();
        assert!(
            spliced.starts_with("[not-a-table]\nbody\n"),
            "host content is preserved verbatim:\n{spliced}"
        );
    }

    /// The limit on adoption, and the more important half of it: a hand-written
    /// entry the managed body does not declare is configuration, not a
    /// duplicate. It is never deleted — it is kept as residue and re-emitted
    /// inside the table the region opens, which is where it was written.
    #[test]
    fn a_hand_written_entry_is_never_dropped() {
        let host = "[advisories]\nignore = [\"RUSTSEC-9999-0001\"]\n";
        let item = plan_managed_region(
            &Manifest::default(),
            Some(host),
            request("deny.toml", "anvil-deny-advisories", "[advisories]\nyanked = \"deny\"\n"),
        )
        .unwrap();

        let spliced = item.spliced_host.as_deref().unwrap();
        assert!(spliced.contains("RUSTSEC-9999-0001"), "user-authored keys survive:\n{spliced}");
    }

    #[test]
    fn start_placement_moves_an_in_sync_body_at_the_end() {
        let body = "trip_wire_patterns = []\n";
        let host = "[git]\nremote_branch = \"origin/main\"\n\n# >>> anvil-managed: r\ntrip_wire_patterns = []\n# <<< anvil-managed: r\n";
        let mut manifest = Manifest::default();
        manifest.set_region("delta.toml", "r", checksum_str(body));

        let item = plan_managed_region(
            &manifest,
            Some(host),
            ManagedRegionRequest {
                placement: RegionPlacement::Start,
                ..request("delta.toml", "r", body)
            },
        )
        .unwrap();

        assert_eq!(item.decision, Decision::Write);
        assert!(item.spliced_host.as_deref().unwrap().starts_with("# >>> anvil-managed: r"));
    }

    #[test]
    fn start_placement_updates_an_untouched_legacy_region_at_the_start() {
        let old_body = "[delta]\nroot-files = [\"Cargo.toml\"]\n";
        let new_body = "trip_wire_patterns = [\"Cargo.toml\"]\n";
        let host = format!("# >>> anvil-managed: r\n{old_body}# <<< anvil-managed: r\n");
        let mut manifest = Manifest::default();
        manifest.set_region("delta.toml", "r", checksum_str(old_body));

        let item = plan_managed_region(
            &manifest,
            Some(&host),
            ManagedRegionRequest {
                placement: RegionPlacement::Start,
                ..request("delta.toml", "r", new_body)
            },
        )
        .unwrap();

        assert_eq!(item.decision, Decision::Write);
        assert!(item.spliced_host.as_deref().unwrap().contains("trip_wire_patterns"));
    }

    #[test]
    fn matching_region_is_in_sync() {
        let host = "before\n\
                    # >>> anvil-managed: r\n\
                    body\n\
                    # <<< anvil-managed: r\n\
                    after\n";
        let item = plan_managed_region(&Manifest::default(), Some(host), request("Justfile", "r", "body\n")).unwrap();
        assert_eq!(item.decision, Decision::InSync);
    }

    #[test]
    fn user_modified_is_refused_when_template_changed() {
        let host = "# >>> anvil-managed: r\nuser body\n# <<< anvil-managed: r\n";
        let mut manifest = Manifest::default();
        manifest.set_region("Justfile", "r", checksum_str("old body\n"));
        plan_managed_region(&manifest, Some(host), request("Justfile", "r", "new body\n")).unwrap_err();
    }

    #[test]
    fn user_modified_is_refused_when_template_unchanged() {
        let host = "# >>> anvil-managed: r\nuser body\n# <<< anvil-managed: r\n";
        let mut manifest = Manifest::default();
        manifest.set_region("Justfile", "r", checksum_str("body\n"));
        plan_managed_region(&manifest, Some(host), request("Justfile", "r", "body\n")).unwrap_err();
    }

    #[test]
    fn empty_region_is_regenerated_when_template_unchanged() {
        let host = "# >>> anvil-managed: r\n# <<< anvil-managed: r\n";
        let mut manifest = Manifest::default();
        manifest.set_region("Justfile", "r", checksum_str("body\n"));
        let item = plan_managed_region(&manifest, Some(host), request("Justfile", "r", "body\n")).unwrap();
        assert_eq!(item.decision, Decision::Write);
    }

    #[test]
    fn empty_region_is_regenerated_with_new_template() {
        let host = "# >>> anvil-managed: r\n# <<< anvil-managed: r\n";
        let mut manifest = Manifest::default();
        manifest.set_region("Justfile", "r", checksum_str("old\n"));
        let item = plan_managed_region(&manifest, Some(host), request("Justfile", "r", "new\n")).unwrap();
        assert_eq!(item.decision, Decision::Write);
    }

    /// The counterpart to the introduction case, and once the reverse of it:
    /// this asserted that an existing region left a hand-written `[lints]`
    /// exactly where it was, which — with the body declaring `[lints]` too —
    /// is a `Cargo.toml` carrying two `[lints]` headers, i.e. one TOML rejects
    /// and cargo cannot read. Adoption now runs here too, so the hand-written
    /// copy is taken over instead. Its lone entry is one the body declares
    /// identically, so it is covered and simply dropped.
    #[test]
    fn an_existing_region_adopts_a_hand_written_table_its_body_declares() {
        let host = "[lints]\nworkspace = true\n\n# >>> anvil-managed: r\nold = true\n# <<< anvil-managed: r\n";
        let mut manifest = Manifest::default();
        manifest.set_region("Cargo.toml", "r", checksum_str("old = true\n"));

        let item = plan_managed_region(&manifest, Some(host), request("Cargo.toml", "r", "[lints]\nworkspace = true\n")).unwrap();

        assert_eq!(item.decision, Decision::Write);
        let spliced = item.spliced_host.as_deref().unwrap();
        spliced
            .parse::<DocumentMut>()
            .unwrap_or_else(|error| panic!("the updated manifest must parse: {error}\n---\n{spliced}\n---"));
        assert_eq!(spliced.matches("[lints]").count(), 1, "no duplicate header:\n{spliced}");
        assert!(spliced.contains("workspace = true"), "the setting survives:\n{spliced}");
    }

    #[test]
    fn composes_onto_existing_region_in_host_text() {
        // A second region planned against host text that already carries a
        // first region must preserve the first and append the second —
        // this is the in-memory composition that lets several regions
        // share one host file (e.g. the sections of deny.toml).
        let host = "# >>> anvil-managed: a\nbody-a\n# <<< anvil-managed: a\n";
        let item = plan_managed_region(&Manifest::default(), Some(host), request("deny.toml", "b", "body-b\n")).unwrap();
        assert_eq!(item.decision, Decision::Write);
        let spliced = item.spliced_host.as_deref().unwrap();
        assert!(spliced.contains("anvil-managed: a"), "first region preserved");
        assert!(spliced.contains("body-a"), "first region body preserved");
        assert!(spliced.contains("anvil-managed: b"), "second region appended");
        assert!(spliced.contains("body-b"), "second region body appended");
    }
}
