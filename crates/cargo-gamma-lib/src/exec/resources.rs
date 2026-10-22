// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::str::FromStr;
use std::collections::BTreeSet;
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use camino::Utf8PathBuf;
use cargo_gamma_process::MemoryRequest;

use super::test_binary::TestBinary;
use super::test_listing;
use super::verdict::Only;
use super::workspace::Workspace;
use crate::error::error;
use crate::{HashMap, Result};

const MARKER_PREFIX: &str = "__cargo_gamma_resource_";
const TEST_SEPARATOR: &str = "_test_";
const BINARY_SUFFIX: &str = "_binary";

/// One command-line or configuration-file resource capacity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceLimit {
    /// The source-visible resource name.
    name: String,

    /// How many tests consuming it may run concurrently.
    max_concurrency: usize,
}

impl ResourceLimit {
    /// Creates a validated resource capacity.
    ///
    /// # Errors
    ///
    /// Returns an error when `name` is not a resource identifier or `max_concurrency` is zero.
    pub fn new(name: impl Into<String>, max_concurrency: usize) -> core::result::Result<Self, String> {
        let name = name.into();
        validate_name(&name)?;
        if max_concurrency == 0 {
            return Err("resource concurrency must be at least 1".to_owned());
        }

        Ok(Self { name, max_concurrency })
    }

    /// Returns the source-visible resource name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns how many consumers may run concurrently.
    #[must_use]
    pub const fn max_concurrency(&self) -> usize {
        self.max_concurrency
    }
}

impl FromStr for ResourceLimit {
    type Err = String;

    fn from_str(text: &str) -> core::result::Result<Self, Self::Err> {
        let Some((name, concurrency)) = text.split_once('=') else {
            return Err("expected `NAME=N`, for example `cargo-subprocess=2`".to_owned());
        };
        let max_concurrency = concurrency
            .parse::<usize>()
            .map_err(|_cause| format!("resource concurrency `{concurrency}` is not a positive integer"))?;
        Self::new(name, max_concurrency)
    }
}

pub(crate) fn validate_name(name: &str) -> core::result::Result<(), String> {
    let mut bytes = name.bytes();
    if !bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "resource name `{name}` must start with an ASCII letter and contain only ASCII letters, digits, `.`, `_`, or `-`"
        ));
    }

    Ok(())
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct BinaryResources {
    binary: Vec<Arc<str>>,
    all: Vec<Arc<str>>,
    by_test: HashMap<Box<str>, Vec<Arc<str>>>,
}

/// Shared-resource admission for every test process in a campaign.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct Resources {
    capacities: HashMap<Arc<str>, usize>,
    binaries: HashMap<Utf8PathBuf, BinaryResources>,
    #[serde(skip)]
    active: Mutex<HashMap<Arc<str>, usize>>,
    #[serde(skip)]
    changed: Condvar,
}

impl Resources {
    #[cfg(test)]
    pub(super) fn fake_binary(binary: &TestBinary) -> Self {
        let resource: Arc<str> = "test-resource".into();
        Self {
            capacities: core::iter::once((Arc::clone(&resource), 1)).collect(),
            binaries: core::iter::once((
                binary.path.clone(),
                BinaryResources {
                    binary: vec![Arc::clone(&resource)],
                    all: vec![resource],
                    by_test: HashMap::default(),
                },
            ))
            .collect(),
            active: Mutex::new(HashMap::default()),
            changed: Condvar::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn fake_tests(binary: &TestBinary, assignments: &[(&str, &str)]) -> Self {
        let by_test = assignments
            .iter()
            .map(|(test, resource)| (Box::<str>::from(*test), vec![Arc::<str>::from(*resource)]))
            .collect::<HashMap<_, _>>();
        let mut all = by_test.values().flatten().cloned().collect::<Vec<_>>();
        all.sort_unstable();
        all.dedup();
        Self {
            capacities: all.iter().cloned().map(|resource| (resource, 1)).collect(),
            binaries: core::iter::once((
                binary.path.clone(),
                BinaryResources {
                    binary: Vec::new(),
                    all,
                    by_test,
                },
            ))
            .collect(),
            active: Mutex::new(HashMap::default()),
            changed: Condvar::new(),
        }
    }

    pub(super) fn discover(work: &Workspace, binaries: &[TestBinary], limits: &[ResourceLimit], request: MemoryRequest) -> Result<Self> {
        Self::discover_with(
            binaries,
            limits,
            |binary| test_listing::list_resource_markers(work, binary, request),
            |binary| test_listing::list_selected_allow_empty(work, binary, request),
        )
    }

    fn discover_with(
        binaries: &[TestBinary],
        limits: &[ResourceLimit],
        mut markers: impl FnMut(&TestBinary) -> Option<Vec<Box<str>>>,
        mut selected_tests: impl FnMut(&TestBinary) -> Option<Vec<Box<str>>>,
    ) -> Result<Self> {
        let mut seen = BTreeSet::new();
        let mut assignments = HashMap::default();

        for binary in binaries {
            // A custom `harness = false` target has no libtest registry and therefore cannot
            // contain the generated ignored marker tests. Do not launch it with libtest-only
            // arguments: a custom main may ignore them and run its real workload instead.
            if binary.libtest == Some(false) {
                continue;
            }
            let Some(names) = markers(binary) else {
                return Err(error!(
                    "could not list tests in `{}` while discovering its `#[gamma::resource(...)]` declarations.\n\
                     Run that test binary with `--list --format terse` to diagnose why its harness cannot be enumerated.",
                    binary.path
                ));
            };
            let mut by_test: HashMap<Box<str>, Vec<Arc<str>>> = HashMap::default();
            let mut binary_resources = Vec::new();
            let mut test_markers = Vec::new();

            for name in names {
                let Some(marker) = marker(&name) else {
                    continue;
                };
                match marker {
                    Marker::Binary(resource) => {
                        let _new = seen.insert(resource.clone());
                        binary_resources.push(resource.into());
                    }
                    Marker::Test { test, resource } => {
                        test_markers.push((test, resource));
                    }
                }
            }

            if !test_markers.is_empty() {
                let selected: BTreeSet<Box<str>> = selected_tests(binary)
                    .ok_or_else(|| {
                        error!(
                            "could not list the selected tests in `{}` while applying its `#[gamma::resource(...)]` declarations.\n\
                             Run that test binary with cargo-gamma's test arguments plus `--list --format terse` to diagnose the selection.",
                            binary.path
                        )
                    })?
                    .into_iter()
                    .collect();
                for (test, resource) in test_markers {
                    if selected.contains(test.as_str()) {
                        let _new = seen.insert(resource.clone());
                        by_test.entry(test.into()).or_default().push(resource.into());
                    }
                }
            }

            if by_test.is_empty() && binary_resources.is_empty() {
                continue;
            }

            binary_resources.sort_unstable();
            binary_resources.dedup();
            for resources in by_test.values_mut() {
                resources.sort_unstable();
                resources.dedup();
            }
            let mut all: Vec<Arc<str>> = binary_resources.iter().chain(by_test.values().flatten()).cloned().collect();
            all.sort_unstable();
            all.dedup();
            let _previous = assignments.insert(
                binary.path.clone(),
                BinaryResources {
                    binary: binary_resources,
                    all,
                    by_test,
                },
            );
        }

        let capacities = capacities(&seen, limits);

        Ok(Self {
            capacities,
            binaries: assignments,
            active: Mutex::new(HashMap::default()),
            changed: Condvar::new(),
        })
    }

    pub(super) fn acquire<'resources>(&'resources self, binary: &TestBinary, only: Only<'_>) -> ResourcePermit<'resources> {
        self.acquire_observed(binary, only, || {})
    }

    fn acquire_observed<'resources>(
        &'resources self,
        binary: &TestBinary,
        only: Only<'_>,
        mut waiting: impl FnMut(),
    ) -> ResourcePermit<'resources> {
        let names = self.required(binary, only);

        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        // #[gamma::skip(cond.negate, iter.any_to_all, tag = "timeout", reason = "the capacity wait predicate must unblock when all requested resources become available")]
        while names
            .iter()
            // #[gamma::skip(relational.ge_to_le, literal.int_increment, tag = "timeout", reason = "the campaign capacity comparison prevented a waiter from acquiring released resources")]
            .any(|name| active.get(name).copied().unwrap_or(0) >= self.capacities.get(name).copied().unwrap_or(1))
        {
            waiting();
            // #[gamma::skip(stmt.delete_assign, tag = "timeout", reason = "waiting must release and reacquire the mutex so permit holders can return resources")]
            active = self.changed.wait(active).unwrap_or_else(PoisonError::into_inner);
        }
        for name in &names {
            let count = active.entry(Arc::clone(name)).or_default();
            // #[gamma::skip(literal.int_increment, tag = "timeout", reason = "counting each permit twice leaks active capacity after it is dropped")]
            *count = count.saturating_add(1);
        }

        ResourcePermit { resources: self, names }
    }

    // #[gamma::skip(fn_value.one_element, tag = "timeout", reason = "fabricated resource names in the campaign made resource-limited scheduling wait indefinitely")]
    pub(super) fn required(&self, binary: &TestBinary, only: Only<'_>) -> Vec<Arc<str>> {
        let Some(binary) = self.binaries.get(&binary.path) else {
            return Vec::new();
        };
        let mut names = match only {
            Only::All => binary.all.clone(),
            Only::One(test) => binary.by_test.get(test).cloned().unwrap_or_default(),
        };
        if !matches!(only, Only::All) {
            names.extend(binary.binary.iter().cloned());
            names.sort_unstable();
            names.dedup();
        }
        names
    }

    pub(super) fn capacities(&self) -> HashMap<Arc<str>, usize> {
        self.capacities.clone()
    }
}

fn capacities(seen: &BTreeSet<String>, limits: &[ResourceLimit]) -> HashMap<Arc<str>, usize> {
    let configured: HashMap<&str, usize> = limits.iter().map(|limit| (limit.name(), limit.max_concurrency())).collect();

    seen.iter()
        .map(|name| {
            let capacity = configured.get(name.as_str()).copied().unwrap_or(1);
            (Arc::<str>::from(name.as_str()), capacity)
        })
        .collect()
}

pub(super) struct ResourcePermit<'resources> {
    resources: &'resources Resources,
    names: Vec<Arc<str>>,
}

impl Drop for ResourcePermit<'_> {
    // #[gamma::skip(fn_value.unit, tag = "timeout", reason = "not returning a permit leaves subsequent resource waiters blocked")]
    fn drop(&mut self) {
        let mut active = self.resources.active.lock().unwrap_or_else(PoisonError::into_inner);
        for name in &self.names {
            if let Some(count) = active.get_mut(name) {
                // #[gamma::skip(stmt.delete_assign, literal.int_decrement, tag = "timeout", reason = "each dropped permit must decrement its active resource count")]
                *count = count.saturating_sub(1);
                if *count == 0 {
                    active.remove(name);
                }
            }
        }
        core::mem::drop(active);
        // #[gamma::skip(stmt.delete_call, tag = "timeout", reason = "resource waiters must be notified after capacity is returned")]
        self.resources.changed.notify_all();
    }
}

pub(super) fn is_marker(name: &str) -> bool {
    marker(name).is_some()
}

#[derive(Debug, PartialEq, Eq)]
enum Marker {
    Binary(String),
    Test { test: String, resource: String },
}

fn marker(name: &str) -> Option<Marker> {
    let (module, local) = name.rsplit_once("::").unwrap_or(("", name));
    let encoded = local.strip_prefix(MARKER_PREFIX)?;
    if let Some(resource) = encoded.strip_suffix(BINARY_SUFFIX) {
        return Some(Marker::Binary(decode(resource)?));
    }
    let (resource, test) = encoded.split_once(TEST_SEPARATOR)?;
    let resource = decode(resource)?;
    let test = decode(test)?;
    let qualified = if module.is_empty() { test } else { format!("{module}::{test}") };

    Some(Marker::Test { test: qualified, resource })
}

fn decode(encoded: &str) -> Option<String> {
    if !encoded.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(encoded.len() / 2);
    for pair in encoded.as_bytes().chunks_exact(2) {
        let high = digit(pair[0])?;
        let low = digit(pair[1])?;
        bytes.push((high << 4) | low);
    }
    String::from_utf8(bytes).ok()
}

const fn digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::testing::test_binary;

    #[test]
    fn an_unconfigured_required_resource_has_one_slot_and_returns_it_without_waiting() {
        let binary = test_binary("default-capacity");
        let mut resources = Resources::fake_tests(&binary, &[("selected", "io")]);
        resources.capacities.clear();
        let permit = resources.acquire_observed(&binary, Only::One("selected"), || {
            panic!("an idle default-capacity resource must not wait")
        });
        let active = resources.active.lock().unwrap().get("io").copied();
        assert_eq!(active, Some(1));
        drop(permit);
        assert!(resources.active.lock().unwrap().is_empty());
        let permit = resources.acquire_observed(&binary, Only::All, || {
            panic!("dropping the first permit must return the default slot")
        });
        let active = resources.active.lock().unwrap().get("io").copied();
        assert_eq!(active, Some(1));
        drop(permit);
        assert!(resources.active.lock().unwrap().is_empty());
    }

    #[test]
    fn resource_discovery_forwards_the_prebaseline_memory_request_to_marker_enumeration() {
        let root = camino::Utf8Path::new(if cfg!(windows) { "C:/resource-policy" } else { "/resource-policy" });
        let work = Workspace::adopt(root.to_owned(), root.join("target"));
        let binary = test_binary("resource-listing");
        let resources = test_listing::with_listing_runner(
            |command, budget, request| {
                assert_eq!(command.get_program(), "resource-listing");
                assert!(request.meter);
                assert_eq!(request.limit, Some(37));
                assert!(budget > Duration::ZERO);
                assert_eq!(
                    command.get_args().collect::<Vec<_>>(),
                    ["--list", "--format=terse", "--ignored"].map(std::ffi::OsStr::new),
                );
                Some(vec!["__cargo_gamma_resource_696f_binary".into()])
            },
            || {
                Resources::discover(
                    &work,
                    core::slice::from_ref(&binary),
                    &[],
                    MemoryRequest {
                        meter: true,
                        limit: Some(37),
                    },
                )
            },
        )
        .unwrap();
        assert_eq!(resources.capacities().get("io"), Some(&1));
        assert_eq!(resources.binaries.len(), 1);
        assert_eq!(
            resources
                .required(&binary, Only::All)
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<&str>>(),
            ["io"]
        );
    }

    #[test]
    fn resource_hex_decode_rejects_a_bad_low_nibble_without_panicking() {
        for invalid in ["0g", "fg", "ag", "0G", "0é", "ff", "c328"] {
            assert_eq!(decode(invalid), None, "{invalid}");
        }
        assert_eq!(decode("616263"), Some("abc".into()));
    }

    #[test]
    fn all_selection_and_partial_selection_have_identical_unique_resource_unions() {
        let binary = test_binary("resource-union");
        let resources = Resources::discover_with(
            core::slice::from_ref(&binary),
            &[],
            |_| {
                Some(vec![
                    "__cargo_gamma_resource_6462_binary".into(),
                    "__cargo_gamma_resource_6462_test_6f6e65".into(),
                    "__cargo_gamma_resource_696f_test_6f6e65".into(),
                ])
            },
            |_| Some(vec!["one".into()]),
        )
        .unwrap();
        let expected = vec![Arc::<str>::from("db"), Arc::<str>::from("io")];
        assert_eq!(resources.required(&binary, Only::All), expected);
        assert_eq!(resources.required(&binary, Only::One("one")), expected);
        assert_eq!(resources.required(&binary, Only::One("unknown")), [Arc::<str>::from("db")]);
        let only_test_resources = Resources::discover_with(
            core::slice::from_ref(&binary),
            &[],
            |_| Some(vec!["__cargo_gamma_resource_696f_test_6f6e65".into()]),
            |_| Some(vec!["one".into()]),
        )
        .unwrap();
        assert_eq!(only_test_resources.required(&binary, Only::All), [Arc::<str>::from("io")]);
        assert!(only_test_resources.required(&binary, Only::One("unknown")).is_empty());
    }

    #[test]
    fn discovery_matrix_authenticates_markers_filters_tests_and_retains_exact_sorted_resources() {
        use core::fmt::Write as _;
        let encode = |text: &str| {
            text.bytes().fold(String::new(), |mut output, byte| {
                write!(output, "{byte:02x}").expect("writing formatted bytes to a String cannot fail");
                output
            })
        };
        let marker_binary = |resource: &str| format!("{MARKER_PREFIX}{}{BINARY_SUFFIX}", encode(resource)).into_boxed_str();
        let marker_test = |resource: &str, test: &str| {
            format!("suite::{MARKER_PREFIX}{}{TEST_SEPARATOR}{}", encode(resource), encode(test)).into_boxed_str()
        };
        let mut custom = test_binary("custom");
        custom.libtest = Some(false);
        let empty = test_binary("empty");
        let binary_only = test_binary("binary-only");
        let mixed = test_binary("mixed");
        let mut marker_calls = Vec::new();
        let mut selected_calls = Vec::new();
        let resources = Resources::discover_with(
            &[custom, empty.clone(), binary_only.clone(), mixed.clone()],
            &[ResourceLimit::new("alpha", 3).expect("valid alpha capacity")],
            |binary| {
                marker_calls.push(binary.path.clone());
                Some(match binary.path.as_str() {
                    "empty" => vec!["ordinary".into()],
                    "binary-only" => vec![marker_binary("zeta"), marker_binary("alpha"), marker_binary("zeta")],
                    "mixed" => vec![
                        "ordinary".into(),
                        marker_test("zeta", "selected"),
                        marker_test("alpha", "selected"),
                        marker_test("zeta", "selected"),
                        marker_test("excluded", "ignored"),
                        marker_binary("gamma"),
                    ],
                    _ => panic!("custom harness must not be enumerated"),
                })
            },
            |binary| {
                selected_calls.push(binary.path.clone());
                Some(vec!["suite::selected".into()])
            },
        )
        .expect("well-formed marker discovery");
        assert_eq!(marker_calls, ["empty", "binary-only", "mixed"].map(Utf8PathBuf::from));
        assert_eq!(selected_calls, ["mixed"].map(Utf8PathBuf::from));
        assert!(!resources.binaries.contains_key(&empty.path));
        assert_eq!(resources.binaries.len(), 2);
        assert_eq!(
            resources.capacities(),
            [
                (Arc::<str>::from("alpha"), 3),
                (Arc::<str>::from("gamma"), 1),
                (Arc::<str>::from("zeta"), 1)
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            resources.binaries[&binary_only.path]
                .binary
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<&str>>(),
            ["alpha", "zeta"]
        );
        assert_eq!(
            resources.binaries[&mixed.path].by_test["suite::selected"]
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<&str>>(),
            ["alpha", "zeta"]
        );
        assert!(!resources.binaries[&mixed.path].by_test.contains_key("suite::ignored"));
        for (only, expected) in [
            (Only::All, vec!["alpha", "gamma", "zeta"]),
            (Only::One("suite::selected"), vec!["alpha", "gamma", "zeta"]),
            (Only::One("suite::ignored"), vec!["gamma"]),
            (Only::One("missing"), vec!["gamma"]),
        ] {
            assert_eq!(
                resources.required(&mixed, only).iter().map(AsRef::as_ref).collect::<Vec<&str>>(),
                expected
            );
        }
    }

    #[test]
    fn discovery_error_matrix_propagates_marker_and_selected_listing_failures_without_panics() {
        let binary = test_binary("broken-harness");
        let failed = Resources::discover_with(
            core::slice::from_ref(&binary),
            &[],
            |_binary| None,
            |_binary| panic!("marker listing already failed"),
        )
        .expect_err("failed marker enumeration");
        assert_eq!(
            failed.to_string(),
            "could not list tests in `broken-harness` while discovering its `#[gamma::resource(...)]` declarations.\nRun that test binary with `--list --format terse` to diagnose why its harness cannot be enumerated."
        );
        let failed = Resources::discover_with(
            core::slice::from_ref(&binary),
            &[],
            |_binary| Some(vec!["__cargo_gamma_resource_61_test_62".into()]),
            |_binary| None,
        )
        .expect_err("failed selected enumeration");
        assert_eq!(
            failed.to_string(),
            "could not list the selected tests in `broken-harness` while applying its `#[gamma::resource(...)]` declarations.\nRun that test binary with cargo-gamma's test arguments plus `--list --format terse` to diagnose the selection."
        );
        let empty = Resources::discover_with(
            &[binary],
            &[],
            |_binary| Some(vec!["__cargo_gamma_resource_61_test_62".into()]),
            |_binary| Some(Vec::new()),
        )
        .expect("empty selected population is authoritative");
        assert!(empty.binaries.is_empty());
        assert!(empty.capacities.is_empty());
    }

    #[test]
    fn permit_count_matrix_releases_exactly_one_and_removes_only_exhausted_entries() {
        let binary = test_binary("permit-count");
        let mut resources = Resources::fake_binary(&binary);
        *resources.capacities.get_mut("test-resource").expect("fake capacity exists") = 3;
        let first = resources.acquire(&binary, Only::All);
        let second = resources.acquire(&binary, Only::All);
        assert_eq!(resources.active.lock().expect("healthy lock").get("test-resource"), Some(&2));
        drop(first);
        assert_eq!(resources.active.lock().expect("healthy lock").get("test-resource"), Some(&1));
        drop(second);
        assert!(resources.active.lock().expect("healthy lock").is_empty());
    }

    #[test]
    fn resource_errors_and_marker_decoding_have_exact_validation_outcomes() {
        for (input, error) in [
            ("cargo", "expected `NAME=N`, for example `cargo-subprocess=2`"),
            ("cargo=0", "resource concurrency must be at least 1"),
            ("cargo=bad", "resource concurrency `bad` is not a positive integer"),
        ] {
            assert_eq!(input.parse::<ResourceLimit>().expect_err("invalid resource capacity"), error);
        }
        assert!(is_marker("__cargo_gamma_resource_61_binary"));
        for marker_name in [
            "__cargo_gamma_resource_ff_test_61",
            "__cargo_gamma_resource_61_test_ff",
            "__cargo_gamma_resource_gg_test_61",
            "__cargo_gamma_resource_61_test_gg",
        ] {
            assert_eq!(marker(marker_name), None);
        }
        assert_eq!(decode(""), Some(String::new()));
        assert_eq!(decode("616263"), Some("abc".to_owned()));
    }

    #[test]
    fn resource_limit_requires_a_positive_capacity() {
        assert_eq!(
            "cargo-subprocess=2".parse::<ResourceLimit>(),
            Ok(ResourceLimit {
                name: "cargo-subprocess".to_owned(),
                max_concurrency: 2
            })
        );
        "cargo-subprocess=0"
            .parse::<ResourceLimit>()
            .expect_err("zero is not a valid capacity");
        "cargo subprocess=2"
            .parse::<ResourceLimit>()
            .expect_err("spaces are not valid in resource names");
        "cargo-subprocess".parse::<ResourceLimit>().expect_err("a capacity is required");
        ResourceLimit::new("", 1).expect_err("an empty name is invalid");
        ResourceLimit::new("two words", 1).expect_err("spaces are invalid");
        ResourceLimit::new("cargo", 0).expect_err("zero is invalid");
        let limit = ResourceLimit::new("cargo", 2).expect("valid resource");
        assert_eq!(limit.name(), "cargo");
        assert_eq!(limit.max_concurrency(), 2);
    }

    #[test]
    fn marker_recovers_the_qualified_test_and_resource() {
        assert_eq!(
            marker("suite::nested::__cargo_gamma_resource_636172676f2d73756270726f63657373_test_7265736f6c766573"),
            Some(Marker::Test {
                test: "suite::nested::resolves".to_owned(),
                resource: "cargo-subprocess".to_owned()
            })
        );
        assert_eq!(
            marker("__cargo_gamma_resource_636172676f_binary"),
            Some(Marker::Binary("cargo".to_owned()))
        );
        assert_eq!(marker("ordinary_test"), None);
        assert_eq!(marker("__cargo_gamma_resource_f_binary"), None);
        assert_eq!(marker("__cargo_gamma_resource_gg_binary"), None);
        assert_eq!(marker("__cargo_gamma_resource_ff_binary"), None);
        assert_eq!(
            marker("__cargo_gamma_resource_636172676f_test_"),
            Some(Marker::Test {
                test: String::new(),
                resource: "cargo".to_owned(),
            })
        );
        assert!(!is_marker("__cargo_gamma_resource_cleanup"));
        assert!(!is_marker("suite::__cargo_gamma_resource_cleanup"));
    }

    #[test]
    fn capacities_default_to_one_and_ignore_inactive_configuration() {
        let seen = BTreeSet::from(["cargo".to_owned(), "powershell".to_owned()]);
        let resolved = capacities(
            &seen,
            &[
                ResourceLimit {
                    name: "cargo".to_owned(),
                    max_concurrency: 2,
                },
                ResourceLimit {
                    name: "network".to_owned(),
                    max_concurrency: 3,
                },
            ],
        );

        assert_eq!(resolved.get("cargo"), Some(&2));
        assert_eq!(resolved.get("powershell"), Some(&1));
        assert_eq!(resolved.get("network"), None);
    }

    #[test]
    fn proc_macro_markers_are_understood_by_resource_discovery() {
        let expanded = cargo_gamma_attrs_impl::resource(
            "\"cargo-subprocess\"".parse().expect("resource argument"),
            "#[test] fn resolves_metadata() {}".parse().expect("annotated test"),
        )
        .to_string();
        let marker_name = expanded
            .split_ascii_whitespace()
            .find(|token| token.starts_with(MARKER_PREFIX))
            .expect("the expansion carries a resource marker");

        assert_eq!(
            marker(marker_name),
            Some(Marker::Test {
                test: "resolves_metadata".to_owned(),
                resource: "cargo-subprocess".to_owned(),
            })
        );
    }

    #[test]
    fn an_uncounted_custom_harness_needs_no_libtest_marker_listing() {
        let (_directory, work) = crate::testing::helper_workspace("resource-custom-harness", &[]);
        let mut binary = test_binary("missing-custom-harness");
        binary.libtest = Some(false);
        test_listing::reset_resource_listing_calls();

        let resources =
            Resources::discover(&work, &[binary], &[], MemoryRequest::default()).expect("custom harness has no libtest markers");

        assert!(resources.capacities.is_empty());
        assert!(resources.binaries.is_empty());
        assert_eq!(test_listing::resource_listing_calls(), 0);
    }

    #[test]
    fn a_default_capacity_serializes_matching_binary_runs_only() {
        let binary = test_binary("suite");
        let resource: Arc<str> = "cargo".into();
        let resources = Resources {
            capacities: core::iter::once((Arc::clone(&resource), 1)).collect(),
            binaries: core::iter::once((
                binary.path.clone(),
                BinaryResources {
                    binary: vec![Arc::clone(&resource)],
                    all: vec![resource],
                    by_test: HashMap::default(),
                },
            ))
            .collect(),
            active: Mutex::new(HashMap::default()),
            changed: Condvar::new(),
        };
        let first = resources.acquire(&binary, Only::All);
        let (waiting_sender, waiting_receiver) = mpsc::channel();
        let (finished_sender, finished_receiver) = mpsc::channel();

        thread::scope(|scope| {
            let blocked = scope.spawn(|| {
                let _permit = resources.acquire_observed(&binary, Only::All, || {
                    waiting_sender.send(()).expect("the admission observer remains live");
                });
                finished_sender.send(()).expect("the completion receiver remains live");
            });

            waiting_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("the second launch must reach blocked admission");
            assert!(finished_receiver.try_recv().is_err(), "blocked admission cannot complete");
            drop(first);
            finished_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("releasing the permit must wake the waiting launch");
            blocked.join().expect("the waiting launch exits normally");
        });
    }

    #[test]
    fn a_configured_capacity_admits_exactly_that_many_runs() {
        let binary = test_binary("suite");
        let resource: Arc<str> = "cargo".into();
        let resources = Resources {
            capacities: core::iter::once((Arc::clone(&resource), 2)).collect(),
            binaries: core::iter::once((
                binary.path.clone(),
                BinaryResources {
                    binary: vec![Arc::clone(&resource)],
                    all: vec![resource],
                    by_test: HashMap::default(),
                },
            ))
            .collect(),
            active: Mutex::new(HashMap::default()),
            changed: Condvar::new(),
        };
        let first = resources.acquire(&binary, Only::All);
        let _second = resources.acquire(&binary, Only::All);
        let (waiting_sender, waiting_receiver) = mpsc::channel();
        let (finished_sender, finished_receiver) = mpsc::channel();

        thread::scope(|scope| {
            let blocked = scope.spawn(|| {
                let _permit = resources.acquire_observed(&binary, Only::All, || {
                    waiting_sender.send(()).expect("the admission observer remains live");
                });
                finished_sender.send(()).expect("the completion receiver remains live");
            });

            waiting_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("the third launch must reach blocked admission");
            assert!(finished_receiver.try_recv().is_err(), "blocked admission cannot complete");
            drop(first);
            finished_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("releasing one permit must admit exactly one waiting launch");
            blocked.join().expect("the waiting launch exits normally");
        });
    }

    #[test]
    fn a_test_resource_does_not_constrain_an_unannotated_test() {
        let binary = test_binary("suite");
        let resource: Arc<str> = "cargo".into();
        let resources = Resources {
            capacities: core::iter::once((Arc::clone(&resource), 1)).collect(),
            binaries: core::iter::once((
                binary.path.clone(),
                BinaryResources {
                    binary: Vec::new(),
                    all: vec![Arc::clone(&resource)],
                    by_test: core::iter::once(("slow".into(), vec![resource])).collect(),
                },
            ))
            .collect(),
            active: Mutex::new(HashMap::default()),
            changed: Condvar::new(),
        };
        let _slow = resources.acquire(&binary, Only::One("slow"));

        let _fast = resources.acquire(&binary, Only::One("fast"));
    }

    #[test]
    fn test_and_binary_resources_are_combined_and_deduplicated() {
        let binary = test_binary("suite");
        let cargo: Arc<str> = "cargo".into();
        let network: Arc<str> = "network".into();
        let resources = Resources {
            capacities: [(Arc::clone(&cargo), 1), (Arc::clone(&network), 1)].into_iter().collect(),
            binaries: core::iter::once((
                binary.path.clone(),
                BinaryResources {
                    binary: vec![Arc::clone(&cargo)],
                    all: vec![Arc::clone(&cargo), Arc::clone(&network)],
                    by_test: [
                        ("first".into(), vec![Arc::clone(&cargo), Arc::clone(&network)]),
                        ("second".into(), vec![Arc::clone(&network)]),
                    ]
                    .into_iter()
                    .collect(),
                },
            ))
            .collect(),
            active: Mutex::new(HashMap::default()),
            changed: Condvar::new(),
        };

        {
            let permit = resources.acquire(&binary, Only::One("first"));
            assert_eq!(permit.names, vec![Arc::clone(&cargo), Arc::clone(&network)]);
        }
        {
            let permit = resources.acquire(&binary, Only::One("second"));
            assert_eq!(permit.names, vec![Arc::clone(&cargo), Arc::clone(&network)]);
        }
        let unknown = test_binary("unknown");
        assert!(resources.acquire(&unknown, Only::All).names.is_empty());
    }

    #[test]
    fn releasing_an_already_absent_resource_is_harmless() {
        let resources = Resources::default();
        drop(ResourcePermit {
            resources: &resources,
            names: vec![Arc::from("missing")],
        });
        assert!(resources.active.lock().expect("the resource lock is healthy").is_empty());
    }
}
