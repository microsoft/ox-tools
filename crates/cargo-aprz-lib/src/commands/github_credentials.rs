// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Best-effort GitHub credential discovery.

use std::convert::Infallible;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;
use std::{fmt, io};

use tokio::process::Command;
use url::Url;

use crate::facts::Endpoints;

const GITHUB_TOKEN_ENV: &str = "GITHUB_TOKEN";
const GH_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const LOG_TARGET: &str = "credentials";

/// A GitHub token whose debug representation never exposes its value.
#[derive(Clone, PartialEq, Eq)]
pub struct GitHubToken(String);

impl GitHubToken {
    /// Expose the token only where it is attached to the GitHub HTTP client.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for GitHubToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitHubToken([REDACTED])")
    }
}

impl FromStr for GitHubToken {
    type Err = Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(value.to_owned()))
    }
}

struct GhCommandOutput {
    success: bool,
    stdout: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GhStdio {
    Null,
    Capture,
}

impl GhStdio {
    fn into_stdio(self) -> Stdio {
        match self {
            Self::Null => Stdio::null(),
            Self::Capture => Stdio::piped(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct GhCommandRequest {
    executable: PathBuf,
    args: Vec<OsString>,
    stdin: GhStdio,
    stdout: GhStdio,
    stderr: GhStdio,
    timeout: Duration,
}

/// Resolve the GitHub credential used by the hosting provider.
pub(super) async fn discover(explicit: Option<&GitHubToken>, github_token_from_gh: bool, endpoints: &Endpoints) -> Option<GitHubToken> {
    discover_with(
        explicit,
        github_token_from_gh,
        endpoints,
        || std::env::var_os(GITHUB_TOKEN_ENV),
        query_gh,
    )
    .await
}

async fn discover_with<OutputFuture>(
    explicit: Option<&GitHubToken>,
    github_token_from_gh: bool,
    endpoints: &Endpoints,
    read_environment: impl FnOnce() -> Option<OsString>,
    query_gh: impl FnOnce(String) -> OutputFuture,
) -> Option<GitHubToken>
where
    OutputFuture: Future<Output = io::Result<GhCommandOutput>>,
{
    if let Some(token) = explicit {
        log::trace!(target: LOG_TARGET, "GitHub credential source: --github-token");
        return Some(token.clone());
    }

    if let Some(token) = read_environment() {
        let Ok(token) = token.into_string() else {
            log::trace!(
                target: LOG_TARGET,
                "GitHub credential source {GITHUB_TOKEN_ENV} is not valid UTF-8; using anonymous access"
            );
            return None;
        };
        let token = token.trim();
        if !token.is_empty() {
            log::trace!(target: LOG_TARGET, "GitHub credential source: {GITHUB_TOKEN_ENV}");
            return Some(GitHubToken(token.to_owned()));
        }
        log::trace!(
            target: LOG_TARGET,
            "GitHub credential source {GITHUB_TOKEN_ENV} is blank; continuing credential discovery"
        );
    }

    if !github_token_from_gh {
        log::trace!(target: LOG_TARGET, "GitHub CLI credential discovery was not requested; using anonymous access");
        return None;
    }

    let Some(hostname) = github_hostname(endpoints) else {
        log::trace!(
            target: LOG_TARGET,
            "Effective GitHub service URL has no usable hostname; using anonymous access"
        );
        return None;
    };

    let output = match query_gh(hostname.clone()).await {
        Ok(output) => output,
        Err(error) => {
            log::trace!(
                target: LOG_TARGET,
                "GitHub credential source gh could not be executed for host '{hostname}' ({}); using anonymous access",
                error.kind()
            );
            return None;
        }
    };

    if !output.success {
        log::trace!(
            target: LOG_TARGET,
            "GitHub credential source gh did not return a token for host '{hostname}'; using anonymous access"
        );
        return None;
    }

    let Ok(stdout) = String::from_utf8(output.stdout) else {
        log::trace!(
            target: LOG_TARGET,
            "GitHub credential source gh returned non-UTF-8 output for host '{hostname}'; using anonymous access"
        );
        return None;
    };

    let token = stdout.trim();
    if token.is_empty() {
        log::trace!(
            target: LOG_TARGET,
            "GitHub credential source gh returned a blank token for host '{hostname}'; using anonymous access"
        );
        return None;
    }

    log::trace!(target: LOG_TARGET, "GitHub credential source: gh for host '{hostname}'");
    Some(GitHubToken(token.to_owned()))
}

fn github_hostname(endpoints: &Endpoints) -> Option<String> {
    let url = Url::parse(endpoints.host_url("github.com")?).ok()?;
    let hostname = url.host_str()?;

    Some(if hostname.eq_ignore_ascii_case("api.github.com") {
        "github.com".to_owned()
    } else {
        hostname.to_owned()
    })
}

async fn query_gh(hostname: String) -> io::Result<GhCommandOutput> {
    query_gh_with(hostname, resolve_gh_executable, run_gh_command).await
}

async fn query_gh_with<ResolveFuture, RunFuture>(
    hostname: String,
    resolve_executable: impl FnOnce() -> ResolveFuture,
    run_command: impl FnOnce(GhCommandRequest) -> RunFuture,
) -> io::Result<GhCommandOutput>
where
    ResolveFuture: Future<Output = io::Result<PathBuf>>,
    RunFuture: Future<Output = io::Result<GhCommandOutput>>,
{
    let executable = resolve_executable().await?;
    if !executable.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "resolved gh executable path was not absolute",
        ));
    }

    run_command(GhCommandRequest {
        executable,
        args: ["auth", "token", "--hostname"]
            .into_iter()
            .map(OsString::from)
            .chain(std::iter::once(OsString::from(hostname)))
            .collect(),
        stdin: GhStdio::Null,
        stdout: GhStdio::Capture,
        stderr: GhStdio::Null,
        timeout: GH_COMMAND_TIMEOUT,
    })
    .await
}

async fn run_gh_command(request: GhCommandRequest) -> io::Result<GhCommandOutput> {
    let GhCommandRequest {
        executable,
        args,
        stdin,
        stdout,
        stderr,
        timeout,
    } = request;
    let child = Command::new(executable)
        .args(args)
        .stdin(stdin.into_stdio())
        .stdout(stdout.into_stdio())
        .stderr(stderr.into_stdio())
        .kill_on_drop(true)
        .spawn()?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|elapsed| io::Error::new(io::ErrorKind::TimedOut, elapsed))??;

    Ok(GhCommandOutput {
        success: output.status.success(),
        stdout: output.stdout,
    })
}

async fn resolve_gh_executable() -> io::Result<PathBuf> {
    resolve_executable_async(resolve_gh_from_environment).await
}

async fn resolve_executable_async(resolver: impl FnOnce() -> Option<PathBuf> + Send + 'static) -> io::Result<PathBuf> {
    tokio::task::spawn_blocking(resolver)
        .await
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "gh was not found on PATH"))
}

fn resolve_gh_from_environment() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let current_dir = std::env::current_dir().ok()?;
    let path_ext = std::env::var_os("PATHEXT");

    resolve_executable(OsStr::new("gh"), &path, path_ext.as_deref(), &current_dir)
}

/// Resolves `program` only from explicit, non-empty PATH entries.
fn resolve_executable(program: &OsStr, path: &OsStr, path_ext: Option<&OsStr>, current_dir: &Path) -> Option<PathBuf> {
    debug_assert!(current_dir.is_absolute(), "the process current directory is absolute");
    let executable_names = executable_names(program, path_ext);

    std::env::split_paths(path)
        // An empty component can make OS command lookup search CWD implicitly. Requiring an
        // actual entry such as `.` keeps repository-local executables opt-in.
        .filter(|directory| !directory.as_os_str().is_empty())
        .map(|directory| {
            if directory.is_absolute() {
                directory
            } else {
                current_dir.join(directory)
            }
        })
        .flat_map(|directory| executable_names.iter().map(move |name| directory.join(name)))
        .find(|candidate| is_executable(candidate))
}

#[cfg(windows)]
fn executable_names(program: &OsStr, path_ext: Option<&OsStr>) -> Vec<OsString> {
    const DEFAULT_PATH_EXT: &str = ".COM;.EXE;.BAT;.CMD";

    if Path::new(program).extension().is_some() {
        return windows_executable_image_extension(Path::new(program).extension().unwrap_or_default())
            .then(|| program.to_owned())
            .into_iter()
            .collect();
    }

    let path_ext = path_ext
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsStr::new(DEFAULT_PATH_EXT));
    std::env::split_paths(path_ext)
        .filter(|extension| !extension.as_os_str().is_empty())
        .filter(|extension| windows_executable_image_extension(extension.as_os_str()))
        .map(|extension| {
            let extension = extension.as_os_str();
            let mut executable = program.to_owned();
            if !extension.to_string_lossy().starts_with('.') {
                executable.push(".");
            }
            executable.push(extension);
            executable
        })
        .collect()
}

#[cfg(windows)]
fn windows_executable_image_extension(extension: &OsStr) -> bool {
    let extension = extension.to_string_lossy();
    let extension = extension.strip_prefix('.').unwrap_or_else(|| extension.as_ref());
    extension.eq_ignore_ascii_case("com") || extension.eq_ignore_ascii_case("exe")
}

#[cfg(not(windows))]
fn executable_names(program: &OsStr, _path_ext: Option<&OsStr>) -> Vec<OsString> {
    vec![program.to_owned()]
}

#[cfg(unix)]
fn is_executable(candidate: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    candidate
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(candidate: &Path) -> bool {
    candidate.is_file()
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::time::Instant;

    use super::*;

    fn token(value: &str) -> GitHubToken {
        value.parse().expect("GitHubToken parsing is infallible")
    }

    fn successful(stdout: &[u8]) -> GhCommandOutput {
        GhCommandOutput {
            success: true,
            stdout: stdout.to_vec(),
        }
    }

    #[tokio::test]
    async fn explicit_token_precedes_environment_and_gh() {
        let environment_read = Cell::new(false);
        let gh_called = Cell::new(false);
        let explicit = token("explicit-secret");

        let selected = discover_with(
            Some(&explicit),
            true,
            &Endpoints::default(),
            || {
                environment_read.set(true);
                Some(OsString::from("environment-secret"))
            },
            |_| {
                gh_called.set(true);
                std::future::ready(Ok(successful(b"gh-secret")))
            },
        )
        .await
        .expect("the explicit token is selected");

        assert_eq!(selected.expose_secret(), "explicit-secret");
        assert!(!environment_read.get(), "an explicit token suppresses environment lookup");
        assert!(!gh_called.get(), "an explicit token suppresses gh");
    }

    #[tokio::test]
    async fn environment_token_precedes_gh() {
        let gh_called = Cell::new(false);

        let selected = discover_with(
            None,
            true,
            &Endpoints::default(),
            || Some(OsString::from("environment-secret")),
            |_| {
                gh_called.set(true);
                std::future::ready(Ok(successful(b"gh-secret")))
            },
        )
        .await
        .expect("the environment token is selected");

        assert_eq!(selected.expose_secret(), "environment-secret");
        assert!(!gh_called.get(), "an environment token suppresses gh");
    }

    #[tokio::test]
    async fn default_off_never_queries_gh_for_absent_or_blank_environment_tokens() {
        for environment in [None, Some(""), Some(" \r\n\t ")] {
            let gh_called = Cell::new(false);

            let selected = discover_with(
                None,
                false,
                &Endpoints::default(),
                || environment.map(OsString::from),
                |_| {
                    gh_called.set(true);
                    std::future::ready(Ok(successful(b"gh-secret")))
                },
            )
            .await;

            assert!(selected.is_none());
            assert!(!gh_called.get(), "gh must remain disabled for environment value {environment:?}");
        }
    }

    #[tokio::test]
    async fn blank_environment_tokens_continue_to_gh_when_enabled() {
        for environment in ["", " \r\n\t "] {
            let gh_called = Cell::new(false);

            let selected = discover_with(
                None,
                true,
                &Endpoints::default(),
                || Some(OsString::from(environment)),
                |_| {
                    gh_called.set(true);
                    std::future::ready(Ok(successful(b"gh-secret")))
                },
            )
            .await
            .expect("a blank environment token falls through to gh");

            assert_eq!(selected.expose_secret(), "gh-secret");
            assert!(gh_called.get(), "gh is queried for a blank environment token");
        }
    }

    #[tokio::test]
    async fn gh_uses_the_enterprise_hostname_and_trims_stdout() {
        let requested_hostname = RefCell::new(None);
        let endpoints = Endpoints::default().with_github_url("https://github.example.test/api/v3");

        let selected = discover_with(
            None,
            true,
            &endpoints,
            || None,
            |hostname| {
                requested_hostname.replace(Some(hostname));
                std::future::ready(Ok(successful(b"  gh-secret\r\n")))
            },
        )
        .await
        .expect("gh returned a token");

        assert_eq!(requested_hostname.borrow().as_deref(), Some("github.example.test"));
        assert_eq!(selected.expose_secret(), "gh-secret");
    }

    #[tokio::test]
    async fn public_api_uses_the_github_com_login() {
        let requested_hostname = RefCell::new(None);

        let selected = discover_with(
            None,
            true,
            &Endpoints::default(),
            || None,
            |hostname| {
                requested_hostname.replace(Some(hostname));
                std::future::ready(Ok(successful(b"gh-secret")))
            },
        )
        .await;

        assert!(selected.is_some());
        assert_eq!(requested_hostname.borrow().as_deref(), Some("github.com"));
    }

    #[tokio::test]
    async fn command_not_found_continues_anonymously() {
        let selected = discover_with(
            None,
            true,
            &Endpoints::default(),
            || None,
            |_| std::future::ready(Err(io::Error::new(io::ErrorKind::NotFound, "test gh is absent"))),
        )
        .await;

        assert!(selected.is_none());
    }

    #[tokio::test]
    async fn timed_out_command_continues_anonymously() {
        let selected = discover_with(
            None,
            true,
            &Endpoints::default(),
            || None,
            |_| std::future::ready(Err(io::Error::new(io::ErrorKind::TimedOut, "test gh lookup expired"))),
        )
        .await;

        assert!(selected.is_none());
    }

    #[tokio::test]
    async fn unsuccessful_command_continues_anonymously_without_exposing_stdout() {
        let secret = "failed-command-secret";
        let selected = discover_with(
            None,
            true,
            &Endpoints::default(),
            || None,
            |_| {
                std::future::ready(Ok(GhCommandOutput {
                    success: false,
                    stdout: secret.as_bytes().to_vec(),
                }))
            },
        )
        .await;

        assert!(selected.is_none());
    }

    #[tokio::test]
    async fn blank_command_output_continues_anonymously() {
        let selected = discover_with(
            None,
            true,
            &Endpoints::default(),
            || None,
            |_| std::future::ready(Ok(successful(b" \r\n\t "))),
        )
        .await;

        assert!(selected.is_none());
    }

    #[tokio::test]
    async fn non_utf8_command_output_continues_anonymously() {
        let selected = discover_with(
            None,
            true,
            &Endpoints::default(),
            || None,
            |_| std::future::ready(Ok(successful(&[0xff, 0xfe]))),
        )
        .await;

        assert!(selected.is_none());
    }

    #[tokio::test]
    async fn query_gh_builds_the_production_process_request() {
        let root = tempfile::tempdir().expect("creating a command fixture");
        let executable = root.path().join(test_gh_name());
        let observed = RefCell::new(None);

        let output = query_gh_with(
            "github.example.test".to_owned(),
            || std::future::ready(Ok(executable.clone())),
            |request| {
                observed.replace(Some(request));
                std::future::ready(Ok(successful(b"fixture-token\n")))
            },
        )
        .await
        .expect("the injected command runner succeeds");

        assert!(output.success);
        assert_eq!(output.stdout, b"fixture-token\n");
        assert_eq!(
            observed.into_inner(),
            Some(GhCommandRequest {
                executable,
                args: ["auth", "token", "--hostname", "github.example.test"]
                    .into_iter()
                    .map(OsString::from)
                    .collect(),
                stdin: GhStdio::Null,
                stdout: GhStdio::Capture,
                stderr: GhStdio::Null,
                timeout: GH_COMMAND_TIMEOUT,
            })
        );
    }

    #[tokio::test]
    async fn command_timeout_is_reported_and_does_not_wait_for_the_child() {
        let executable = std::env::current_exe().expect("the test harness executable has an absolute path");
        let module = module_path!().split_once("::").map_or(module_path!(), |(_, module)| module);
        let fixture = format!("{module}::command_timeout_child_fixture");
        let started = Instant::now();

        let Err(error) = run_gh_command(GhCommandRequest {
            executable,
            args: ["--ignored", "--exact"]
                .into_iter()
                .map(OsString::from)
                .chain(std::iter::once(OsString::from(fixture)))
                .collect(),
            stdin: GhStdio::Null,
            stdout: GhStdio::Capture,
            stderr: GhStdio::Null,
            timeout: Duration::from_millis(50),
        })
        .await
        else {
            panic!("the child exceeds the test timeout");
        };

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the timed-out child was awaited instead of terminated"
        );
    }

    #[test]
    #[ignore = "subprocess fixture for command_timeout_is_reported_and_does_not_wait_for_the_child"]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn command_timeout_child_fixture() {
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn path_resolution_prefers_path_over_a_planted_cwd_executable() {
        let root = tempfile::tempdir().expect("creating a resolver fixture");
        let current_dir = root.path().join("project");
        let path_dir = root.path().join("bin");
        fs::create_dir_all(&current_dir).expect("creating the project directory");
        fs::create_dir_all(&path_dir).expect("creating the PATH directory");
        write_test_executable(&current_dir.join(test_gh_name()));
        let expected = path_dir.join(test_gh_name());
        write_test_executable(&expected);
        let path = std::env::join_paths([&path_dir]).expect("the fixture PATH is valid");

        let resolved = resolve_test_executable(&path, &current_dir);

        assert_eq!(resolved.as_deref(), Some(expected.as_path()));
    }

    #[test]
    fn path_resolution_does_not_search_cwd_implicitly() {
        let root = tempfile::tempdir().expect("creating a resolver fixture");
        let current_dir = root.path().join("project");
        fs::create_dir_all(&current_dir).expect("creating the project directory");
        write_test_executable(&current_dir.join(test_gh_name()));

        let resolved = resolve_test_executable(OsStr::new(""), &current_dir);

        assert!(resolved.is_none());
    }

    #[test]
    fn path_resolution_honors_an_explicit_cwd_entry() {
        let root = tempfile::tempdir().expect("creating a resolver fixture");
        let current_dir = root.path().join("project");
        fs::create_dir_all(&current_dir).expect("creating the project directory");
        let expected = current_dir.join(test_gh_name());
        write_test_executable(&expected);

        let resolved = resolve_test_executable(OsStr::new("."), &current_dir);

        assert_eq!(resolved.as_deref(), Some(current_dir.join(".").join(test_gh_name()).as_path()));
    }

    #[cfg(windows)]
    #[test]
    fn path_resolution_uses_pathext_order() {
        let root = tempfile::tempdir().expect("creating a resolver fixture");
        let current_dir = root.path().join("project");
        let path_dir = root.path().join("bin");
        fs::create_dir_all(&current_dir).expect("creating the project directory");
        fs::create_dir_all(&path_dir).expect("creating the PATH directory");
        write_test_executable(&path_dir.join("gh.EXE"));
        let expected = path_dir.join("gh.COM");
        write_test_executable(&expected);
        let path = std::env::join_paths([&path_dir]).expect("the fixture PATH is valid");

        let resolved = resolve_executable(OsStr::new("gh"), &path, Some(OsStr::new(".CMD;.COM;.EXE")), &current_dir);

        assert_eq!(resolved.as_deref(), Some(expected.as_path()));
    }

    #[cfg(windows)]
    #[test]
    fn path_resolution_rejects_batch_scripts() {
        let root = tempfile::tempdir().expect("creating a resolver fixture");
        let current_dir = root.path().join("project");
        let path_dir = root.path().join("bin");
        fs::create_dir_all(&current_dir).expect("creating the project directory");
        fs::create_dir_all(&path_dir).expect("creating the PATH directory");
        write_test_executable(&path_dir.join("gh.CMD"));
        write_test_executable(&path_dir.join("gh.BAT"));
        let path = std::env::join_paths([&path_dir]).expect("the fixture PATH is valid");

        let resolved = resolve_executable(OsStr::new("gh"), &path, Some(OsStr::new(".CMD;.BAT")), &current_dir);

        assert!(resolved.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn executable_resolution_does_not_block_the_async_worker() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let resolution = tokio::spawn(resolve_executable_async(move || {
            let _ = started_tx.send(Instant::now());
            std::thread::sleep(Duration::from_millis(400));
            None
        }));

        let started_at = started_rx.await.expect("the blocking resolver starts");
        assert!(
            started_at.elapsed() < Duration::from_millis(200),
            "the async worker was blocked by executable resolution"
        );

        let error = resolution
            .await
            .expect("the resolution task does not panic")
            .expect_err("the fixture resolver returns no executable");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn token_debug_output_is_redacted() {
        let secret = "diagnostic-secret";
        let diagnostic = format!("{:?}", token(secret));

        assert!(!diagnostic.contains(secret));
        assert_eq!(diagnostic, "GitHubToken([REDACTED])");
    }

    #[cfg(windows)]
    fn test_gh_name() -> &'static str {
        "gh.EXE"
    }

    #[cfg(not(windows))]
    fn test_gh_name() -> &'static str {
        "gh"
    }

    fn resolve_test_executable(path: &OsStr, current_dir: &Path) -> Option<PathBuf> {
        #[cfg(windows)]
        let path_ext = Some(OsStr::new(".EXE"));
        #[cfg(not(windows))]
        let path_ext = None;

        resolve_executable(OsStr::new("gh"), path, path_ext, current_dir)
    }

    fn write_test_executable(path: &Path) {
        fs::write(path, b"resolver fixture").expect("writing the resolver fixture");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("making the resolver fixture executable");
        }
    }
}
