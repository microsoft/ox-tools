// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Interruptible reads from child stdout and stderr pipes.
//!
//! A blocking `Read` owned by a capture thread cannot be interrupted by
//! dropping its `JoinHandle`. This wrapper exposes the platform's nonblocking
//! pipe observation behind an ordinary safe [`Read`] implementation: no data
//! currently available is reported as [`io::ErrorKind::WouldBlock`].

use std::io::{self, Read};
use std::process::{ChildStderr, ChildStdout};

/// A child pipe whose reads report [`io::ErrorKind::WouldBlock`] instead of
/// waiting indefinitely for another process to write or close the pipe.
#[derive(Debug)]
pub struct InterruptiblePipe<R> {
    inner: R,
}

impl InterruptiblePipe<ChildStdout> {
    /// Takes ownership of a child's stdout pipe and makes its reads interruptible.
    ///
    /// On Unix, this sets `O_NONBLOCK` on the pipe's shared open-file
    /// description. Callers must not retain duplicated descriptors whose
    /// blocking mode they expect to remain unchanged. On Windows,
    /// [`ChildStdout`] supplies the readable anonymous pipe required by
    /// `PeekNamedPipe`.
    ///
    /// # Errors
    ///
    /// Returns the operating system's error when pipe readiness cannot be
    /// configured.
    pub fn stdout(inner: ChildStdout) -> io::Result<Self> {
        interruptible(inner)
    }
}

impl InterruptiblePipe<ChildStderr> {
    /// Takes ownership of a child's stderr pipe and makes its reads interruptible.
    ///
    /// The platform preconditions are the same as for child stdout.
    ///
    /// # Errors
    ///
    /// Returns the operating system's error when pipe readiness cannot be
    /// configured.
    pub fn stderr(inner: ChildStderr) -> io::Result<Self> {
        interruptible(inner)
    }
}

#[cfg(unix)]
fn interruptible<R>(inner: R) -> io::Result<InterruptiblePipe<R>>
where
    R: std::os::fd::AsFd,
{
    use std::os::fd::AsRawFd as _;

    let descriptor = inner.as_fd().as_raw_fd();

    // SAFETY: `AsFd` proves the descriptor remains live for this borrow.
    // `F_GETFL` reads its flags and does not access caller memory.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK == 0 {
        // SAFETY: `AsFd` keeps the same descriptor live through this call.
        // Preserving every existing flag avoids changing any property other
        // than nonblocking I/O.
        if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(InterruptiblePipe { inner })
}

#[cfg(unix)]
impl<R> Read for InterruptiblePipe<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::InterruptiblePipe;

    #[test]
    #[cfg_attr(miri, ignore = "spawns a child process; Miri isolation does not support process creation")]
    fn child_pipe_reads_are_pending_data_then_eof() {
        let mut command = pipe_writer();
        let _ = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = command.spawn().expect("spawn pipe writer");
        let stdout = child.stdout.take().expect("capture child stdout");
        let mut pipe = InterruptiblePipe::stdout(stdout).expect("make child pipe interruptible");
        let mut buffer = [0_u8; 64];

        let pending = pipe.read(&mut buffer).expect_err("the blocked writer has not produced output");
        assert_eq!(pending.kind(), std::io::ErrorKind::WouldBlock);

        child
            .stdin
            .take()
            .expect("capture child stdin")
            .write_all(b"go\n")
            .expect("release child writer");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => output.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("failed to read child pipe: {error}"),
            }
        }

        assert!(child.wait().expect("wait for pipe writer").success());
        assert_eq!(output, b"payload");
    }

    #[cfg(unix)]
    fn pipe_writer() -> Command {
        let mut command = Command::new("sh");
        let _ = command.args(["-c", "read gate; printf payload"]);
        command
    }

    #[cfg(windows)]
    fn pipe_writer() -> Command {
        let mut command = Command::new("pwsh");
        let _ = command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$null = [Console]::In.ReadLine(); [Console]::Out.Write('payload')",
        ]);
        command
    }
}

#[cfg(windows)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "Unix construction configures descriptor flags and can fail; the cross-platform constructor keeps one signature"
)]
fn interruptible<R>(inner: R) -> io::Result<InterruptiblePipe<R>> {
    Ok(InterruptiblePipe { inner })
}

#[cfg(windows)]
impl<R> Read for InterruptiblePipe<R>
where
    R: Read + std::os::windows::io::AsHandle,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use std::os::windows::io::AsRawHandle as _;

        use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED, HANDLE};
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;

        if buf.is_empty() {
            return Ok(0);
        }

        let mut available = 0_u32;
        // SAFETY: the handle is borrowed from the live pipe for the duration
        // of this call. Null buffer arguments request only the available-byte
        // count, which is written to the valid `available` pointer.
        let ready = unsafe {
            PeekNamedPipe(
                self.inner.as_handle().as_raw_handle().cast::<core::ffi::c_void>() as HANDLE,
                core::ptr::null_mut(),
                0,
                core::ptr::null_mut(),
                core::ptr::from_mut(&mut available),
                core::ptr::null_mut(),
            )
        };
        if ready == 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error().and_then(|code| u32::try_from(code).ok()) {
                Some(ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED) => Ok(0),
                _ => Err(error),
            };
        }
        if available == 0 {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }

        let available = usize::try_from(available).unwrap_or(usize::MAX);
        let readable = buf.len().min(available);
        self.inner.read(&mut buf[..readable])
    }
}

#[cfg(not(any(unix, windows)))]
fn interruptible<R>(inner: R) -> io::Result<InterruptiblePipe<R>> {
    // No bounded child-pipe readiness primitive is available.
    drop(inner);
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "interruptible child-pipe reads require Unix nonblocking descriptors or Windows pipe readiness",
    ))
}

#[cfg(not(any(unix, windows)))]
impl<R> Read for InterruptiblePipe<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}
