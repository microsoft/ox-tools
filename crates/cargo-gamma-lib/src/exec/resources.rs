// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::str::FromStr;
use std::collections::BTreeSet;
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use camino::Utf8PathBuf;
use cargo_gamma_process::MemoryRequest;

use super::census;
use super::test_binary::TestBinary;
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

#[derive(Debug, Default)]
struct BinaryResources {
    binary: Vec<Arc<str>>,
    all: Vec<Arc<str>>,
    by_test: HashMap<Box<str>, Vec<Arc<str>>>,
}

/// Shared-resource admission for every test process in a campaign.
#[derive(Debug, Default)]
pub(super) struct Resources {
    capacities: HashMap<Arc<str>, usize>,
    binaries: HashMap<Utf8PathBuf, BinaryResources>,
    active: Mutex<HashMap<Arc<str>, usize>>,
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
        let mut seen = BTreeSet::new();
        let mut assignments = HashMap::default();

        for binary in binaries {
            // A custom `harness = false` target has no libtest registry and therefore cannot
            // contain the generated ignored marker tests. Do not launch it with libtest-only
            // arguments: a custom main may ignore them and run its real workload instead.
            if binary.libtest == Some(false) {
                continue;
            }
            let Some(names) = census::list_resource_markers(work, binary, request) else {
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
                let selected: BTreeSet<Box<str>> = census::list_selected_allow_empty(work, binary, request)
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
        while names
            .iter()
            .any(|name| active.get(name).copied().unwrap_or(0) >= self.capacities.get(name).copied().unwrap_or(1))
        {
            waiting();
            active = self.changed.wait(active).unwrap_or_else(PoisonError::into_inner);
        }
        for name in &names {
            let count = active.entry(Arc::clone(name)).or_default();
            *count = count.saturating_add(1);
        }

        ResourcePermit { resources: self, names }
    }

    pub(super) fn required(&self, binary: &TestBinary, only: Only<'_>) -> Vec<Arc<str>> {
        let Some(binary) = self.binaries.get(&binary.path) else {
            return Vec::new();
        };
        let mut names = match only {
            Only::All => binary.all.clone(),
            Only::One(test) => binary.by_test.get(test).cloned().unwrap_or_default(),
            Only::These(tests) => {
                let mut names: Vec<Arc<str>> = tests
                    .iter()
                    .filter_map(|test| binary.by_test.get(*test))
                    .flatten()
                    .cloned()
                    .collect();
                names.sort_unstable();
                names.dedup();
                names
            }
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
    fn drop(&mut self) {
        let mut active = self.resources.active.lock().unwrap_or_else(PoisonError::into_inner);
        for name in &self.names {
            if let Some(count) = active.get_mut(name) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    active.remove(name);
                }
            }
        }
        core::mem::drop(active);
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
        census::reset_resource_listing_calls();

        let resources =
            Resources::discover(&work, &[binary], &[], MemoryRequest::default()).expect("custom harness has no libtest markers");

        assert!(resources.capacities.is_empty());
        assert!(resources.binaries.is_empty());
        assert_eq!(census::resource_listing_calls(), 0);
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
            let permit = resources.acquire(&binary, Only::These(&["first", "second"]));
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
