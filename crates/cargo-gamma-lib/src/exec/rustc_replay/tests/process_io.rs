// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{Cursor, ErrorKind, Read, Write};
use std::ops::{Deref, DerefMut};
use std::process::{Child as NativeChild, Command, ExitStatus};
use std::sync::{Arc, Mutex};

pub(in crate::exec::rustc_replay) mod io {
    pub(crate) use std::io::*;

    pub(in crate::exec::rustc_replay) fn stdout() -> super::Relay {
        super::Relay::new(false)
    }

    pub(in crate::exec::rustc_replay) fn stderr() -> super::Relay {
        super::Relay::new(true)
    }
}

#[derive(Default)]
pub(in crate::exec::rustc_replay) struct Script {
    pub(super) statuses: VecDeque<Result<Option<i32>, ErrorKind>>,
    pub(super) children: VecDeque<Result<Output, ErrorKind>>,
    pub(super) calls: Vec<&'static str>,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    pub(super) write_failure: Option<ErrorKind>,
}

pub(super) struct Output {
    pub(super) code: Result<Option<i32>, ErrorKind>,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    pub(super) read_failure: Option<ErrorKind>,
}

pub(in crate::exec::rustc_replay) type State = Arc<Mutex<Script>>;

thread_local! {
    static SCRIPT: RefCell<Option<State>> = const { RefCell::new(None) };
}

pub(in crate::exec::rustc_replay) struct Scope(Option<State>);

impl Drop for Scope {
    fn drop(&mut self) {
        SCRIPT.with_borrow_mut(|script| *script = self.0.take());
    }
}

pub(in crate::exec::rustc_replay) fn enter(script: Option<State>) -> Scope {
    Scope(SCRIPT.with_borrow_mut(|slot| core::mem::replace(slot, script)))
}

pub(in crate::exec::rustc_replay) fn current() -> Option<State> {
    SCRIPT.with_borrow(Clone::clone)
}

pub(super) fn with<T>(script: Script, run: impl FnOnce(&State) -> T) -> T {
    let script = Arc::new(Mutex::new(script));
    let _scope = enter(Some(Arc::clone(&script)));
    run(&script)
}

pub(in crate::exec::rustc_replay) struct CommandRef<'a>(pub(in crate::exec::rustc_replay) &'a mut Command);

impl Deref for CommandRef<'_> {
    type Target = Command;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl DerefMut for CommandRef<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
    }
}

impl CommandRef<'_> {
    pub(in crate::exec::rustc_replay) fn status(&mut self) -> std::io::Result<ExitStatus> {
        if let Some(script) = current() {
            let mut script = script.lock().unwrap();
            script.calls.push("status");
            let status = script.statuses.pop_front().expect("the test must script every compiler invocation");
            return status.map(exit_status).map_err(std::io::Error::from);
        }
        self.0.status()
    }

    pub(in crate::exec::rustc_replay) fn spawn(&mut self) -> std::io::Result<Child> {
        if let Some(script) = current() {
            let mut script = script.lock().unwrap();
            script.calls.push("spawn");
            let output = script
                .children
                .pop_front()
                .expect("the test must script every compiler invocation")?;
            return Ok(Child {
                stdout: Some(Box::new(Reader(Cursor::new(output.stdout), output.read_failure))),
                stderr: Some(Box::new(Cursor::new(output.stderr))),
                source: Source::Scripted(output.code),
            });
        }
        let mut child = self.0.spawn()?;
        Ok(Child {
            stdout: child.stdout.take().map(|stream| Box::new(stream) as Box<dyn Read + Send>),
            stderr: child.stderr.take().map(|stream| Box::new(stream) as Box<dyn Read + Send>),
            source: Source::Native(child),
        })
    }
}

fn exit_status(code: Option<i32>) -> ExitStatus {
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt as _;
        ExitStatus::from_raw(code.unwrap_or(1).cast_unsigned())
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        ExitStatus::from_raw(code.map_or(9, |code| code << 8))
    }
}

pub(in crate::exec::rustc_replay) struct Child {
    pub(in crate::exec::rustc_replay) stdout: Option<Box<dyn Read + Send>>,
    pub(in crate::exec::rustc_replay) stderr: Option<Box<dyn Read + Send>>,
    source: Source,
}

enum Source {
    Native(NativeChild),
    Scripted(Result<Option<i32>, ErrorKind>),
}

impl Child {
    pub(in crate::exec::rustc_replay) fn kill(&mut self) -> std::io::Result<()> {
        match &mut self.source {
            Source::Native(child) => child.kill(),
            Source::Scripted(_) => {
                current().unwrap().lock().unwrap().calls.push("kill");
                Ok(())
            }
        }
    }

    pub(in crate::exec::rustc_replay) fn wait(&mut self) -> std::io::Result<ExitStatus> {
        match &mut self.source {
            Source::Native(child) => child.wait(),
            Source::Scripted(code) => {
                current().unwrap().lock().unwrap().calls.push("wait");
                code.map(exit_status).map_err(std::io::Error::from)
            }
        }
    }
}

struct Reader(Cursor<Vec<u8>>, Option<ErrorKind>);

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(error) = self.1.take() {
            return Err(error.into());
        }
        self.0.read(buf)
    }
}

pub(in crate::exec::rustc_replay) struct Relay {
    script: Option<State>,
    stderr: bool,
}

impl Relay {
    fn new(stderr: bool) -> Self {
        Self { script: current(), stderr }
    }
}

impl Write for Relay {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(script) = &self.script {
            let mut script = script.lock().unwrap();
            if let Some(error) = script.write_failure {
                return Err(error.into());
            }
            if self.stderr {
                script.stderr.extend_from_slice(buf);
            } else {
                script.stdout.extend_from_slice(buf);
            }
            return Ok(buf.len());
        }
        if self.stderr {
            std::io::stderr().write(buf)
        } else {
            std::io::stdout().write(buf)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
