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

use ohno::{AppError, app_err};
use toml_edit::DocumentMut;

use crate::checksum::checksum_str;
use crate::decision::{Decision, DecisionInputs, UpdateDecision, decide};
use crate::manifest::{Manifest, RegionKey};
use crate::plan::{PlanItem, Target};
use crate::region::{
    CommentSyntax, RegionPlacement, TomlAdoption, adopt_unmanaged_toml_tables, find_region, insert_after_region,
    mask_other_managed_regions, upsert_region_with_placement,
};

/// Inputs that identify and render one managed region.
#[derive(Clone, Copy)]
pub struct ManagedRegionRequest<'a> {
    /// Repo-root-relative forward-slash path of the host file.
    pub host_relpath: &'a str,
    /// Stable identifier written into the region sentinels.
    pub region_id: &'a str,
    /// Byte-exact content rendered between the sentinels.
    pub rendered_body: &'a str,
    /// Comment flavor used by the host file.
    pub syntax: CommentSyntax,
    /// Required position of the region within the host file.
    pub placement: RegionPlacement,
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
    let needs_reposition = placement == RegionPlacement::Start && disk_region.as_ref().is_some_and(|region| region.start_line.start != 0);

    let inputs = DecisionInputs {
        last_rendered,
        disk: disk_checksum.as_deref(),
        template: &template_checksum,
    };

    let target = Target::Region {
        host: host_relpath.to_owned(),
        id: region_id.to_owned(),
    };
    let decision = match decide(&inputs) {
        UpdateDecision::InSync if needs_reposition => UpdateDecision::Write,
        decision => decision,
    };
    let item = match decision {
        UpdateDecision::InSync => PlanItem::insync(target, template_checksum),
        UpdateDecision::LeaveAlone => PlanItem::noop(target, Decision::LeaveAlone),
        UpdateDecision::Write => {
            let spliced = splice(host_relpath, host_text, region_id, rendered_body, syntax, placement)?;
            PlanItem::write_region(host_relpath, region_id, rendered_body.to_owned(), spliced, template_checksum)
        }
        UpdateDecision::Propose => {
            let spliced = splice(host_relpath, host_text, region_id, rendered_body, syntax, placement)?;
            PlanItem::propose_region(host_relpath, region_id, rendered_body.to_owned(), spliced, template_checksum)
        }
    };

    Ok(item)
}

/// Why introducing `request`'s region into its TOML host would produce a file
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
/// Only an **introduction** is checked. Once the region exists,
/// `upsert_region_with_placement` replaces it where it stands and cannot
/// introduce a duplicate header — and a host the repository has since broken by
/// hand is not anvil's to refuse.
///
/// Returns `None` for a host that is not TOML, for a region that is already
/// present, and for a splice whose result parses.
#[must_use]
pub fn toml_introduction_refusal(host_text: Option<&str>, request: ManagedRegionRequest<'_>) -> Option<String> {
    let ManagedRegionRequest {
        host_relpath,
        region_id,
        rendered_body,
        syntax,
        placement,
    } = request;
    if !is_toml_host(host_relpath) {
        return None;
    }
    let base = host_text.unwrap_or("");
    // A malformed region is a separate diagnosis, raised by the planner.
    if !matches!(find_region(base, region_id, syntax), Ok(None)) {
        return None;
    }

    match splice(host_relpath, host_text, region_id, rendered_body, syntax, placement) {
        Err(error) => Some(error.to_string()),
        Ok(spliced) => mask_other_managed_regions(&spliced, syntax, region_id)
            .parse::<DocumentMut>()
            .err()
            .map(|error| format!("splicing the region would leave {host_relpath} unparsable as TOML: {error}")),
    }
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
) -> Result<String, AppError> {
    let base = host_text.unwrap_or("");

    // Introducing a region into a TOML host: adopt any hand-written copy of the
    // tables the body declares, rather than appending a duplicate that TOML
    // will refuse to parse. Only on introduction — once the region exists,
    // `upsert_region_with_placement` replaces it in place and there is nothing
    // to adopt.
    let adopted;
    let mut residue = String::new();
    let base = if is_toml_host(host_relpath) && find_region(base, region_id, syntax)?.is_none() {
        match adopt_unmanaged_toml_tables(base, rendered_body, syntax) {
            TomlAdoption::Unchanged => base,
            TomlAdoption::Adopted { text, residue: kept } => {
                residue = kept;
                adopted = text;
                adopted.as_str()
            }
            // Unreachable in the normal path: `run` refuses the host before it
            // ever plans a conflicting region (see `toml_adoption_refusal`).
            // Reported rather than written, because every output available here
            // either repeats a key TOML forbids or discards configuration.
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
        }
    } else {
        base
    };

    let spliced = upsert_region_with_placement(base, region_id, rendered_body, syntax, placement)?;
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

        let reason = toml_introduction_refusal(Some(host), request("deny.toml", "anvil-deny-advisories", body))
            .expect("a disagreement over `yanked` must be refused");

        assert!(reason.contains("yanked"), "the refusal names the key: {reason}");
    }

    /// The refusal is a backstop, not a gate. An ordinary introduction — and an
    /// adoption that keeps residue — has to pass it, or onboarding stops for
    /// every repository that ever hand-wrote one of these tables.
    #[test]
    fn an_adoptable_host_is_not_refused() {
        let host = "[advisories]\nignore = [\"RUSTSEC-9999-0001\"]\n";
        let body = "[advisories]\nyanked = \"deny\"\n";

        assert_eq!(
            toml_introduction_refusal(Some(host), request("deny.toml", "anvil-deny-advisories", body)),
            None
        );
        assert_eq!(
            toml_introduction_refusal(None, request("deny.toml", "anvil-deny-advisories", body)),
            None
        );
        assert_eq!(
            toml_introduction_refusal(Some("recipe:\n"), request("Justfile", "r", "body\n")),
            None
        );
    }

    /// Once the region exists it is replaced where it stands, so it cannot
    /// introduce a duplicate header — and a host the repository has since
    /// broken by hand is not anvil's to refuse. Checking an update too would
    /// turn every such file into a refusal of a region that is already there.
    #[test]
    fn an_existing_region_is_not_re_checked() {
        let host = "# >>> anvil-managed: r\n[advisories]\nyanked = \"deny\"\n# <<< anvil-managed: r\n\n[advisories]\nignore = []\n";

        assert_eq!(
            toml_introduction_refusal(Some(host), request("deny.toml", "r", "[advisories]\nyanked = \"deny\"\n")),
            None
        );
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
    fn user_modified_proposes_when_template_changed() {
        let host = "# >>> anvil-managed: r\nuser body\n# <<< anvil-managed: r\n";
        let mut manifest = Manifest::default();
        manifest.set_region("Justfile", "r", checksum_str("old body\n"));
        let item = plan_managed_region(&manifest, Some(host), request("Justfile", "r", "new body\n")).unwrap();
        assert_eq!(item.decision, Decision::Propose);
        assert!(item.spliced_host.is_some());
    }

    #[test]
    fn user_modified_template_unchanged_leaves_alone() {
        let host = "# >>> anvil-managed: r\nuser body\n# <<< anvil-managed: r\n";
        let mut manifest = Manifest::default();
        manifest.set_region("Justfile", "r", checksum_str("body\n"));
        let item = plan_managed_region(&manifest, Some(host), request("Justfile", "r", "body\n")).unwrap();
        assert_eq!(item.decision, Decision::LeaveAlone);
    }

    #[test]
    fn empty_region_opts_out_when_template_unchanged() {
        // Steady-state opt-out: user emptied the region, template hasn't moved.
        let host = "# >>> anvil-managed: r\n# <<< anvil-managed: r\n";
        let mut manifest = Manifest::default();
        manifest.set_region("Justfile", "r", checksum_str("body\n"));
        let item = plan_managed_region(&manifest, Some(host), request("Justfile", "r", "body\n")).unwrap();
        assert_eq!(item.decision, Decision::LeaveAlone);
    }

    #[test]
    fn empty_region_with_new_template_proposes() {
        let host = "# >>> anvil-managed: r\n# <<< anvil-managed: r\n";
        let mut manifest = Manifest::default();
        manifest.set_region("Justfile", "r", checksum_str("old\n"));
        let item = plan_managed_region(&manifest, Some(host), request("Justfile", "r", "new\n")).unwrap();
        // Opt-out remains in place but the user gets a proposed host file.
        assert_eq!(item.decision, Decision::Propose);
    }

    /// Adoption applies only when the region is first introduced. Once the
    /// region exists, the host outside the sentinels is untouched and the
    /// body is replaced in place, so a hand-written table stays exactly where
    /// it is whatever its relationship to the rendered body.
    #[test]
    fn an_existing_region_does_not_re_run_table_adoption() {
        let host = "[lints]\nworkspace = true\n\n# >>> anvil-managed: r\nold = true\n# <<< anvil-managed: r\n";
        let mut manifest = Manifest::default();
        manifest.set_region("Cargo.toml", "r", checksum_str("old = true\n"));

        let item = plan_managed_region(&manifest, Some(host), request("Cargo.toml", "r", "[lints]\nworkspace = true\n")).unwrap();

        assert_eq!(item.decision, Decision::Write);
        let spliced = item.spliced_host.as_deref().unwrap();
        assert!(
            spliced.starts_with("[lints]\nworkspace = true\n"),
            "the hand-written table is left alone:\n{spliced}"
        );
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
